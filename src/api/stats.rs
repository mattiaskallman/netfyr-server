// =====================================================================
// api/stats.rs
// Statistik/KPI — port av desktopvariantens kpi.ts + db.ts-aggregeringar.
//
// Samma semantik som desktop, medvetet:
//   * drifttid = online-mätningar / alla mätningar i fönstret (rådata,
//     inte grindens bekräftade status — ett flakande nät syns även om
//     det aldrig larmade)
//   * snitt och p95 räknas bara på svar med svarstid > 0
//   * p95 = diskret index: min(cnt, ceil(0.95 * cnt)) — samma som desktop
//   * linjen är 48 fasta tidsbucklar: snitt av svarande, -1 om någon
//     mätning i buckeln missades, null om ingen data alls
//   * incidenter = råa online↔offline-växlingar (LAG i SQL, intervallen
//     byggs i Rust — samma uppdelning av arbete som desktop)
//
// Det tunga lyftet sker i SQLite, inte i Rust-minne: fönster över 180
// dagar ska inte innebära att hundratusentals rader åker genom API:t.
// =====================================================================

use axum::extract::{ConnectInfo, Query, State};
use axum::{Extension, Json};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::engine::repo;
use crate::routes::AppState;

/// Antal bucklar i svarstidslinjen — samma som desktopvariantens STATS_COLS.
const COLS: usize = 48;

fn window_ms(name: &str) -> Option<i64> {
    match name {
        "24h" => Some(86_400_000),
        "7d" => Some(604_800_000),
        "30d" => Some(2_592_000_000),
        "60d" => Some(5_184_000_000),
        "90d" => Some(7_776_000_000),
        "180d" => Some(15_552_000_000),
        _ => None,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsRow {
    pub host_id: i64,
    pub name: String,
    pub address: String,
    pub group: Option<String>,
    pub total: i64,
    pub online_count: i64,
    /// Null när enheten saknar mätningar i fönstret.
    pub uptime_pct: Option<f64>,
    pub avg_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    /// 48 värden: snitt-ms, -1 = missad mätning i buckeln, null = ingen data.
    pub line: Vec<Option<f64>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Incident {
    pub name: String,
    pub address: String,
    pub start: i64,
    /// Null = pågår fortfarande.
    pub end: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsView {
    pub window: String,
    pub from: i64,
    pub to: i64,
    pub total_samples: i64,
    pub rows: Vec<StatsRow>,
    pub incidents: Vec<Incident>,
}

#[derive(serde::Deserialize)]
pub struct Params {
    window: Option<String>,
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Json<StatsView>> {
    let window = p.window.unwrap_or_else(|| "24h".to_string());
    let span = match window_ms(&window) {
        Some(s) => s,
        None => {
            let lang = crate::i18n::load_db(&state.db).await;
            return Err(ApiError::bad_request(crate::i18n::unknown_window(lang, &window)));
        }
    };
    let now = super::now_ms();
    let from = now - span;

    let out = state
        .db
        .call(move |conn| build(conn, &window, from, now))
        .await?;
    Ok(Json(out))
}

fn build(conn: &Connection, window: &str, from: i64, to: i64) -> anyhow::Result<StatsView> {
    let hosts = repo::load_hosts(conn)?;

    // KPI per adress: antal, online-antal och medelsvarstid (ms).
    let mut kpi: HashMap<String, (i64, i64, Option<f64>)> = HashMap::new();
    let mut total_samples: i64 = 0;
    {
        let mut stmt = conn.prepare(
            "SELECT address,
                    COUNT(*) AS total,
                    SUM(online) AS online_count,
                    AVG(CASE WHEN online = 1 AND latency_us > 0
                             THEN latency_us / 1000.0 END) AS avg_ms
             FROM samples WHERE ts >= ?1 GROUP BY address",
        )?;
        let rows = stmt.query_map([from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<f64>>(3)?,
            ))
        })?;
        for row in rows {
            let (addr, total, online, avg) = row?;
            total_samples += total;
            kpi.insert(addr, (total, online, avg));
        }
    }

    // Exakt 95:e percentil per adress — samma diskreta index som
    // desktop: 1-baserat rn = min(cnt, ceil(0.95 * cnt)).
    let mut p95: HashMap<String, f64> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "WITH ranked AS (
               SELECT address, latency_us / 1000.0 AS ms,
                      ROW_NUMBER() OVER (PARTITION BY address ORDER BY latency_us) AS rn,
                      COUNT(*)     OVER (PARTITION BY address) AS cnt
               FROM samples
               WHERE ts >= ?1 AND online = 1 AND latency_us > 0
             )
             SELECT address, ms FROM ranked
             WHERE rn = MIN(cnt,
                            CAST(0.95 * cnt AS INT)
                            + (CASE WHEN 0.95 * cnt > CAST(0.95 * cnt AS INT)
                                    THEN 1 ELSE 0 END))",
        )?;
        let rows = stmt
            .query_map([from], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?;
        for row in rows {
            let (addr, ms) = row?;
            p95.insert(addr, ms);
        }
    }

    // Linjen: 48 fasta tidsbucklar. Per buckel: online-snitt om någon
    // svarade, annars -1 om någon mätning missades, annars null (ritas
    // som hål i linjen).
    let span = (to - from).max(1);
    let mut lines: HashMap<String, Vec<Option<f64>>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT address,
                    MAX(0, MIN(?1 - 1, CAST(((ts - ?2) * 1.0 / ?3) * ?1 AS INT))) AS bucket,
                    AVG(CASE WHEN online = 1 AND latency_us > 0
                             THEN latency_us / 1000.0 END) AS avg_ms,
                    SUM(CASE WHEN online = 1 AND latency_us > 0 THEN 1 ELSE 0 END) AS online_count,
                    SUM(CASE WHEN online = 0 THEN 1 ELSE 0 END) AS offline_count
             FROM samples WHERE ts >= ?2
             GROUP BY address, bucket",
        )?;
        let rows = stmt.query_map(rusqlite::params![COLS as i64, from, span], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as usize,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        })?;
        for row in rows {
            let (addr, bucket, avg, online, offline) = row?;
            if bucket >= COLS {
                continue;
            }
            let line = lines.entry(addr).or_insert_with(|| vec![None; COLS]);
            line[bucket] = if online > 0 {
                avg
            } else if offline > 0 {
                Some(-1.0)
            } else {
                None
            };
        }
    }

    // Incidenter: endast tillståndsväxlingar via LAG — samma trick som
    // desktop, och ger identiska intervall som att läsa rådata.
    let mut incidents: Vec<Incident> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "WITH flagged AS (
               SELECT address, ts, online,
                      LAG(online) OVER (PARTITION BY address ORDER BY ts) AS prev
               FROM samples WHERE ts >= ?1
             )
             SELECT address, ts, online FROM flagged
             WHERE prev IS NULL OR online <> prev
             ORDER BY address, ts",
        )?;
        let rows = stmt.query_map([from], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        let name_of: HashMap<&str, &str> = hosts
            .iter()
            .map(|h| (h.address.as_str(), h.name.as_str()))
            .collect();
        let mut open: HashMap<String, i64> = HashMap::new();
        for row in rows {
            let (addr, ts, online) = row?;
            if !online {
                // Första växlingen i fönstret kan vara "offline" redan vid
                // start — då börjar incidenten vid fönsterkanten, samma
                // som i desktop.
                open.entry(addr).or_insert(ts);
            } else if let Some(start) = open.remove(&addr) {
                incidents.push(incident(&name_of, &addr, start, Some(ts)));
            }
        }
        // Kvar i `open` = avbrott som pågår fortfarande.
        for (addr, start) in open {
            incidents.push(incident(&name_of, &addr, start, None));
        }
        // Nyast först. Taket finns för att ett flakande nät inte ska
        // kunna skicka tusentals rader till gränssnittet.
        incidents.sort_by(|a, b| b.start.cmp(&a.start));
        incidents.truncate(200);
    }

    // En rad per enhet — även enheter utan mätningar (de får null-KPIer,
    // samma som desktop).
    let mut rows: Vec<StatsRow> = hosts
        .iter()
        .map(|h| {
            let (total, online, avg) = kpi.get(&h.address).copied().unwrap_or((0, 0, None));
            StatsRow {
                host_id: h.id,
                name: h.name.clone(),
                address: h.address.clone(),
                group: h.group.clone(),
                total,
                online_count: online,
                uptime_pct: if total > 0 {
                    Some(online as f64 / total as f64 * 100.0)
                } else {
                    None
                },
                avg_ms: avg,
                p95_ms: p95.get(&h.address).copied(),
                line: lines
                    .get(&h.address)
                    .cloned()
                    .unwrap_or_else(|| vec![None; COLS]),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    Ok(StatsView {
        window: window.to_string(),
        from,
        to,
        total_samples,
        rows,
        incidents,
    })
}

fn incident(name_of: &HashMap<&str, &str>, addr: &str, start: i64, end: Option<i64>) -> Incident {
    Incident {
        name: name_of.get(addr).unwrap_or(&addr).to_string(),
        address: addr.to_string(),
        start,
        end,
    }
}

/// Rensa all mätdata — motsvarigheten till desktopens "Rensa historik".
///
/// Admin-only (routern). Att radera historik är oåterkalleligt och
/// påverkar alla användares statistik — därför auditloggas det.
pub async fn clear(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> ApiResult<Json<serde_json::Value>> {
    let removed = state
        .db
        .call(|conn| {
            let n = conn.execute("DELETE FROM samples", [])?;
            Ok(n)
        })
        .await?;

    super::audit::record(
        &state.db,
        &actor.username,
        "stats_clear",
        None,
        Some(&crate::i18n::cleared_detail(crate::i18n::load_db(&state.db).await, removed)),
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(serde_json::json!({ "removed": removed })))
}
