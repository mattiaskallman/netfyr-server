// =====================================================================
// api/maintenance.rs
// Underhållsfönster: planerade tider då larm tystas.
//
// Motorn läser fönstren varje svep och håller tillbaka larmen. Det här
// är administrationen av dem — desktopvariantens inställningssida
// "Underhåll", som serverns REST-API.
//
// Två scheman stöds, samma som desktop:
//   once  — ett engångsspann mellan två tidpunkter (ms-sedan-epok)
//   daily — återkommande, i serverns lokala tid, valfria veckodagar
//
// Målet är antingen alla enheter, en grupp, eller en lista med
// enheter. Gruppmål slås upp vid varje svep — nya enheter i gruppen
// omfattas automatiskt.
//
// Bara admin ändrar. Vanliga användare kan läsa listan — att se att
// "det är tyst för att det är underhåll" är inte hemligt.
// =====================================================================

use axum::{
    extract::{ConnectInfo, Extension, Path, State},
    Json,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{audit, now_ms, ApiError};
use crate::auth::AuthUser;
use crate::routes::AppState;

// ---- Typer -----------------------------------------------------------

#[derive(Serialize)]
pub struct WindowRow {
    id: i64,
    label: String,
    enabled: bool,
    /// Räknas ut på servern: enhetlig tid, och visningslogik ska
    /// inte bo i klienten.
    #[serde(rename = "activeNow")]
    active_now: bool,
    /// "once" eller "daily".
    kind: String,
    // once
    #[serde(rename = "startsAt")]
    starts_at: Option<i64>,
    #[serde(rename = "endsAt")]
    ends_at: Option<i64>,
    // daily
    #[serde(rename = "startMin")]
    start_min: Option<i64>,
    #[serde(rename = "durationMin")]
    duration_min: Option<i64>,
    days: Vec<i64>,
    // mål
    #[serde(rename = "targetKind")]
    target_kind: String,
    #[serde(rename = "groupId")]
    group_id: Option<i64>,
    group: Option<String>,
    #[serde(rename = "hostIds")]
    host_ids: Vec<i64>,
}

#[derive(Deserialize)]
pub struct NewWindow {
    label: String,
    kind: String,
    #[serde(rename = "targetKind")]
    target_kind: String,
    #[serde(rename = "startsAt")]
    starts_at: Option<i64>,
    #[serde(rename = "endsAt")]
    ends_at: Option<i64>,
    #[serde(rename = "startMin")]
    start_min: Option<i64>,
    #[serde(rename = "durationMin")]
    duration_min: Option<i64>,
    days: Option<Vec<i64>>,
    #[serde(rename = "groupId")]
    group_id: Option<i64>,
    #[serde(rename = "hostIds")]
    host_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
pub struct WindowPatch {
    label: Option<String>,
    enabled: Option<bool>,
}

// ---- Hjälpare --------------------------------------------------------

fn row_to_api(
    conn: &Connection,
    id: i64,
    label: String,
    enabled: bool,
    kind: String,
    starts_at: Option<i64>,
    ends_at: Option<i64>,
    start_min: Option<i64>,
    duration_min: Option<i64>,
    days_raw: Option<String>,
    target_kind: String,
    group_id: Option<i64>,
    now: i64,
) -> Result<WindowRow, rusqlite::Error> {
    use crate::engine::suppression::{self, MaintenanceWindow, Schedule, Target};

    let days: Vec<i64> = days_raw
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();

    let host_ids: Vec<i64> = if target_kind == "hosts" {
        let mut s =
            conn.prepare("SELECT host_id FROM maintenance_targets WHERE window_id = ?1")?;
        let rows = s.query_map([id], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    let group: Option<String> = match group_id {
        Some(gid) => conn
            .query_row("SELECT name FROM groups WHERE id = ?1", [gid], |r| r.get(0))
            .ok(),
        None => None,
    };

    // Räkna ut om fönstret är aktivt just nu med exakt samma logik
    // som motorn använder.
    let schedule = if kind == "daily" {
        Schedule::Daily {
            start_min: start_min.unwrap_or(0) as u32,
            duration_min: duration_min.unwrap_or(0) as u32,
            days: days.iter().map(|d| *d as u8).collect(),
        }
    } else {
        Schedule::Once {
            start: starts_at.unwrap_or(0),
            end: ends_at.unwrap_or(0),
        }
    };
    let target = match target_kind.as_str() {
        "group" => Target::Group(group_id.unwrap_or(-1)),
        "hosts" => Target::Hosts(host_ids.clone()),
        _ => Target::All,
    };
    let w = MaintenanceWindow { id, enabled, schedule, target };
    let active_now = suppression::is_window_active(&w, now);

    Ok(WindowRow {
        id,
        label,
        enabled,
        active_now,
        kind,
        starts_at,
        ends_at,
        start_min,
        duration_min,
        days,
        target_kind,
        group_id,
        group,
        host_ids,
    })
}

// ---- Läs -------------------------------------------------------------

pub async fn list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let now = now_ms();
    let windows = state
        .db
        .call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, label, enabled, kind, starts_at, ends_at,
                        start_min, duration_min, days, target_kind, target_group_id
                 FROM maintenance_windows ORDER BY label",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                ))
            })?;
            let mut out: Vec<WindowRow> = Vec::new();
            for row in rows {
                let (id, label, enabled, kind, sa, ea, sm, dm, days, tk, gid) = row?;
                out.push(row_to_api(
                    conn, id, label, enabled, kind, sa, ea, sm, dm, days, tk, gid, now,
                )?);
            }
            Ok(out)
        })
        .await?;

    Ok(Json(serde_json::json!({ "windows": windows })))
}

// ---- Skapa -----------------------------------------------------------

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<NewWindow>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = addr.ip().to_string();
    let actor_name = user.username.clone();
    let label = body.label.trim().to_string();
    if label.is_empty() {
        return Err(ApiError::bad_request("ange en etikett"));
    }

    // Validera schemat innan något skrivs.
    let lang = crate::i18n::load_db(&state.db).await;
    match body.kind.as_str() {
        "once" => {
            let (s, e) = (body.starts_at, body.ends_at);
            match (s, e) {
                (Some(s), Some(e)) if e > s => {}
                _ => return Err(ApiError::bad_request(crate::i18n::end_before_start(lang))),
            }
        }
        "daily" => {
            let sm = body.start_min.unwrap_or(0);
            let dm = body.duration_min.unwrap_or(0);
            if !(0..1440).contains(&sm) || dm < 1 {
                return Err(ApiError::bad_request(crate::i18n::invalid_time(lang)));
            }
        }
        _ => return Err(ApiError::bad_request(crate::i18n::kind_invalid(lang))),
    }

    if !["all", "group", "hosts"].contains(&body.target_kind.as_str()) {
        return Err(ApiError::bad_request(crate::i18n::target_invalid(lang)));
    }

    let days_raw = body
        .days
        .unwrap_or_default()
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let host_ids = body.host_ids.clone().unwrap_or_default();

    let now = now_ms();
    let id = state
        .db
        .call({
            let label = label.clone();
            let kind = body.kind.clone();
            let target_kind = body.target_kind.clone();
            let group_id = body.group_id;
            let starts_at = body.starts_at;
            let ends_at = body.ends_at;
            let start_min = body.start_min;
            let duration_min = body.duration_min;
            move |conn| {
                let tx = conn.unchecked_transaction()?;
                tx.execute(
                    "INSERT INTO maintenance_windows
                        (label, enabled, kind, starts_at, ends_at,
                         start_min, duration_min, days, target_kind, target_group_id, created_at)
                     VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        label, kind, starts_at, ends_at,
                        start_min, duration_min, days_raw, target_kind, group_id, now,
                    ],
                )?;
                let id = tx.last_insert_rowid();
                if target_kind == "hosts" {
                    for hid in &host_ids {
                        tx.execute(
                            "INSERT INTO maintenance_targets (window_id, host_id) VALUES (?1, ?2)",
                            params![id, hid],
                        )?;
                    }
                }
                tx.commit()?;
                Ok(id)
            }
        })
        .await?;

    audit::record(&state.db, &actor_name, "create_maintenance", Some(&label), Some(&id.to_string()), Some(&ip)).await;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

// ---- Ändra (etikett/på-av) --------------------------------------------

pub async fn update(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
    Json(body): Json<WindowPatch>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = addr.ip().to_string();
    let actor_name = user.username.clone();
    let found = state
        .db
        .call({
            let label = body.label.clone();
            move |conn| {
                let exists: bool = conn
                    .query_row(
                        "SELECT COUNT(*) FROM maintenance_windows WHERE id = ?1",
                        [id],
                        |r| r.get::<_, i64>(0),
                    )?
                    > 0;
                if !exists {
                    return Ok(false);
                }
                if let Some(l) = label {
                    conn.execute(
                        "UPDATE maintenance_windows SET label = ?2 WHERE id = ?1",
                        params![id, l.trim()],
                    )?;
                }
                if let Some(en) = body.enabled {
                    conn.execute(
                        "UPDATE maintenance_windows SET enabled = ?2 WHERE id = ?1",
                        params![id, en as i64],
                    )?;
                }
                Ok(true)
            }
        })
        .await?;

    if !found {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::not_found(crate::i18n::window_not_found(lang)));
    }
    audit::record(&state.db, &actor_name, "update_maintenance", Some(&id.to_string()), None, Some(&ip)).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- Ta bort -----------------------------------------------------------

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ip = addr.ip().to_string();
    let actor_name = user.username.clone();
    let deleted = state
        .db
        .call(move |conn| {
            let n = conn.execute("DELETE FROM maintenance_windows WHERE id = ?1", [id])?;
            conn.execute("DELETE FROM maintenance_targets WHERE window_id = ?1", [id])?;
            Ok(n > 0)
        })
        .await?;

    if !deleted {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::not_found(crate::i18n::window_not_found(lang)));
    }
    audit::record(&state.db, &actor_name, "delete_maintenance", Some(&id.to_string()), None, Some(&ip)).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
