// =====================================================================
// engine/repo.rs
// Databasåtkomst för motorn.
//
// Alla funktioner tar &Connection och är blockerande. De anropas genom
// Db::call, som lägger dem på en blockerande tråd så att tokios
// arbetartrådar aldrig stannar.
// =====================================================================

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;

use super::flap::{HysteresisConfig, FLAP_FAIL_SEC_DEFAULT, FLAP_SUCCESS_SEC_DEFAULT};
use super::suppression::{MaintenanceWindow, Schedule, Target};
use super::types::{Host, ProbeType, RawStatus, Status};

// ---- Globala inställningar -------------------------------------------

/// Motsvarar det som ligger i localStorage på desktopvarianten.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Bevakning på. Av betyder att inga svep körs alls.
    pub live: bool,
    /// Larm på. Av betyder att svepen fortsätter men inget levereras.
    pub alarms: bool,
    pub sweep_interval_sec: u64,
    pub fail_period_sec: u32,
    pub success_period_sec: u32,
    pub packet_size: u32,
    pub slow_threshold_ms: u32,
    pub ping_timeout_sec: u64,
    /// Latenslarm på (etapp 8). Av som standard — ett larm ska vara ett
    /// aktivt val, samma princip som vakthundens port.
    pub slow_alarm: bool,
    /// Sammanhängande tid med svarstider över tröskeln innan larm.
    pub slow_alarm_sec: u32,
    /// Kanaler som larm ska köas till, kommaseparerat.
    pub channels: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            live: true,
            alarms: true,
            sweep_interval_sec: 10,
            fail_period_sec: FLAP_FAIL_SEC_DEFAULT,
            success_period_sec: FLAP_SUCCESS_SEC_DEFAULT,
            packet_size: 32,
            slow_threshold_ms: 300,
            ping_timeout_sec: 2,
            slow_alarm: false,
            slow_alarm_sec: 60,
            channels: Vec::new(),
        }
    }
}

fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    let v = conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    Ok(v)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn load_settings(conn: &Connection) -> Result<Settings> {
    let d = Settings::default();
    let b = |k: &str, fallback: bool| -> bool {
        get_setting(conn, k)
            .ok()
            .flatten()
            .map(|v| v == "true" || v == "1")
            .unwrap_or(fallback)
    };
    let n = |k: &str, fallback: u64| -> u64 {
        get_setting(conn, k)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(fallback)
    };

    Ok(Settings {
        live: b("live", d.live),
        alarms: b("alarms", d.alarms),
        // Svep under 2 sekunder är nästan alltid ett misstag och kan
        // dränka både nätet och databasen.
        sweep_interval_sec: n("sweepIntervalSec", d.sweep_interval_sec).clamp(2, 3600),
        fail_period_sec: n("flapFailSec", d.fail_period_sec as u64) as u32,
        success_period_sec: n("flapSuccessSec", d.success_period_sec as u64) as u32,
        packet_size: n("packetSize", d.packet_size as u64).clamp(16, 65_500) as u32,
        slow_threshold_ms: n("slowThresholdMs", d.slow_threshold_ms as u64) as u32,
        ping_timeout_sec: n("pingTimeoutSec", d.ping_timeout_sec).clamp(1, 30),
        slow_alarm: b("slowAlarm", d.slow_alarm),
        // Under 5 sekunder är latenslarm bara ett dyrare sätt att
        // upptäcka normal jitter — det är NER-grindens jobb.
        slow_alarm_sec: n("slowAlarmSec", d.slow_alarm_sec as u64).clamp(5, 3600) as u32,
        channels: get_setting(conn, "channels")
            .ok()
            .flatten()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
    })
}

// ---- Enheter ---------------------------------------------------------

pub fn load_hosts(conn: &Connection) -> Result<Vec<Host>> {
    let mut stmt = conn.prepare(
        "SELECT h.id, h.name, h.address, h.group_id, g.name, h.note, h.enabled,
                h.snooze_until, h.depends_on_address,
                h.interval_sec, h.packet_size, h.slow_threshold_ms,
                h.fail_period_sec, h.success_period_sec,
                COALESCE(s.status, 'unknown'),
                COALESCE(h.probe_type, 'icmp'), h.probe_port,
                COALESCE(h.sms_enabled, 1)
         FROM hosts h
         LEFT JOIN host_status s ON s.address = h.address
         LEFT JOIN groups g ON g.id = h.group_id
         ORDER BY h.name",
    )?;

    let rows = stmt.query_map([], |r| {
        Ok(Host {
            id: r.get(0)?,
            name: r.get(1)?,
            address: r.get(2)?,
            group_id: r.get(3)?,
            group: r.get(4)?,
            note: r.get(5)?,
            enabled: r.get::<_, i64>(6)? != 0,
            snooze_until: r.get(7)?,
            depends_on_address: r.get(8)?,
            interval_sec: r.get::<_, Option<i64>>(9)?.map(|v| v as u32),
            packet_size: r.get::<_, Option<i64>>(10)?.map(|v| v as u32),
            slow_threshold_ms: r.get::<_, Option<i64>>(11)?.map(|v| v as u32),
            fail_period_sec: r.get::<_, Option<i64>>(12)?.map(|v| v as u32),
            success_period_sec: r.get::<_, Option<i64>>(13)?.map(|v| v as u32),
            confirmed: Status::parse(&r.get::<_, String>(14)?),
            // Rått utfall är inte persisterat — det säger bara vad som
            // hände nyss och har ingen mening efter en omstart.
            raw: RawStatus::Online,
            probe_type: ProbeType::parse(&r.get::<_, String>(15)?),
            probe_port: r.get::<_, Option<i64>>(16)?.map(|v| v as u16),
            sms_enabled: r.get::<_, i64>(17)? != 0,
        })
    })?;

    let mut out = Vec::new();
    for h in rows {
        out.push(h?);
    }
    Ok(out)
}

/// Trösklar för en enhet: override om satt, annars globalt.
pub fn hysteresis_for(host: &Host, s: &Settings) -> HysteresisConfig {
    HysteresisConfig {
        fail_period_sec: host.fail_period_sec.unwrap_or(s.fail_period_sec),
        success_period_sec: host.success_period_sec.unwrap_or(s.success_period_sec),
    }
}

// ---- Cykelskydd ------------------------------------------------------

/// Kontrollera att ett beroende inte skapar en cykel.
///
/// Utan skyddet kan A bero på B och B på A. Blir båda nere undertrycker
/// de varandra och INGET larm går ut — en tyst dubbel nedgång är det
/// värsta tänkbara utfallet för en nattvaktskonsol.
///
/// Motorn själv är säker: suppression_reason tittar bara på närmaste
/// förälder och kan inte hamna i oändlig rekursion. Men den skulle tysta
/// båda, så cykeln måste stoppas när den skrivs — inte hanteras när den
/// läses.
pub fn validate_dependency(
    conn: &Connection,
    host_id: i64,
    depends_on: Option<&str>,
    lang: crate::i18n::Lang,
) -> Result<()> {
    let Some(target) = depends_on else {
        return Ok(());
    };

    let own_address: String =
        conn.query_row("SELECT address FROM hosts WHERE id = ?1", [host_id], |r| {
            r.get(0)
        })?;

    if own_address == target {
        bail!(crate::i18n::dep_self(lang));
    }

    // Vandra uppåt från målet. Når vi tillbaka till oss själva finns en
    // cykel. Taket skyddar mot en redan trasig kedja i databasen.
    let mut current = target.to_string();
    for _ in 0..64 {
        let parent: Option<String> = conn
            .query_row(
                "SELECT depends_on_address FROM hosts WHERE address = ?1",
                [&current],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let Some(p) = parent else { return Ok(()) };
        if p == own_address {
            bail!(crate::i18n::dep_cycle(lang));
        }
        current = p;
    }

    bail!(crate::i18n::dep_too_deep(lang))
}

// ---- Status ----------------------------------------------------------

/// Bekräftad status plus den senast RAPPORTERADE statusen.
///
/// De två skiljer sig när ett larm hållits tillbaka: grinden har
/// bekräftat en övergång men undertryckning eller avslagna larm har
/// hindrat leveransen. Genom att jämföra dem varje svep levereras larmet
/// så snart hindret upphör — och en enhet som hunnit gå ner och upp igen
/// under tystnaden ger inget larm alls, eftersom rapporterat läge då
/// stämmer med bekräftat.
#[derive(Debug, Clone, Copy)]
pub struct StoredStatus {
    pub status: Status,
    pub reported: Status,
}

pub fn load_status(conn: &Connection) -> Result<HashMap<String, StoredStatus>> {
    let mut stmt = conn.prepare(
        "SELECT address, status, COALESCE(reported_status, status) FROM host_status",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            StoredStatus {
                status: Status::parse(&r.get::<_, String>(1)?),
                reported: Status::parse(&r.get::<_, String>(2)?),
            },
        ))
    })?;

    let mut out = HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}

/// Skriv status efter ett svep — fullständig rad, inklusive råutfallet.
///
/// changed_at flyttas ENDAST när statusen faktiskt ändras. Skrivningen
/// sker varje svep, så ett fält som uppdaterades varje gång hade svarat
/// på "när skrev vi senast" i stället för "hur länge har det varit så
/// här" — och det senare är vad en operatör frågar sig.
///
/// checked_at flyttas alltid, och visar hur färska uppgifterna är.
pub fn save_status(
    conn: &Connection,
    address: &str,
    status: Status,
    reported: Status,
    raw: RawStatus,
    now_ms: i64,
    latency_us: Option<u32>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO host_status
            (address, status, reported_status, raw, changed_at, checked_at, last_latency_us)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
         ON CONFLICT(address) DO UPDATE SET
             status          = excluded.status,
             reported_status = excluded.reported_status,
             raw             = excluded.raw,
             checked_at      = excluded.checked_at,
             last_latency_us = excluded.last_latency_us,
             changed_at      = CASE
                                   WHEN host_status.status <> excluded.status
                                   THEN excluded.changed_at
                                   ELSE host_status.changed_at
                               END",
        params![
            address,
            status.as_str(),
            reported.as_str(),
            raw.as_str(),
            now_ms,
            latency_us.map(|v| v as i64)
        ],
    )?;
    Ok(())
}

/// Flytta bekräftat/rapporterat läge UTAN att röra mätningarna.
///
/// Larmgrinden och de tysta framflyttningarna anropar den här efter att
/// svepets fulla skrivning redan skett. Gör de en full skrivning i
/// stället raderar de senaste svarstiden och råutfallet precis när ett
/// larm går ut — och översikten tappar underlaget för "återhämtar".
pub fn save_transition(
    conn: &Connection,
    address: &str,
    status: Status,
    reported: Status,
    now_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO host_status (address, status, reported_status, changed_at, checked_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(address) DO UPDATE SET
             status          = excluded.status,
             reported_status = excluded.reported_status,
             checked_at      = excluded.checked_at,
             changed_at      = CASE
                                   WHEN host_status.status <> excluded.status
                                   THEN excluded.changed_at
                                   ELSE host_status.changed_at
                               END",
        params![address, status.as_str(), reported.as_str(), now_ms],
    )?;
    Ok(())
}

// ---- Mätvärden, logg och kö ------------------------------------------

/// Ett komplett pollresultat som ska skrivas i en transaktion.
pub struct PollRecord<'a> {
    pub address: &'a str,
    pub ts: i64,
    pub online: bool,
    pub latency_us: Option<u32>,
    pub status: Status,
    pub reported: Status,
    pub raw: RawStatus,
}

/// Spara sample och aktuell host-status atomiskt för samma poll.
pub fn record_poll(conn: &Connection, poll: PollRecord<'_>) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    record_sample(&tx, poll.address, poll.ts, poll.online, poll.latency_us)?;
    save_status(
        &tx,
        poll.address,
        poll.status,
        poll.reported,
        poll.raw,
        poll.ts,
        poll.latency_us,
    )?;
    tx.commit()?;
    Ok(())
}

pub fn record_sample(
    conn: &Connection,
    address: &str,
    ts: i64,
    online: bool,
    latency_us: Option<u32>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO samples (address, ts, online, latency_us) VALUES (?1, ?2, ?3, ?4)",
        params![address, ts, online as i64, latency_us.map(|v| v as i64)],
    )?;
    Ok(())
}

pub fn log_event(conn: &Connection, ts: i64, level: &str, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO events (ts, level, text) VALUES (?1, ?2, ?3)",
        params![ts, level, text],
    )?;
    Ok(())
}

pub fn enqueue_delivery(
    conn: &Connection,
    channel: &str,
    device: &str,
    event: &str,
    payload: &str,
    now_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO deliveries
            (channel, device, event, payload, status, attempts, next_attempt, created_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, ?5)",
        params![channel, device, event, payload, now_ms],
    )?;
    Ok(())
}

// ---- Underhållsfönster -----------------------------------------------

pub fn load_windows(conn: &Connection) -> Result<Vec<MaintenanceWindow>> {
    let mut stmt = conn.prepare(
        "SELECT id, enabled, kind, starts_at, ends_at,
                start_min, duration_min, days,
                target_kind, target_group_id
         FROM maintenance_windows",
    )?;

    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let enabled: bool = r.get::<_, i64>(1)? != 0;
        let kind: String = r.get(2)?;

        let schedule = if kind == "daily" {
            let days_raw: Option<String> = r.get(7)?;
            let days = days_raw
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse::<u8>().ok())
                .collect();
            Schedule::Daily {
                start_min: r.get::<_, Option<i64>>(5)?.unwrap_or(0) as u32,
                duration_min: r.get::<_, Option<i64>>(6)?.unwrap_or(0) as u32,
                days,
            }
        } else {
            Schedule::Once {
                start: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                end: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            }
        };

        let target_kind: String = r.get(8)?;
        let group_id: Option<i64> = r.get(9)?;

        Ok((id, enabled, schedule, target_kind, group_id))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, enabled, schedule, target_kind, group_id) = row?;

        let target = match target_kind.as_str() {
            "group" => Target::Group(group_id.unwrap_or(-1)),
            "hosts" => {
                let mut s =
                    conn.prepare("SELECT host_id FROM maintenance_targets WHERE window_id = ?1")?;
                let ids = s
                    .query_map([id], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Target::Hosts(ids)
            }
            _ => Target::All,
        };

        out.push(MaintenanceWindow {
            id,
            enabled,
            schedule,
            target,
        });
    }
    Ok(out)
}

// ---- Grupper ---------------------------------------------------------

/// Slå upp en grupp på namn, eller skapa den.
///
/// Gränssnittet arbetar med gruppnamn som fri text, precis som
/// desktopvarianten. Databasen håller ändå en egen tabell, så att
/// underhållsfönster mot grupp och borttagning med CASCADE fungerar.
pub fn find_or_create_group(conn: &Connection, name: &str) -> Result<Option<i64>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if let Some(id) = conn
        .query_row("SELECT id FROM groups WHERE name = ?1", [name], |r| r.get(0))
        .optional()?
    {
        return Ok(Some(id));
    }
    conn.execute("INSERT INTO groups (name) VALUES (?1)", [name])?;
    Ok(Some(conn.last_insert_rowid()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_poll_rullar_tillbaka_sample_om_statusskrivningen_misslyckas() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (
                 id INTEGER PRIMARY KEY,
                 address TEXT NOT NULL,
                 ts INTEGER NOT NULL,
                 online INTEGER NOT NULL,
                 latency_us INTEGER
             );
             CREATE TABLE host_status (
                 address TEXT PRIMARY KEY,
                 status TEXT NOT NULL CHECK(status = 'omojligt'),
                 reported_status TEXT NOT NULL,
                 raw TEXT NOT NULL,
                 changed_at INTEGER NOT NULL,
                 checked_at INTEGER NOT NULL,
                 last_latency_us INTEGER
             );",
        )
        .unwrap();

        let result = record_poll(
            &conn,
            PollRecord {
                address: "10.0.0.1",
                ts: 1_000,
                online: true,
                latency_us: Some(123),
                status: Status::Up,
                reported: Status::Up,
                raw: RawStatus::Online,
            },
        );

        assert!(result.is_err());
        let samples: i64 = conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(samples, 0);
    }
}
