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
use std::cmp::Reverse;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::engine::repo;
use crate::routes::AppState;

/// Antal bucklar i svarstidslinjen — samma som desktopvariantens STATS_COLS.
const COLS: usize = 48;
const CACHE_TTL: Duration = Duration::from_secs(30);

struct CachedStats {
    created: Instant,
    value: StatsView,
}

#[derive(Default)]
struct StatsCache {
    generation: u64,
    entries: HashMap<String, CachedStats>,
}

static CACHE: OnceLock<Mutex<StatsCache>> = OnceLock::new();

fn cache() -> &'static Mutex<StatsCache> {
    CACHE.get_or_init(|| Mutex::new(StatsCache::default()))
}

fn cached(window: &str) -> Option<StatsView> {
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entries
        .retain(|_, item| item.created.elapsed() < CACHE_TTL);
    guard.entries.get(window).map(|item| item.value.clone())
}

fn cache_generation() -> u64 {
    cache().lock().unwrap_or_else(|e| e.into_inner()).generation
}

/// Kontroll och insert sker under samma lås. Därmed kan clear() aldrig
/// hamna mellan generationskontrollen och återfyllningen.
fn cache_put_if_generation(generation: u64, window: String, value: StatsView) -> bool {
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    if guard.generation != generation {
        return false;
    }
    guard.entries.insert(
        window,
        CachedStats {
            created: Instant::now(),
            value,
        },
    );
    true
}

fn cache_clear() {
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    guard.generation = guard.generation.wrapping_add(1);
    guard.entries.clear();
}

const KPI_SQL: &str = "SELECT address,
            COUNT(*) AS total,
            SUM(online) AS online_count,
            AVG(CASE WHEN online = 1 AND latency_us > 0
                     THEN latency_us / 1000.0 END) AS avg_ms
     FROM samples INDEXED BY idx_samples_ts
     WHERE ts >= ?1 GROUP BY address";

const P95_SQL: &str = "WITH ranked AS (
       SELECT address, latency_us / 1000.0 AS ms,
              ROW_NUMBER() OVER (PARTITION BY address ORDER BY latency_us) AS rn,
              COUNT(*)     OVER (PARTITION BY address) AS cnt
       FROM samples INDEXED BY idx_samples_ts
       WHERE ts >= ?1 AND online = 1 AND latency_us > 0
     )
     SELECT address, ms FROM ranked
     WHERE rn = MIN(cnt,
                    CAST(0.95 * cnt AS INT)
                    + (CASE WHEN 0.95 * cnt > CAST(0.95 * cnt AS INT)
                            THEN 1 ELSE 0 END))";

const LINE_SQL: &str = "SELECT address,
            MAX(0, MIN(?1 - 1, CAST(((ts - ?2) * 1.0 / ?3) * ?1 AS INT))) AS bucket,
            AVG(CASE WHEN online = 1 AND latency_us > 0
                     THEN latency_us / 1000.0 END) AS avg_ms,
            SUM(CASE WHEN online = 1 AND latency_us > 0 THEN 1 ELSE 0 END) AS online_count,
            SUM(CASE WHEN online = 0 THEN 1 ELSE 0 END) AS offline_count
     FROM samples INDEXED BY idx_samples_ts
     WHERE ts >= ?2
     GROUP BY address, bucket";

const INCIDENTS_SQL: &str = "WITH flagged AS (
       SELECT address, ts, online,
              LAG(online) OVER (PARTITION BY address ORDER BY ts) AS prev
       FROM samples INDEXED BY idx_samples_ts WHERE ts >= ?1
     )
     SELECT address, ts, online FROM flagged
     WHERE prev IS NULL OR online <> prev
     ORDER BY address, ts";

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

#[derive(Clone, Serialize)]
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

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Incident {
    pub name: String,
    pub address: String,
    pub start: i64,
    /// Null = pågår fortfarande.
    pub end: Option<i64>,
}

#[derive(Clone, Serialize)]
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
            return Err(ApiError::bad_request(crate::i18n::unknown_window(
                lang, &window,
            )));
        }
    };
    let now = super::now_ms();
    let from = now - span;

    if let Some(out) = cached(&window) {
        return Ok(Json(out));
    }

    let out = state
        .db
        .read_call(move |conn| {
            // read_call serialiserar cache-missar. Kontrollera igen efter
            // grinden så samtidiga anrop delar samma byggjobb.
            let generation = cache_generation();
            if let Some(out) = cached(&window) {
                return Ok(out);
            }
            let out = build(conn, &window, from, now)?;
            // En historikradering kan ha skett medan snapshoten byggdes.
            // Lägg aldrig tillbaka data från en äldre generation.
            cache_put_if_generation(generation, window, out.clone());
            Ok(out)
        })
        .await?;
    Ok(Json(out))
}

fn build(conn: &Connection, window: &str, from: i64, to: i64) -> anyhow::Result<StatsView> {
    let hosts = repo::load_hosts(conn)?;

    // KPI per adress: antal, online-antal och medelsvarstid (ms).
    let mut kpi: HashMap<String, (i64, i64, Option<f64>)> = HashMap::new();
    let mut total_samples: i64 = 0;
    {
        let mut stmt = conn.prepare(KPI_SQL)?;
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
        let mut stmt = conn.prepare(P95_SQL)?;
        let rows = stmt.query_map([from], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
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
        let mut stmt = conn.prepare(LINE_SQL)?;
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
        let mut stmt = conn.prepare(INCIDENTS_SQL)?;
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
        incidents.sort_by_key(|item| Reverse(item.start));
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
    rows.sort_by_key(|item| item.name.to_lowercase());

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
    cache_clear();

    super::audit::record(
        &state.db,
        &actor.username,
        "stats_clear",
        None,
        Some(&crate::i18n::cleared_detail(
            crate::i18n::load_db(&state.db).await,
            removed,
        )),
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(serde_json::json!({ "removed": removed })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidsfiltrerade_statistikfragor_anvander_tidsindex() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (id INTEGER PRIMARY KEY, address TEXT NOT NULL, ts INTEGER NOT NULL, online INTEGER NOT NULL, latency_us INTEGER);
             CREATE INDEX idx_samples_ts ON samples(ts);",
        )
        .unwrap();

        let cases: [(&str, Vec<i64>); 4] = [
            (KPI_SQL, vec![1]),
            (P95_SQL, vec![1]),
            (LINE_SQL, vec![48, 1, 1000]),
            (INCIDENTS_SQL, vec![1]),
        ];

        for (sql, values) in cases {
            let explain = format!("EXPLAIN QUERY PLAN {sql}");
            let mut stmt = conn.prepare(&explain).unwrap();
            let details: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(values), |row| row.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                details
                    .iter()
                    .any(|detail| detail.contains("idx_samples_ts (ts>?)")),
                "frågan använde inte tidsindex: {details:?}"
            );
        }
    }

    fn empty_stats() -> StatsView {
        StatsView {
            window: "24h".to_string(),
            from: 1,
            to: 2,
            total_samples: 0,
            rows: Vec::new(),
            incidents: Vec::new(),
        }
    }

    #[test]
    fn cache_clear_tar_bort_tidigare_statistik() {
        cache_clear();
        let generation = cache_generation();
        assert!(cache_put_if_generation(
            generation,
            "24h".to_string(),
            empty_stats()
        ));
        assert!(cached("24h").is_some());
        cache_clear();
        assert!(cached("24h").is_none());
    }

    #[test]
    fn gammal_snapshot_kan_inte_aterfylla_cache_efter_clear() {
        cache_clear();
        let stale_generation = cache_generation();

        // Deterministiskt interleaving: snapshoten startar, clear sker,
        // därefter försöker snapshoten återfylla cachen.
        cache_clear();
        assert!(!cache_put_if_generation(
            stale_generation,
            "24h".to_string(),
            empty_stats()
        ));
        assert!(cached("24h").is_none());
    }
}
