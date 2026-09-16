// =====================================================================
// api/overview.rs
// Översikten — det gränssnittet visar först.
//
// Här kopplas engine::display in. Visningsstatus räknas ut på SERVERN,
// inte i webbläsaren: regeln om att rött bara betyder bekräftat larm
// får inte kunna tolkas olika av olika klienter.
// =====================================================================

use axum::extract::State;
use axum::Json;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

use super::{ApiResult, now_ms};
use crate::engine::display::{display_status_of, tally_hosts, DisplayTally};
use crate::engine::polls::PollCounters;
use crate::engine::repo;
use crate::engine::suppression::suppression_reason;
use crate::engine::types::{Host, RawStatus, Status};
use crate::routes::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostView {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub group_id: Option<i64>,
    pub group: Option<String>,
    pub note: Option<String>,
    pub enabled: bool,
    /// Bekräftad status från grinden.
    pub confirmed: Status,
    /// Vad enheten ska visas som. Enda källan till färg.
    pub display: crate::engine::types::DisplayStatus,
    pub reason: Option<crate::engine::types::SuppressionReason>,
    /// Mikrosekunder. Frontend formaterar för läsbarhet.
    pub latency_us: Option<u32>,
    /// När statusen senast ändrades — inte när den senast skrevs.
    pub changed_at: Option<i64>,
    /// När enheten senast mättes. Visar hur färska uppgifterna är.
    pub checked_at: Option<i64>,
    /// Antal pollningar sedan den aktuella serverprocessen startade, och
    /// hur många som gav svar. Börjar medvetet om från noll vid omstart.
    pub polls: i64,
    pub polls_ok: i64,
    /// Senaste svarstiderna i mikrosekunder, äldst först. Noll betyder
    /// uteblivet svar — grafen ska visa hålet, inte hoppa över det.
    pub spark: Vec<u32>,
    pub snooze_until: Option<i64>,
    pub depends_on_address: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TallyView {
    pub total: usize,
    pub active: usize,
    pub online: usize,
    pub warning: usize,
    pub uncertain: usize,
    pub offline: usize,
    pub suppressed: usize,
    pub suppressed_down: usize,
    pub paused: usize,
    pub alarming: usize,
}

impl From<DisplayTally> for TallyView {
    fn from(t: DisplayTally) -> Self {
        Self {
            total: t.total,
            active: t.active,
            online: t.online,
            warning: t.warning,
            uncertain: t.uncertain,
            offline: t.offline,
            suppressed: t.suppressed,
            suppressed_down: t.suppressed_down,
            paused: t.paused,
            alarming: t.alarming,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub live: bool,
    pub alarms: bool,
    pub tally: TallyView,
    pub hosts: Vec<HostView>,
}

pub async fn get(State(state): State<Arc<AppState>>) -> ApiResult<Json<Overview>> {
    let now = now_ms();
    let polls = state.polls.clone();
    let out = state
        .db
        .call(move |conn| build(conn, now, &polls))
        .await?;
    Ok(Json(out))
}

fn build(conn: &Connection, now: i64, polls: &PollCounters) -> anyhow::Result<Overview> {
    let settings = repo::load_settings(conn)?;
    let windows = repo::load_windows(conn)?;
    let hosts = repo::load_hosts(conn)?;

    // Senaste råa utfallet och tidsstämplar ligger i host_status.
    #[derive(Clone, Copy, Default)]
    struct Extra {
        latency_us: Option<u32>,
        changed_at: Option<i64>,
        checked_at: Option<i64>,
        /// Råutfallet som motorn räknade ut i senaste svepet. NULL bara
        /// på rader skrivna före schema v4 — de fångas av fallbacken nedan.
        raw: Option<RawStatus>,
    }

    let mut extra: HashMap<String, Extra> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT address, last_latency_us, changed_at, checked_at, raw FROM host_status",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Extra {
                    latency_us: r.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                    changed_at: r.get::<_, Option<i64>>(2)?,
                    checked_at: r.get::<_, Option<i64>>(3)?,
                    raw: r.get::<_, Option<String>>(4)?.map(|s| RawStatus::parse(&s)),
                },
            ))
        })?;
        for row in rows {
            let (addr, e) = row?;
            extra.insert(addr, e);
        }
    }

    // Råutfallet kommer från motorn — det är hon som vet om senaste
    // pingen svarade. Tidigare GISSADE översikten ur svarstiden, och
    // gissade fel i båda riktningarna: en missad ping har ingen svarstid
    // och såg ut som "uppe" tills grinden bekräftat NER (mellanläget
    // "osäker" visades aldrig), och en bekräftat nere enhet tvingades
    // till "offline" även när svaren kommit tillbaka ("återhämtar"
    // visades aldrig).
    //
    // Fallbacken gäller bara rader utan sparat råutfall: efter en
    // omstart, innan första svepet, saknas det — då visas enheten som
    // online tills motorn hunnit mäta. Alternativet vore att visa allt
    // som osäkert, vilket vore ärligare men skulle blinka i onödan vid
    // varje omstart.
    let hosts: Vec<Host> = hosts
        .into_iter()
        .map(|mut h| {
            if let Some(raw) = extra.get(&h.address).and_then(|e| e.raw) {
                h.raw = raw;
                return h;
            }
            let latency = extra.get(&h.address).and_then(|e| e.latency_us);
            let slow_us = h
                .slow_threshold_ms
                .unwrap_or(settings.slow_threshold_ms)
                .saturating_mul(1000);
            h.raw = match (h.confirmed, latency) {
                (Status::Down, _) => RawStatus::Offline,
                (_, Some(us)) if us >= slow_us => RawStatus::Warning,
                _ => RawStatus::Online,
            };
            h
        })
        .collect();

    // Sparkline. Ett bundet antal rader totalt, inte per enhet: en
    // ofiltrerad fråga över hela samples-tabellen växer med drifttiden
    // och skulle till slut göra översikten långsam.
    let mut spark: HashMap<String, Vec<u32>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT address, latency_us, online FROM samples ORDER BY id DESC LIMIT 3000",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u32,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        for row in rows {
            let (a, us, online) = row?;
            let v = spark.entry(a).or_default();
            if v.len() < 40 {
                v.push(if online { us } else { 0 });
            }
        }
        // Frågan gick nyast först; grafen ritas äldst först.
        for v in spark.values_mut() {
            v.reverse();
        }
    }

    let by_address: HashMap<String, Host> =
        hosts.iter().map(|h| (h.address.clone(), h.clone())).collect();
    let lookup = |addr: &str| by_address.get(addr).cloned();

    let reason_of = |h: &Host| suppression_reason(h, now, &windows, &lookup);
    let tally = tally_hosts(&hosts, &reason_of);

    let views = hosts
        .iter()
        .map(|h| {
            let reason = reason_of(h);
            let e = extra.get(&h.address).copied().unwrap_or_default();
            HostView {
                id: h.id,
                name: h.name.clone(),
                address: h.address.clone(),
                group_id: h.group_id,
                group: h.group.clone(),
                note: h.note.clone(),
                enabled: h.enabled,
                confirmed: h.confirmed,
                display: display_status_of(h, reason),
                reason,
                latency_us: e.latency_us,
                changed_at: e.changed_at,
                checked_at: e.checked_at,
                polls: polls.get(&h.address).0,
                polls_ok: polls.get(&h.address).1,
                spark: spark.get(&h.address).cloned().unwrap_or_default(),
                snooze_until: h.snooze_until,
                depends_on_address: h.depends_on_address.clone(),
            }
        })
        .collect();

    Ok(Overview {
        live: settings.live,
        alarms: settings.alarms,
        tally: tally.into(),
        hosts: views,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::polls::PollCounters;

    #[test]
    fn oversikten_visar_bara_processens_pollningar() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            concat!(
                include_str!("../schema.sql"),
                "\nALTER TABLE hosts ADD COLUMN probe_type TEXT;",
                "\nALTER TABLE hosts ADD COLUMN probe_port INTEGER;",
                "\nALTER TABLE hosts ADD COLUMN sms_enabled INTEGER NOT NULL DEFAULT 1;",
                "\nALTER TABLE host_status ADD COLUMN raw TEXT;"
            )
        )
        .unwrap();
        conn.execute(
            "INSERT INTO hosts (name,address,enabled,created_at,updated_at) VALUES ('Router','10.0.0.1',1,1,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO samples (address,ts,online,latency_us) VALUES ('10.0.0.1',1,1,1000)",
            [],
        )
        .unwrap();

        let counters = PollCounters::default();
        let after_restart = build(&conn, 2, &counters).unwrap();
        assert_eq!(after_restart.hosts[0].polls, 0);
        assert_eq!(after_restart.hosts[0].polls_ok, 0);

        counters.record("10.0.0.1", true);
        counters.record("10.0.0.1", false);
        let current_process = build(&conn, 3, &counters).unwrap();
        assert_eq!(current_process.hosts[0].polls, 2);
        assert_eq!(current_process.hosts[0].polls_ok, 1);
    }
}
