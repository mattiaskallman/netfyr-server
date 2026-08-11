// =====================================================================
// api/auth.rs
// Inloggning, utloggning, egen kontoinformation och lösenordsbyte.
//
// Login är den enda öppna dörren i API:t (bredvid hälsokontrollen) och
// är därför byggd att stå emot tryck: samma felmeddelande vare sig
// kontot saknas eller lösenordet är fel, utelåsning efter fem försök,
// och en dummy-verifiering som jämnar ut svarstiderna.
// =====================================================================

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{now_ms, ApiError, ApiResult};
use crate::auth::{self, AuthUser, Role, SessionPolicy};
use crate::routes::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeView {
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
    pub admin_idle_minutes: i64,
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

/// Orsaken till ett avvisat inloggningsförsök. En typ, inte en sträng —
/// meddelandet sätts först i ytterkanten, på aktivt språk. Tidigare
/// jämfördes svenska strängar internt, och då blir varje omformulering
/// ett potentiellt beteendehaveri.
enum LoginReject {
    Locked,
    Disabled,
    BadCreds,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> ApiResult<Response> {
    let username = body.username.trim().to_lowercase();
    let ip = addr.ip().to_string();
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(200).collect::<String>());

    if username.is_empty() || body.password.is_empty() {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::bad_request(
            crate::i18n::username_password_required(lang),
        ));
    }

    let password = body.password.clone();
    let ip_for_audit = ip.clone();
    let ua_for_session = user_agent.clone();
    let session_hours = state.session_hours;
    let operator_session_days = state.operator_session_days;

    // Hela kontrollen körs inne på databastråden: uppslaget, den
    // avsiktligt dyra hash-verifieringen och räknarna är en sekvens.
    let outcome = state
        .db
        .call(move |conn| {
            let row: Option<(i64, String, String, i64, i64, Option<i64>)> = conn
                .query_row(
                    "SELECT id, password_hash, role, disabled, failed_attempts, locked_until
                     FROM users WHERE username = ?1",
                    [&username],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .optional()?;

            let Some((user_id, hash, role, disabled, failed, locked_until)) = row else {
                // Okänt konto: verifiera mot en dummy så svarstiden inte
                // skiljer sig från ett känt konto med fel lösenord.
                auth::verify_dummy(&password);
                return Ok(Err(LoginReject::BadCreds));
            };

            let now = now_ms();
            if let Some(until) = locked_until {
                if until > now {
                    // Låst konto avvisas före verifieringen — det är
                    // hela poängen med låset. Att kontot är låst råder
                    // det ingen tvekan om för den som orsakade låset.
                    return Ok(Err(LoginReject::Locked));
                }
            }

            if !auth::verify_password(&password, &hash) {
                let failed = failed + 1;
                if failed >= auth::MAX_FAILED_ATTEMPTS {
                    conn.execute(
                        "UPDATE users SET failed_attempts = 0, locked_until = ?2 WHERE id = ?1",
                        params![user_id, now + auth::LOCKOUT_MS],
                    )?;
                } else {
                    conn.execute(
                        "UPDATE users SET failed_attempts = ?2 WHERE id = ?1",
                        params![user_id, failed],
                    )?;
                }
                return Ok(Err(LoginReject::BadCreds));
            }

            // Hit kommer bara den som bevisat lösenordet — då går det
            // bra att säga att kontot är avstängt.
            if disabled != 0 {
                return Ok(Err(LoginReject::Disabled));
            }

            // Lyckat: nollställ räknarna och stansa sessionen.
            conn.execute(
                "UPDATE users SET failed_attempts = 0, locked_until = NULL, last_login_at = ?2
                 WHERE id = ?1",
                params![user_id, now],
            )?;
            let role = Role::parse(&role).unwrap_or(Role::User);
            let token = auth::create_session(
                conn,
                user_id,
                role,
                SessionPolicy {
                    admin_hours: session_hours,
                    operator_days: operator_session_days,
                },
                Some(&ip_for_audit),
                ua_for_session.as_deref(),
            )?;
            Ok(Ok((user_id, role, token)))
        })
        .await?;

    match outcome {
        Ok((user_id, role, token)) => {
            audit_login(
                &state,
                &body.username.trim().to_lowercase(),
                "login_ok",
                &ip,
            )
            .await;
            let me = me_view(&state, user_id).await?;
            let cookie = auth::session_cookie(
                &token,
                auth::cookie_max_age_seconds(
                    role,
                    state.session_hours,
                    state.operator_session_days,
                ),
                state.secure_cookies,
            );
            Ok(([(header::SET_COOKIE, cookie)], Json(me)).into_response())
        }
        Err(reject) => {
            audit_login(
                &state,
                &body.username.trim().to_lowercase(),
                "login_fail",
                &ip,
            )
            .await;
            let lang = crate::i18n::load_db(&state.db).await;
            match reject {
                LoginReject::Locked => Err(ApiError::too_many(crate::i18n::login_locked(lang))),
                LoginReject::Disabled => {
                    Err(ApiError::forbidden(crate::i18n::account_disabled(lang)))
                }
                // Samma meddelande oavsett orsak — annars går det att
                // sondera vilka användarnamn som finns.
                LoginReject::BadCreds => Err(ApiError::unauthorized(crate::i18n::login_fail(lang))),
            }
        }
    }
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            h.split(';').find_map(|p| {
                let p = p.trim();
                p.strip_prefix(auth::COOKIE_NAME)
                    .and_then(|v| v.strip_prefix('='))
                    .map(|s| s.to_string())
            })
        });

    if let Some(t) = token {
        let th = t.clone();
        state
            .db
            .call(move |conn| auth::destroy_session(conn, &th))
            .await?;
    }

    super::audit::record(&state.db, &user.username, "logout", None, None, None).await;

    Ok((
        [(header::SET_COOKIE, auth::clear_cookie(state.secure_cookies))],
        Json(()),
    )
        .into_response())
}

/// Registrera mänsklig aktivitet. Den automatiska statuspollningen anropar
/// aldrig denna endpoint, vilket gör admin-gränsen till riktig inaktivitet.
pub async fn activity(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            h.split(';').find_map(|p| {
                let p = p.trim();
                p.strip_prefix(auth::COOKIE_NAME)
                    .and_then(|v| v.strip_prefix('='))
                    .map(str::to_string)
            })
        })
        .ok_or_else(|| ApiError::unauthorized("session saknas"))?;

    let role = user.role;
    let operator_session_days = state.operator_session_days;
    let token_for_db = token.clone();
    let touched = state
        .db
        .call(move |conn| auth::touch_session(conn, &token_for_db, role, operator_session_days))
        .await?;
    if !touched {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::unauthorized(crate::i18n::not_logged_in(lang)));
    }

    if role == Role::User {
        let cookie = auth::session_cookie(
            &token,
            auth::cookie_max_age_seconds(role, state.session_hours, operator_session_days),
            state.secure_cookies,
        );
        Ok(([(header::SET_COOKIE, cookie)], Json(())).into_response())
    } else {
        Ok(Json(()).into_response())
    }
}

pub async fn me(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> ApiResult<Json<MeView>> {
    me_view(&state, user.id).await.map(Json)
}

async fn me_view(state: &Arc<AppState>, user_id: i64) -> ApiResult<MeView> {
    let admin_idle_minutes = state.admin_idle_minutes;
    let view = state
        .db
        .call(move |conn| {
            let (username, role, must_change): (String, String, i64) = conn.query_row(
                "SELECT username, role, must_change_password FROM users WHERE id = ?1",
                [user_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            Ok(MeView {
                username,
                role,
                must_change_password: must_change != 0,
                admin_idle_minutes,
            })
        })
        .await?;
    Ok(view)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordBody {
    pub current: String,
    pub new_password: String,
}

/// Byt sitt eget lösenord.
///
/// Det nuvarande lösenordet krävs — annars skulle en obevakad inloggad
/// session räcka för att kapa kontot. Vid bytet dödas alla andra
/// sessioner: den som byter lösenord gör det ofta för att någon annan
/// kan ha kommit åt det gamla.
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<Json<()>> {
    if let Err(msg) =
        auth::validate_password(&body.new_password, crate::i18n::load_db(&state.db).await)
    {
        return Err(ApiError::bad_request(msg));
    }

    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            h.split(';').find_map(|p| {
                let p = p.trim();
                p.strip_prefix(auth::COOKIE_NAME)
                    .and_then(|v| v.strip_prefix('='))
                    .map(|s| s.to_string())
            })
        });

    let current = body.current.clone();
    let new = body.new_password.clone();
    let uid = user.id;
    let uname = user.username.clone();
    let ip = addr.ip().to_string();

    state
        .db
        .call(move |conn| {
            let hash: String = conn.query_row(
                "SELECT password_hash FROM users WHERE id = ?1",
                [uid],
                |r| r.get(0),
            )?;
            if !auth::verify_password(&current, &hash) {
                anyhow::bail!(crate::i18n::current_password_wrong(crate::i18n::load(conn)));
            }
            let new_hash = auth::hash_password(&new)?;
            conn.execute(
                "UPDATE users SET password_hash = ?2, must_change_password = 0,
                        failed_attempts = 0, locked_until = NULL, updated_at = ?3
                 WHERE id = ?1",
                params![uid, new_hash, now_ms()],
            )?;
            auth::destroy_user_sessions(conn, uid, token.as_deref())?;
            Ok(())
        })
        .await
        .map_err(|e| ApiError::bad_request(format!("{e}")))?;

    super::audit::record(&state.db, &uname, "password_change", None, None, Some(&ip)).await;

    Ok(Json(()))
}

/// Logga inloggningsförsök. Sker utanför den vanliga audit-hjälpen
/// eftersom det inte finns någon inloggad användare än — användarnamnet
/// är det som angavs i formuläret, och kan alltså vara påhittat.
async fn audit_login(state: &Arc<AppState>, username: &str, action: &str, ip: &str) {
    super::audit::record(&state.db, username, action, None, None, Some(ip)).await;
}
