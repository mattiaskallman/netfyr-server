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
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
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
const DUMMY_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2FsdA$\
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
    session_hours: i64,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<String> {
    let token = generate_token();
    let now = now_ms();
    let expires = now + session_hours.max(1) * 3_600_000;
    conn.execute(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at, ip, user_agent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![hash_token(&token), user_id, now, expires, ip, user_agent],
    )?;
    Ok(token)
}

/// Slå upp en session till sin användare. None vid ogiltig, utgången
/// eller avstängd. Utgångna sessioner raderas i förbifarten — annars
/// växer tabellen mellan städningarna.
fn resolve_session(conn: &rusqlite::Connection, token: &str) -> Result<Option<AuthUser>> {
    let now = now_ms();
    let hash = hash_token(token);

    let row: Option<(i64, i64, String, String, i64)> = conn
        .query_row(
            "SELECT s.user_id, s.expires_at, u.username, u.role, u.disabled
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1",
            [&hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;

    let Some((user_id, expires_at, username, role, disabled)) = row else {
        return Ok(None);
    };

    if expires_at <= now {
        conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [&hash])?;
        return Ok(None);
    }
    if disabled != 0 {
        return Ok(None);
    }

    Ok(Some(AuthUser {
        id: user_id,
        username,
        role: Role::parse(&role).unwrap_or(Role::User),
    }))
}

pub fn destroy_session(conn: &rusqlite::Connection, token: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sessions WHERE token_hash = ?1",
        [hash_token(token)],
    )?;
    Ok(())
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

/// Bygg Set-Cookie-värdet för en ny session.
pub fn session_cookie(token: &str, session_hours: i64, secure: bool) -> String {
    let mut cookie = format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        session_hours.max(1) * 3600
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
    let user = match token {
        Some(t) => state
            .db
            .call(move |conn| resolve_session(conn, &t))
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
    let user = match token {
        Some(t) => state
            .db
            .call(move |conn| resolve_session(conn, &t))
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
