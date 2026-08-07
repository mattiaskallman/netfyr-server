// =====================================================================
// api/groups.rs
// Grupper.
//
// Desktop har gruppen som en fri sträng på enheten. Här är den en egen
// tabell med främmande nyckel. Skillnaden är avsiktlig: ett namnbyte
// slår igenom överallt på en gång, en borttagen grupp lämnar inga
// föräldralösa referenser, och underhållsfönster mot grupp blir
// korrekta även när gruppen ändras.
//
// API:t exponerar namnet vid läsning, så gränssnittet ser samma sak
// som desktop.
// =====================================================================

use axum::extract::{ConnectInfo, Path, State};
use axum::Extension;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::routes::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRow {
    pub id: i64,
    pub name: String,
    /// Antal enheter i gruppen. Gränssnittet visar det i filterraden.
    pub hosts: i64,
}

pub async fn list(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<GroupRow>>> {
    let rows = state
        .db
        .call(|conn| {
            let mut stmt = conn.prepare(
                "SELECT g.id, g.name, COUNT(h.id)
                 FROM groups g
                 LEFT JOIN hosts h ON h.group_id = g.id
                 GROUP BY g.id, g.name
                 ORDER BY g.name",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(GroupRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    hosts: r.get(2)?,
                })
            })?;
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
pub struct GroupBody {
    pub name: String,
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<GroupBody>,
) -> ApiResult<Json<GroupRow>> {
    let name = body.name.trim().to_string();
    let lang = crate::i18n::load_db(&state.db).await;
    if name.is_empty() {
        return Err(ApiError::bad_request(crate::i18n::name_missing(lang)));
    }
    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();

    let row = state
        .db
        .call(move |conn| {
            let exists: Option<i64> = conn
                .query_row("SELECT id FROM groups WHERE name = ?1", [&name], |r| r.get(0))
                .optional()?;
            if exists.is_some() {
                anyhow::bail!(crate::i18n::group_exists(lang));
            }
            conn.execute("INSERT INTO groups (name) VALUES (?1)", [&name])?;
            Ok(GroupRow {
                id: conn.last_insert_rowid(),
                name,
                hosts: 0,
            })
        })
        .await
        .map_err(|e| ApiError::bad_request(format!("{e}")))?;

    super::audit::record(&state.db, &actor_name, "group_create", Some(&row.name), None, Some(&ip)).await;

    Ok(Json(row))
}

pub async fn rename(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
    Json(body): Json<GroupBody>,
) -> ApiResult<Json<()>> {
    let name = body.name.trim().to_string();
    let lang = crate::i18n::load_db(&state.db).await;
    if name.is_empty() {
        return Err(ApiError::bad_request(crate::i18n::name_missing(lang)));
    }
    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();
    let name_for_db = name.clone();

    let n = state
        .db
        .call(move |conn| {
            let n = conn.execute(
                "UPDATE groups SET name = ?2 WHERE id = ?1",
                params![id, name_for_db],
            )?;
            Ok(n)
        })
        .await
        .map_err(|_| ApiError::bad_request(crate::i18n::name_taken(lang)))?;

    if n == 0 {
        return Err(ApiError::not_found(crate::i18n::group_not_found(lang)));
    }

    super::audit::record(&state.db, &actor_name, "group_rename", Some(&name), None, Some(&ip)).await;

    Ok(Json(()))
}

/// Ta bort en grupp.
///
/// Enheterna blir grupplösa, inte borttagna — schemat har ON DELETE SET
/// NULL. Att radera enheter för att någon städar bland grupper vore ett
/// oväntat och oåterkalleligt bortfall av bevakning.
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
) -> ApiResult<Json<()>> {
    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();
    let (n, name) = state
        .db
        .call(move |conn| {
            let name: Option<String> = conn
                .query_row("SELECT name FROM groups WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            let n = conn.execute("DELETE FROM groups WHERE id = ?1", [id])?;
            Ok((n, name))
        })
        .await?;

    if n == 0 {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::not_found(crate::i18n::group_not_found(lang)));
    }

    super::audit::record(&state.db, &actor_name, "group_delete", name.as_deref(), None, Some(&ip)).await;

    Ok(Json(()))
}
