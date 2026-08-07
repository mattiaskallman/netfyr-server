//! # Retention — automatisk rensning av historisk data
//!
//! Port av desktopens städrutin (`purgeOlderThan` / `pruneDeliveries` /
//! `pruneEvents` i `db.ts`, schemalagd i `App.tsx` vid start + varje dygn).
//!
//! ## Varför
//!
//! Utan rensning växer `samples` obegränsat — tabellen fylls av varje
//! ping mot varje enhet, dygnet runt. Statistikfönstren i UI:t sträcker
//! sig som längst 180 dagar tillbaka, så äldre data är oanvändbart men
//! kostar ändå disk och söktid. Att lagringen är tidsbegränsad är dessutom
//! ett krav i sig (dataminimering, GDPR).
//!
//! ## Reglerna (samma som desktop)
//!
//! | Tabell       | Behålls | Undantag                                  |
//! |--------------|---------|-------------------------------------------|
//! | `samples`    | 180 dgr | —                                         |
//! | `deliveries` | 30 dgr  | bara rader med status `sent`/`failed` —   |
//! |              |         | `pending` rörs aldrig, kön äger dem       |
//! | `events`     | 7 dgr   | —                                         |
//!
//! `audit_log` hanteras av authens egen janitor (395 dagar) och rörs
//! inte här — revisionsspåret ska leva längre än driftdata.
//!
//! ## När
//!
//! Första ticken i `tokio::time::interval` avfyras direkt, så städningen
//! körs vid uppstart och därefter en gång per dygn — exakt som desktop.

use crate::api::now_ms;
use crate::db::Db;

/// Mätdata (`samples`) behålls 180 dagar — längsta statistikfönstret.
const SAMPLES_RETENTION_DAYS: i64 = 180;
/// Avslutade larmleveranser (`deliveries`) behålls 30 dagar.
const DELIVERIES_RETENTION_DAYS: i64 = 30;
/// Händelseloggen (`events`, Terminalens historik) behålls 7 dagar.
const EVENTS_RETENTION_DAYS: i64 = 7;

const DAY_MS: i64 = 86_400_000;

/// Städloopen. Körs som egen task hela processens liv.
pub async fn run(db: Db) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(DAY_MS as u64));
    loop {
        interval.tick().await;
        let now = now_ms();
        let res = db
            .call(move |conn| {
                let samples = conn.execute(
                    "DELETE FROM samples WHERE ts < ?1",
                    [now - SAMPLES_RETENTION_DAYS * DAY_MS],
                )?;
                let deliveries = conn.execute(
                    "DELETE FROM deliveries
                     WHERE status IN ('sent', 'failed') AND created_at < ?1",
                    [now - DELIVERIES_RETENTION_DAYS * DAY_MS],
                )?;
                let events = conn.execute(
                    "DELETE FROM events WHERE ts < ?1",
                    [now - EVENTS_RETENTION_DAYS * DAY_MS],
                )?;
                Ok((samples, deliveries, events))
            })
            .await;
        match res {
            Ok((0, 0, 0)) => {}
            Ok((s, d, e)) => tracing::info!(
                "retention-städning: {s} mätningar, {d} leveranser, {e} händelser borttagna"
            ),
            Err(e) => tracing::warn!("retention-städningen misslyckades: {e:#}"),
        }
    }
}
