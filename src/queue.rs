// =====================================================================
// queue.rs
// Leveranskön.
//
// Larm som ska ut på en kanal ligger i tabellen deliveries. Den här
// tasken plockar pending-rader vars nästa försök förfallit, skickar
// dem, och skriver tillbaka resultatet.
//
// KÖN ÖVERLEVER OMSTART. Det är hela poängen: ett larm får inte
// försvinna för att tjänsten startades om mitt i ett utskick. Därför
// ligger tillståndet i databasen och inte i minnet.
//
// Misslyckade utskick görs om med växande fördröjning tills de lyckas
// eller passerar GIVEUP. Att ge upp är också ett beslut som ska synas —
// raden markeras failed och loggas, den försvinner inte tyst.
// =====================================================================

use anyhow::Result;
use rusqlite::{Connection, params};
use std::time::Duration;

use crate::channels;
use crate::db::Db;
use crate::secrets::Secrets;

/// Hur ofta kön gås igenom.
const TICK: Duration = Duration::from_secs(5);

/// Efter så här länge ges försöken upp. Ett larm som inte gått fram på
/// sex timmar kommer inte fram, och en kö som växer i evighet är värre
/// än ett erkänt misslyckande.
const GIVEUP_MS: i64 = 6 * 60 * 60 * 1000;

/// Hur många rader som behandlas per varv. Taket hindrar att en lång kö
/// blockerar allt annat efter ett längre avbrott.
const BATCH: usize = 20;

#[derive(Debug)]
struct Delivery {
    id: i64,
    channel: String,
    device: String,
    event: String,
    payload: String,
    attempts: i64,
    created_at: i64,
}

/// Växande fördröjning: 5 s, 30 s, 2 min, 10 min (tak).
///
/// Samma trappa som desktopvarianten. Snabbt nog för ett tillfälligt
/// nätavbrott, långsamt nog för att inte hamra på en trasig mottagare.
fn next_delay_ms(attempts: i64) -> i64 {
    match attempts {
        0 | 1 => 5_000,
        2 => 30_000,
        3 => 120_000,
        _ => 600_000,
    }
}

pub struct Queue {
    db: Db,
    secrets: Secrets,
}

impl Queue {
    pub fn new(db: Db, secrets: Secrets) -> Self {
        Self { db, secrets }
    }

    pub async fn run(self) {
        loop {
            if let Err(e) = self.tick().await {
                tracing::error!("leveranskön: {e:#}");
            }
            tokio::time::sleep(TICK).await;
        }
    }

    async fn tick(&self) -> Result<()> {
        let now = now_ms();
        // Språket läses en gång per varv — en ändring i gränssnittet
        // gäller direkt, utan omstart.
        let lang = self.db.call(|conn| Ok(crate::i18n::load(conn))).await?;
        let pending = self.db.call(move |conn| fetch_pending(conn, now)).await?;

        for d in pending {
            let config = self
                .db
                .call({
                    let ch = d.channel.clone();
                    move |conn| channel_config(conn, &ch)
                })
                .await?;

            let result = channels::send(
                &d.channel,
                &d.payload,
                &config,
                &self.secrets,
                &self.db,
                lang,
            )
            .await;

            let now = now_ms();
            match result {
                channels::SendOutcome::Delivered => {
                    tracing::info!("levererat → {} ({})", d.channel, d.device);
                    let text = crate::i18n::ev_delivered(lang, &d.channel, &d.device);
                    self.db
                        .call(move |conn| {
                            mark_sent(conn, d.id, now)?;
                            log_event(conn, now, "ok", &text)?;
                            Ok(())
                        })
                        .await?;
                }
                channels::SendOutcome::Terminal(e) => {
                    let msg = format!("{e:#}");
                    let attempts = d.attempts + 1;
                    tracing::error!(
                        "terminalt delresultat → {} ({}) efter {} försök: {msg}",
                        d.channel,
                        d.device,
                        attempts
                    );
                    let text =
                        crate::i18n::ev_gave_up(lang, &d.channel, &d.device, attempts as u32, &msg);
                    self.db
                        .call(move |conn| {
                            mark_failed(conn, d.id, attempts, &msg)?;
                            log_event(conn, now, "warn", &text)?;
                            Ok(())
                        })
                        .await?;
                }
                channels::SendOutcome::Retryable(e) => {
                    let msg = format!("{e:#}");
                    let age = now - d.created_at;
                    let attempts = d.attempts + 1;

                    if age >= GIVEUP_MS {
                        tracing::error!(
                            "ger upp → {} ({}) efter {} försök: {msg}",
                            d.channel,
                            d.device,
                            attempts
                        );
                        let text = crate::i18n::ev_gave_up(
                            lang,
                            &d.channel,
                            &d.device,
                            attempts as u32,
                            &msg,
                        );
                        self.db
                            .call(move |conn| {
                                mark_failed(conn, d.id, attempts, &msg)?;
                                log_event(conn, now, "warn", &text)?;
                                Ok(())
                            })
                            .await?;
                    } else {
                        let delay = next_delay_ms(attempts);
                        tracing::warn!(
                            "misslyckades → {} ({}), försök {} om {} s: {msg}",
                            d.channel,
                            d.device,
                            attempts,
                            delay / 1000
                        );
                        self.db
                            .call(move |conn| reschedule(conn, d.id, attempts, now + delay, &msg))
                            .await?;
                    }
                }
            }
        }

        Ok(())
    }
}

// ---- Databas ---------------------------------------------------------

fn fetch_pending(conn: &Connection, now: i64) -> Result<Vec<Delivery>> {
    let mut stmt = conn.prepare(
        "SELECT id, channel, device, event, payload, attempts, created_at
         FROM deliveries
         WHERE status = 'pending' AND next_attempt <= ?1
         ORDER BY created_at
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![now, BATCH as i64], |r| {
        Ok(Delivery {
            id: r.get(0)?,
            channel: r.get(1)?,
            device: r.get(2)?,
            event: r.get(3)?,
            payload: r.get(4)?,
            attempts: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Kanalens konfiguration ur settings, som JSON-sträng.
///
/// Saknas den returneras ett tomt objekt. Kanalen får då själv avgöra
/// om den kan arbeta utan konfiguration — webhook kan, SMTP kan inte.
fn channel_config(conn: &Connection, channel: &str) -> Result<String> {
    use rusqlite::OptionalExtension;
    let key = format!("channel.{channel}.config");
    let v: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [&key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(v.unwrap_or_else(|| "{}".to_string()))
}

fn mark_sent(conn: &Connection, id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE deliveries SET status = 'sent', sent_at = ?2, last_error = NULL WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

fn mark_failed(conn: &Connection, id: i64, attempts: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE deliveries SET status = 'failed', attempts = ?2, last_error = ?3 WHERE id = ?1",
        params![id, attempts, error],
    )?;
    Ok(())
}

fn reschedule(conn: &Connection, id: i64, attempts: i64, next: i64, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE deliveries SET attempts = ?2, next_attempt = ?3, last_error = ?4 WHERE id = ?1",
        params![id, attempts, next, error],
    )?;
    Ok(())
}

fn log_event(conn: &Connection, ts: i64, level: &str, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO events (ts, level, text) VALUES (?1, ?2, ?3)",
        params![ts, level, text],
    )?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
