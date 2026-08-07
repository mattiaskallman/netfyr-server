// =====================================================================
// api/audit.rs
// Auditlogg: vem gjorde vad, när, varifrån.
//
// Skrivning sker via record() från handlarna. Läsning är en ren vy för
// administratörer. Det finns medvetet ingen ändring eller radering —
// gallring efter AUDIT_RETENTION_DAYS görs av städloopen i auth.rs.
//
// Det är den här tabellen som svarar på NIS2-frågan "vad hände, och
// vem gjorde det?" vid en incident.
// =====================================================================

use axum::extract::{Query, State};
use axum::Json;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{now_ms, ApiResult};
use crate::db::Db;
use crate::routes::AppState;

/// Skriv en auditpost. Misslyckas skrivningen loggas det, men anropet
/// som utlöste posten får inte falla på det — en övervakningstjänst som
/// vägrar kvittera larm för att loggen är trasig har fel prioriteringar.
pub async fn record(
    db: &Db,
    username: &str,
    action: &str,
    target: Option<&str>,
    detail: Option<&str>,
    ip: Option<&str>,
) {
    let username = username.to_string();
    let action = action.to_string();
    let target = target.map(|s| s.to_string());
    let detail = detail.map(|s| s.to_string());
    let ip = ip.map(|s| s.to_string());

    let res = db
        .call(move |conn| {
            conn.execute(
                "INSERT INTO audit_log (ts, username, action, target, detail, ip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![now_ms(), username, action, target, detail, ip],
            )?;
            Ok(())
        })
        .await;

    if let Err(e) = res {
        tracing::error!("kunde inte skriva auditpost: {e:#}");
    }
}

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
pub struct AuditRow {
    pub id: i64,
    pub ts: i64,
    pub username: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
    pub ip: Option<String>,
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Limit>,
) -> ApiResult<Json<Vec<AuditRow>>> {
    let limit = q.limit.clamp(1, 2000);
    let rows = state
        .db
        .call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, ts, username, action, target, detail, ip
                 FROM audit_log ORDER BY ts DESC, id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit], |r| {
                Ok(AuditRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    username: r.get(2)?,
                    action: r.get(3)?,
                    target: r.get(4)?,
                    detail: r.get(5)?,
                    ip: r.get(6)?,
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
