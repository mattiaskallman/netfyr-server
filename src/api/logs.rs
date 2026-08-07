// =====================================================================
// api/logs.rs
// Händelselogg och leveranslogg.
//
// Båda är rena läsvyer. Ingen radering — en logg som går att redigera
// är inte en logg.
// =====================================================================

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::ApiResult;
use crate::routes::AppState;

#[derive(Deserialize)]
pub struct Limit {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    200
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRow {
    pub id: i64,
    pub ts: i64,
    pub level: String,
    pub text: String,
}

pub async fn events(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Limit>,
) -> ApiResult<Json<Vec<EventRow>>> {
    let limit = q.limit.clamp(1, 2000);
    let rows = state
        .db
        .call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, ts, level, text FROM events ORDER BY ts DESC, id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit], |r| {
                Ok(EventRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    level: r.get(2)?,
                    text: r.get(3)?,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRow {
    pub id: i64,
    pub channel: String,
    pub device: String,
    pub event: String,
    pub status: String,
    pub attempts: i64,
    pub next_attempt: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub sent_at: Option<i64>,
}

pub async fn deliveries(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Limit>,
) -> ApiResult<Json<Vec<DeliveryRow>>> {
    let limit = q.limit.clamp(1, 2000);
    let rows = state
        .db
        .call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, channel, device, event, status, attempts, next_attempt,
                        last_error, created_at, sent_at
                 FROM deliveries ORDER BY created_at DESC, id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit], |r| {
                Ok(DeliveryRow {
                    id: r.get(0)?,
                    channel: r.get(1)?,
                    device: r.get(2)?,
                    event: r.get(3)?,
                    status: r.get(4)?,
                    attempts: r.get(5)?,
                    next_attempt: r.get(6)?,
                    last_error: r.get(7)?,
                    created_at: r.get(8)?,
                    sent_at: r.get(9)?,
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
