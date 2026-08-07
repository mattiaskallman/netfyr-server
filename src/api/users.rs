// =====================================================================
// api/users.rs
// Användarhantering. Bara administratörer når de här endpunkterna —
// middlewaren require_admin står som vakt i routes.rs.
//
// Två skydd kan aldrig slås av:
//   * man kan inte ta bort eller degradera sig själv
//   * den sista administratören kan inte tas bort eller degraderas
// Utan dem går det att låsa sig ute ur systemet helt.
// =====================================================================

use axum::extract::{ConnectInfo, Path, State};
use axum::Extension;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{now_ms, ApiError, ApiResult};
use crate::auth::{self, AuthUser, Role};
use crate::routes::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub disabled: bool,
    pub must_change_password: bool,
    pub last_login_at: Option<i64>,
    pub created_at: i64,
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: r.get(0)?,
        username: r.get(1)?,
        role: r.get(2)?,
        disabled: r.get::<_, i64>(3)? != 0,
        must_change_password: r.get::<_, i64>(4)? != 0,
        last_login_at: r.get(5)?,
        created_at: r.get(6)?,
    })
}

const SELECT: &str = "SELECT id, username, role, disabled, must_change_password,
                             last_login_at, created_at FROM users";

pub async fn list(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<UserRow>>> {
    let rows = state
        .db
        .call(|conn| {
            let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY username"))?;
            let rows = stmt.query_map([], read_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct NewUser {
    pub username: String,
    pub password: String,
    pub role: String,
}

/// Användarnamn: gemener, siffror, punkt, understreck, bindestreck.
/// En snäv uppsättning som inte kan smuggla in märkliga tecken i
/// loggar eller gränssnitt.
fn validate_username(name: &str, lang: crate::i18n::Lang) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(crate::i18n::username_chars(lang).into())
    }
}

/// Varför en uppdatering/borttagning avvisades. Typad i stället för
/// strängmatchning — meddelandet skapas på aktivt språk i ytterkanten.
enum UserReject {
    NotFound,
    LastAdmin,
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<NewUser>,
) -> ApiResult<Json<UserRow>> {
    let username = body.username.trim().to_lowercase();
    let lang = crate::i18n::load_db(&state.db).await;
    if let Err(msg) = validate_username(&username, lang) {
        return Err(ApiError::bad_request(msg));
    }
    if Role::parse(&body.role).is_none() {
        return Err(ApiError::bad_request(crate::i18n::role_invalid(lang)));
    }
    if let Err(msg) = auth::validate_password(&body.password, lang) {
        return Err(ApiError::bad_request(msg));
    }

    let hash = auth::hash_password(&body.password)?;
    let now = now_ms();
    let uname = username.clone();
    let role = body.role.clone();

    let row = state
        .db
        .call(move |conn| {
            let exists: Option<i64> = conn
                .query_row("SELECT id FROM users WHERE username = ?1", [&uname], |r| r.get(0))
                .optional()?;
            if exists.is_some() {
                anyhow::bail!(crate::i18n::username_taken(lang));
            }
            conn.execute(
                "INSERT INTO users
                    (username, password_hash, role, must_change_password, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4, ?4)",
                params![uname, hash, role, now],
            )?;
            let id = conn.last_insert_rowid();
            let row = conn.query_row(
                &format!("{SELECT} WHERE id = ?1"),
                [id],
                read_row,
            )?;
            Ok(row)
        })
        .await
        .map_err(|e| ApiError::bad_request(format!("{e}")))?;

    super::audit::record(
        &state.db,
        &actor.username,
        "user_create",
        Some(&username),
        Some(&format!("roll {}", body.role)),
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(row))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPatch {
    pub role: Option<String>,
    pub disabled: Option<bool>,
    /// Nytt lösenord. Blir ett engångslösen — mottagaren måste byta
    /// det vid första inloggningen.
    pub password: Option<String>,
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
    Json(p): Json<UserPatch>,
) -> ApiResult<Json<UserRow>> {
    // Skydden: inte sig själv, inte sista admin.
    let lang = crate::i18n::load_db(&state.db).await;
    if actor.id == id && (p.role.is_some() || p.disabled.is_some()) {
        return Err(ApiError::bad_request(crate::i18n::self_change_forbidden(lang)));
    }

    if let Some(role) = &p.role {
        if Role::parse(role).is_none() {
            return Err(ApiError::bad_request(crate::i18n::role_invalid(lang)));
        }
    }
    if let Some(pw) = &p.password {
        if let Err(msg) = auth::validate_password(pw, lang) {
            return Err(ApiError::bad_request(msg));
        }
    }

    let new_hash = match &p.password {
        Some(pw) => Some(auth::hash_password(pw)?),
        None => None,
    };

    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();
    let role_for_audit = p.role.clone();

    let outcome = state
        .db
        .call(move |conn| {
            let target: Option<(String, String)> = conn
                .query_row("SELECT username, role FROM users WHERE id = ?1", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            let Some((target_name, target_role)) = target else {
                return Ok(Err(UserReject::NotFound));
            };

            let removes_admin = p.role.as_deref() == Some("user") && target_role == "admin"
                || p.disabled == Some(true) && target_role == "admin";
            if removes_admin {
                let admins: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0",
                    [],
                    |r| r.get(0),
                )?;
                if admins <= 1 {
                    return Ok(Err(UserReject::LastAdmin));
                }
            }

            let now = now_ms();
            if let Some(role) = &p.role {
                conn.execute("UPDATE users SET role = ?2 WHERE id = ?1", params![id, role])?;
            }
            if let Some(dis) = p.disabled {
                conn.execute(
                    "UPDATE users SET disabled = ?2 WHERE id = ?1",
                    params![id, dis as i64],
                )?;
                if dis {
                    // Ett avstängt konto ska sluta fungera omedelbart,
                    // inte när sessionen råkar löpa ut.
                    auth::destroy_user_sessions(conn, id, None)?;
                }
            }
            if let Some(hash) = &new_hash {
                conn.execute(
                    "UPDATE users SET password_hash = ?2, must_change_password = 1,
                            failed_attempts = 0, locked_until = NULL WHERE id = ?1",
                    params![id, hash],
                )?;
                auth::destroy_user_sessions(conn, id, None)?;
            }
            conn.execute("UPDATE users SET updated_at = ?2 WHERE id = ?1", params![id, now])?;

            let row = conn.query_row(&format!("{SELECT} WHERE id = ?1"), [id], read_row)?;
            Ok(Ok((row, target_name)))
        })
        .await?;

    let (row, target_name) = match outcome {
        Ok(v) => v,
        Err(UserReject::NotFound) => {
            return Err(ApiError::not_found(crate::i18n::user_not_found(lang)));
        }
        Err(UserReject::LastAdmin) => {
            return Err(ApiError::bad_request(crate::i18n::last_admin_change(lang)));
        }
    };

    let mut actions = Vec::new();
    if let Some(role) = role_for_audit {
        actions.push(crate::i18n::audit_role_change(lang, &role));
    }
    if let Some(d) = p.disabled {
        actions.push(
            if d {
                crate::i18n::audit_disabled(lang)
            } else {
                crate::i18n::audit_enabled(lang)
            }
            .into(),
        );
    }
    if p.password.is_some() {
        actions.push(crate::i18n::audit_password_reset(lang).into());
    }
    super::audit::record(
        &state.db,
        &actor_name,
        "user_update",
        Some(&target_name),
        Some(&actions.join(", ")),
        Some(&ip),
    )
    .await;

    Ok(Json(row))
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
) -> ApiResult<Json<()>> {
    if actor.id == id {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::bad_request(crate::i18n::self_delete_forbidden(lang)));
    }

    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();

    let outcome = state
        .db
        .call(move |conn| {
            let target: Option<(String, String)> = conn
                .query_row("SELECT username, role FROM users WHERE id = ?1", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            let Some((name, role)) = target else {
                return Ok(Err(UserReject::NotFound));
            };
            if role == "admin" {
                let admins: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0",
                    [],
                    |r| r.get(0),
                )?;
                if admins <= 1 {
                    return Ok(Err(UserReject::LastAdmin));
                }
            }
            // Sessionerna följer med via ON DELETE CASCADE. Auditposterna
            // ligger kvar med användarnamnet — ett borttaget konto får
            // inte sudda sitt spår.
            conn.execute("DELETE FROM users WHERE id = ?1", [id])?;
            Ok(Ok(name))
        })
        .await?;

    let target_name = match outcome {
        Ok(v) => v,
        Err(UserReject::NotFound) => {
            let lang = crate::i18n::load_db(&state.db).await;
            return Err(ApiError::not_found(crate::i18n::user_not_found(lang)));
        }
        Err(UserReject::LastAdmin) => {
            let lang = crate::i18n::load_db(&state.db).await;
            return Err(ApiError::bad_request(crate::i18n::last_admin_delete(lang)));
        }
    };

    super::audit::record(
        &state.db,
        &actor_name,
        "user_delete",
        Some(&target_name),
        None,
        Some(&ip),
    )
    .await;

    Ok(Json(()))
}
