// =====================================================================
// auth.rs
// Autentisering och behörighet — etapp 5.
//
// MODELLEN I KORTHET
//
//   * Lösenord hashas med Argon2id och lämnar aldrig servern.
//   * En inloggning ger en session: 32 slumpade byte i en kaka. I
//     databasen lagras bara SHA-256 av token — en databaskopia ger
//     inga inloggningar.
//   * Kakorna är HttpOnly (JavaScript når dem inte) och SameSite=Strict
//     (CSRF-anrop från främmande sajter skickar dem inte).
//   * Två roller. "user" läser läget och kvitterar/tystar larm. "admin"
//     gör allt annat: enheter, grupper, kanaler, hemligheter, användare.
//     Kontrollen sker i middleware HÄR, aldrig i gränssnittet — ett
//     dolt fält i en webbsida är ingen behörighetskontroll.
//
// UTELÅSNING
//
// Fem felaktiga försök i rad låser kontot i en kvart. Räknaren sitter
// i databasen, inte i minnet, så en omstart inte nollställer skyddet.
// Meddelandet vid fel är medvetet samma vare sig användaren saknas
// eller lösenordet är fel — annars går det att sondera vilka konton
// som finns.
// =====================================================================

use anyhow::Result;
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chacha20poly1305::aead::rand_core::RngCore;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::api::{now_ms, ApiError};
use crate::db::Db;
use crate::routes::AppState;

/// Kakans namn. Kort och eget — den syns i devtools.
pub const COOKIE_NAME: &str = "nf_session";

/// Antal felaktiga inloggningsförsök innan kontot låses.
pub const MAX_FAILED_ATTEMPTS: i64 = 5;

/// Utelåsningens längd i millisekunder (en kvart).
pub const LOCKOUT_MS: i64 = 15 * 60 * 1000;

/// Administratörens absoluta sessionsgräns får aldrig höjas via config.
pub const MAX_ADMIN_SESSION_HOURS: i64 = 12;

pub fn effective_admin_session_hours(configured_hours: i64) -> i64 {
    configured_hours.clamp(1, MAX_ADMIN_SESSION_HOURS)
}

/// Auditloggens livslängd i dygn. Drygt ett år — NIS2-arbete kräver
//  att incidenter går att följa upp långt efteråt.
pub const AUDIT_RETENTION_DAYS: i64 = 395;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionPolicy {
    pub admin_hours: i64,
    pub operator_days: i64,
}

/// Den inloggade användaren, uppslagen från sessionen en gång per
/// anrop och stoppad i requestens extensions av middlewaren.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub role: Role,
}

// ---- Lösenord ---------------------------------------------------------

/// Minsta lösenordslängd. Tolv tecken är en rimlig ribba för ett system
/// med utelåsning — gissning online är ändå strypt, längden skyddar mot
//  att en stulen hash knäcks offline.
pub const MIN_PASSWORD_LEN: usize = 12;

pub fn validate_password(password: &str, lang: crate::i18n::Lang) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(crate::i18n::password_min_len(lang, MIN_PASSWORD_LEN));
    }
    if password.len() > 256 {
        return Err(crate::i18n::password_too_long(lang).to_string());
    }
    Ok(())
}

/// Hasha ett lösenord med Argon2id och standardparametrar.
///
/// Blockerande och avsiktligt dyr (~50 ms) — anropa inuti db.call eller
/// spawn_blocking, aldrig direkt på en arbetartråd.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("lösenordshashning misslyckades: {e}"))
}

/// Verifiera ett lösenord mot en lagrad hash.
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Färdig hash att verifiera mot när användaren inte finns. Utan den
/// skiljer sig svarstiden mellan "användaren saknas" och "lösenordet
/// är fel", och skillnaden avslöjar vilka konton som existerar.
///
/// Strängen är en giltig Argon2id-kodning av ett värdelöst lösenord —
/// den kan aldrig matcha ett riktigt försök.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2FsdA$\
     X4ylOc+6KyXnWy5l06GrKj7UzBvVbbUQKSctwI9gGsA";

pub fn verify_dummy(password: &str) {
    let _ = verify_password(password, DUMMY_HASH);
}

// ---- Token ------------------------------------------------------------

/// 32 slumpade byte, hexkodade. Entropin är 256 bitar — gissning är
/// meningslös, så ingen ytterligare signering behövs.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    to_hex(&bytes)
}

/// SHA-256 av token, hexkodat. Det är detta som lagras i databasen.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    to_hex(&digest)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Engångslösenord för nystartade och återställda konton. Sex tecken
/// per grupp, gruppsatt för att gå att läsa upp i telefon.
pub fn generate_one_time_password() -> String {
    let mut bytes = [0u8; 9];
    OsRng.fill_bytes(&mut bytes);
    let hex = to_hex(&bytes);
    format!("{}-{}-{}", &hex[0..6], &hex[6..12], &hex[12..18])
}

// ---- Sessioner --------------------------------------------------------

/// Skapa en session och returnera token (inte hashen) för kakan.
pub fn create_session(
    conn: &rusqlite::Connection,
    user_id: i64,
    role: Role,
    policy: SessionPolicy,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<String> {
    create_session_at(conn, user_id, role, policy, ip, user_agent, now_ms())
}

fn create_session_at(
    conn: &rusqlite::Connection,
    user_id: i64,
    role: Role,
    policy: SessionPolicy,
    ip: Option<&str>,
    user_agent: Option<&str>,
    now: i64,
) -> Result<String> {
    let token = generate_token();
    let expires = match role {
        Role::Admin => now + effective_admin_session_hours(policy.admin_hours) * 3_600_000,
        Role::User => now + policy.operator_days.max(1) * 86_400_000,
    };
    conn.execute(
        "INSERT INTO sessions (token_hash, user_id, created_at, last_activity, expires_at, ip, user_agent)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
        params![hash_token(&token), user_id, now, expires, ip, user_agent],
    )?;
    Ok(token)
}

/// Slå upp en session till sin användare. None vid ogiltig, utgången,
/// inaktiv administratör eller avstängd. Ogiltiga sessioner raderas i
/// förbifarten — annars växer tabellen mellan städningarna.
fn resolve_session(
    conn: &rusqlite::Connection,
    token: &str,
    admin_idle_minutes: i64,
    admin_session_hours: i64,
) -> Result<Option<AuthUser>> {
    resolve_session_at(
        conn,
        token,
        now_ms(),
        admin_idle_minutes,
        admin_session_hours,
    )
}

fn resolve_session_at(
    conn: &rusqlite::Connection,
    token: &str,
    now: i64,
    admin_idle_minutes: i64,
    admin_session_hours: i64,
) -> Result<Option<AuthUser>> {
    let hash = hash_token(token);

    let row: Option<(i64, i64, i64, i64, String, String, i64)> = conn
        .query_row(
            "SELECT s.user_id, s.created_at, s.expires_at,
                    COALESCE(s.last_activity, s.created_at),
                    u.username, u.role, u.disabled
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1",
            [&hash],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?;

    let Some((user_id, created_at, expires_at, last_activity, username, role, disabled)) = row
    else {
        return Ok(None);
    };

    let parsed_role = Role::parse(&role).unwrap_or(Role::User);
    let admin_idle_ms = admin_idle_minutes.max(1) * 60_000;
    let admin_absolute_ms = effective_admin_session_hours(admin_session_hours) * 3_600_000;
    if expires_at <= now
        || (parsed_role == Role::Admin
            && (last_activity <= now - admin_idle_ms || created_at <= now - admin_absolute_ms))
    {
        conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [&hash])?;
        return Ok(None);
    }
    if disabled != 0 {
        return Ok(None);
    }

    Ok(Some(AuthUser {
        id: user_id,
        username,
        role: parsed_role,
    }))
}

pub fn destroy_session(conn: &rusqlite::Connection, token: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sessions WHERE token_hash = ?1",
        [hash_token(token)],
    )?;
    Ok(())
}

/// Registrera uttrycklig mänsklig aktivitet. För operatörer förnyas även
/// den rullande sessionsgränsen; administratörens absoluta gräns flyttas
/// aldrig fram.
pub fn touch_session(
    conn: &rusqlite::Connection,
    token: &str,
    role: Role,
    operator_session_days: i64,
) -> Result<bool> {
    touch_session_at(conn, token, role, now_ms(), operator_session_days)
}

fn touch_session_at(
    conn: &rusqlite::Connection,
    token: &str,
    role: Role,
    now: i64,
    operator_session_days: i64,
) -> Result<bool> {
    let hash = hash_token(token);
    let updated = match role {
        Role::Admin => conn.execute(
            "UPDATE sessions SET last_activity = ?2 WHERE token_hash = ?1",
            params![hash, now],
        )?,
        Role::User => {
            let expires = now + operator_session_days.max(1) * 86_400_000;
            conn.execute(
                "UPDATE sessions SET last_activity = ?2, expires_at = ?3 WHERE token_hash = ?1",
                params![hash, now, expires],
            )?
        }
    };
    Ok(updated == 1)
}

/// Ta bort alla sessioner för en användare, utom eventuellt en angiven.
/// Används vid lösenordsbyte och avstängning: gamla inloggningar ska
/// inte leva kvar när kontot ändras.
pub fn destroy_user_sessions(
    conn: &rusqlite::Connection,
    user_id: i64,
    except_token: Option<&str>,
) -> Result<()> {
    match except_token {
        Some(t) => conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token_hash != ?2",
            params![user_id, hash_token(t)],
        )?,
        None => conn.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])?,
    };
    Ok(())
}

// ---- Kakor --------------------------------------------------------------

/// Plocka sessionstoken ur Cookie-huvudet. Handparsat — kakformatet är
//  enkelt och en hel kak-crate för ett enda värde är onödig tyngd.
pub fn token_from_request(req: &Request) -> Option<String> {
    let header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(COOKIE_NAME) {
            let value = value.strip_prefix('=')?;
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

pub fn cookie_max_age_seconds(
    role: Role,
    admin_session_hours: i64,
    operator_session_days: i64,
) -> i64 {
    match role {
        Role::Admin => effective_admin_session_hours(admin_session_hours) * 3600,
        Role::User => operator_session_days.max(1) * 86_400,
    }
}

/// Bygg Set-Cookie-värdet för en ny eller förnyad session.
pub fn session_cookie(token: &str, max_age_seconds: i64, secure: bool) -> String {
    let mut cookie = format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        max_age_seconds.max(1)
    );
    // Secure-flaggan får bara sättas över HTTPS — annars vägrar
    // webbläsaren kakan helt. Därför styrs den av konfigurationen.
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Kakvärde som raderar sessionen hos klienten.
pub fn clear_cookie(secure: bool) -> String {
    let mut cookie = format!("{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

// ---- Middleware ---------------------------------------------------------

/// Kräv inloggning. Slår upp sessionen och lägger AuthUser i
/// requestens extensions — handlarna hämtar den därifrån och behöver
/// aldrig tolka kakor själva.
///
/// VIKTIGT OM FRAMTIDEN: token plockas ur kakan FÖRE await, och
/// uppslaget sker inline — INTE via en hjälpfunktion som tar &Request.
/// En async-hjälpare som håller en referens över sin await-punkt gör
/// middlewarens framtid icke-Send i axums ögon, och då vägrar
/// route_layer att kompilera. Det kostade oss en felsökning.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let token = token_from_request(&req);
    let admin_idle_minutes = state.admin_idle_minutes;
    let admin_session_hours = state.session_hours;
    let user = match token {
        Some(t) => state
            .db
            .call(move |conn| resolve_session(conn, &t, admin_idle_minutes, admin_session_hours))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    match user {
        Some(user) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        None => {
            let lang = crate::i18n::load_db(&state.db).await;
            ApiError::unauthorized(crate::i18n::not_logged_in(lang)).into_response()
        }
    }
}

/// Kräv administratörsroll. Innehåller hela require_auth-kedjan —
/// ett anrop utan session avvisas här direkt.
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let token = token_from_request(&req);
    let admin_idle_minutes = state.admin_idle_minutes;
    let admin_session_hours = state.session_hours;
    let user = match token {
        Some(t) => state
            .db
            .call(move |conn| resolve_session(conn, &t, admin_idle_minutes, admin_session_hours))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    match user {
        Some(user) if user.role == Role::Admin => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        Some(_) => {
            let lang = crate::i18n::load_db(&state.db).await;
            ApiError::forbidden(crate::i18n::requires_admin(lang)).into_response()
        }
        None => {
            let lang = crate::i18n::load_db(&state.db).await;
            ApiError::unauthorized(crate::i18n::not_logged_in(lang)).into_response()
        }
    }
}

// ---- Första start -----------------------------------------------------

/// Skapa administratörskontot vid allra första körningen.
///
/// Returnerar engångslösenordet om ett konto skapades — det skrivs ut i
/// loggen och måste bytas vid första inloggningen. Finns det redan
/// användare gör funktionen ingenting.
pub async fn bootstrap_admin(db: &Db) -> Result<Option<String>> {
    let count: i64 = db
        .call(|conn| {
            let n = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
            Ok(n)
        })
        .await?;

    if count > 0 {
        return Ok(None);
    }

    let password = generate_one_time_password();
    let hash = hash_password(&password)?;
    let now = now_ms();

    db.call(move |conn| {
        conn.execute(
            "INSERT INTO users
                (username, password_hash, role, must_change_password, created_at, updated_at)
             VALUES ('admin', ?1, 'admin', 1, ?2, ?2)",
            params![hash, now],
        )?;
        Ok(())
    })
    .await?;

    Ok(Some(password))
}

/// Städloopen: utgångna sessioner och gammal auditdata.
///
/// Körs som en egen task. Misslyckas en städning loggas det och loopen
/// fortsätter — en trasig städning får aldrig fälla tjänsten.
pub async fn run_janitor(db: Db) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        interval.tick().await;
        let cutoff = now_ms() - AUDIT_RETENTION_DAYS * 86_400_000;
        let res = db
            .call(move |conn| {
                let now = now_ms();
                let s = conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
                let a = conn.execute("DELETE FROM audit_log WHERE ts < ?1", [cutoff])?;
                Ok((s, a))
            })
            .await;
        match res {
            Ok((0, 0)) => {}
            Ok((s, a)) => {
                tracing::debug!("städning: {s} utgångna sessioner, {a} gamla auditposter borttagna")
            }
            Err(e) => tracing::warn!("städningen misslyckades: {e:#}"),
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use rusqlite::Connection;

    fn session_db(
        role: &str,
        created_at: i64,
        last_activity: i64,
        expires_at: i64,
    ) -> (Connection, String) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                username TEXT NOT NULL,
                role TEXT NOT NULL,
                disabled INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE sessions (
                token_hash TEXT PRIMARY KEY,
                user_id INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                last_activity INTEGER,
                expires_at INTEGER NOT NULL,
                ip TEXT,
                user_agent TEXT
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO users (id, username, role) VALUES (1, 'test', ?1)",
            [role],
        )
        .unwrap();
        let token = "test-token".to_string();
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, last_activity, expires_at)
             VALUES (?1, 1, ?2, ?3, ?4)",
            params![hash_token(&token), created_at, last_activity, expires_at],
        )
        .unwrap();
        (conn, token)
    }

    #[test]
    fn admin_session_avvisas_efter_femton_minuters_inaktivitet() {
        let now = 2_000_000;
        let (conn, token) = session_db("admin", 0, now - 15 * 60 * 1000, now + 1_000_000);

        let user = resolve_session_at(&conn, &token, now, 15, 12).unwrap();

        assert!(user.is_none());
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "den inaktiva sessionen ska raderas");
    }

    #[test]
    fn admin_session_gäller_fram_till_inaktivitetsgränsen() {
        let now = 2_000_000;
        let (conn, token) = session_db("admin", 0, now - 15 * 60 * 1000 + 1, now + 1_000_000);

        let user = resolve_session_at(&conn, &token, now, 15, 12)
            .unwrap()
            .unwrap();

        assert_eq!(user.role, Role::Admin);
    }

    #[test]
    fn befordrad_operatör_får_admins_absoluta_tolv_timmar() {
        let now = 50_000_000;
        let (conn, token) = session_db(
            "admin",
            now - 12 * 3_600_000,
            now - 1_000,
            now + 30 * 86_400_000,
        );

        let user = resolve_session_at(&conn, &token, now, 15, 12).unwrap();

        assert!(user.is_none());
    }

    #[test]
    fn aktivitet_på_återkallad_session_avvisas() {
        let (conn, token) = session_db("user", 0, 0, 1);
        conn.execute("DELETE FROM sessions", []).unwrap();

        let touched = touch_session_at(&conn, &token, Role::User, 2_000_000, 30).unwrap();

        assert!(!touched);
    }

    #[test]
    fn operatörens_aktivitet_förnyar_sessionen_trettio_dygn() {
        let now = 2_000_000;
        let old_expiry = now + 1_000;
        let (conn, token) = session_db("user", 0, now - 500, old_expiry);

        touch_session_at(&conn, &token, Role::User, now, 30).unwrap();

        let (last_activity, expires_at): (i64, i64) = conn
            .query_row("SELECT last_activity, expires_at FROM sessions", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(last_activity, now);
        assert_eq!(expires_at, now + 30 * 86_400_000);
    }

    #[test]
    fn felkonfigurerad_adminpolicy_begränsas_till_tolv_timmar() {
        let now = 50_000_000;
        let (conn, _) = session_db("admin", 0, 0, 1);
        conn.execute("DELETE FROM sessions", []).unwrap();
        let policy = SessionPolicy {
            admin_hours: 72,
            operator_days: 30,
        };
        create_session_at(&conn, 1, Role::Admin, policy, None, None, now).unwrap();
        let expires_at: i64 = conn
            .query_row("SELECT expires_at FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(expires_at, now + 12 * 3_600_000);
        assert_eq!(cookie_max_age_seconds(Role::Admin, 72, 30), 12 * 3_600);

        conn.execute(
            "UPDATE sessions SET created_at=?1, last_activity=?2, expires_at=?3",
            params![now - 12 * 3_600_000, now - 1_000, now + 72 * 3_600_000],
        )
        .unwrap();
        let token_hash: String = conn
            .query_row("SELECT token_hash FROM sessions", [], |r| r.get(0))
            .unwrap();
        let token = "replacement-test-token";
        conn.execute(
            "UPDATE sessions SET token_hash=?1 WHERE token_hash=?2",
            params![hash_token(token), token_hash],
        )
        .unwrap();
        assert!(resolve_session_at(&conn, token, now, 15, 72)
            .unwrap()
            .is_none());
    }

    #[test]
    fn nya_sessioner_får_rollstyrd_absolut_livslängd() {
        let now = 2_000_000;
        let (admin_conn, _) = session_db("admin", 0, 0, 1);
        admin_conn.execute("DELETE FROM sessions", []).unwrap();
        let policy = SessionPolicy {
            admin_hours: 12,
            operator_days: 30,
        };
        create_session_at(&admin_conn, 1, Role::Admin, policy, None, None, now).unwrap();
        let admin_expiry: i64 = admin_conn
            .query_row("SELECT expires_at FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(admin_expiry, now + 12 * 3_600_000);

        let (user_conn, _) = session_db("user", 0, 0, 1);
        user_conn.execute("DELETE FROM sessions", []).unwrap();
        create_session_at(&user_conn, 1, Role::User, policy, None, None, now).unwrap();
        let (last_activity, user_expiry): (i64, i64) = user_conn
            .query_row("SELECT last_activity, expires_at FROM sessions", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(last_activity, now);
        assert_eq!(user_expiry, now + 30 * 86_400_000);
    }
}
