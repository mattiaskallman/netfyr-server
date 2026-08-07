// =====================================================================
// watchdog.rs
// TCP-lyssnare som signalerar att NetFyr faktiskt bevakar.
//
// Port av desktopens heartbeat.rs plus styrlogiken i App.tsx /
// heartbeat.ts. En extern vakthund — t.ex. en Teltonika-gateway med
// Event Juggler — ansluter till porten och ser därmed om bevakningen
// är igång. Ping räcker inte: maskinen svarar på ICMP även när
// bevakningen är avstängd, vilket är precis det fall en nattvakt
// behöver upptäcka.
//
// Lyssnaren svarar med en kort rad och stänger direkt — samma banner
// som desktop ("NetFyr watching\n"), så befintliga vakthundsskript
// fungerar oförändrade. Det gör testet användbart även med magra
// verktyg: BusyBox nc har varken -z eller timeout-flagga och hänger
// annars tills anslutningen bryts. Får den data avslutar den, och
// exitkoden blir meningsfull.
//
// PORTEN ÄR UPPE ENDAST NÄR BEVAKNING PÅGÅR — inte bara när servern
// kör. En pausad server övervakar ingenting, och det ska synas
// utifrån.
//
// RESPIT VID PAUS: underhållsfönster i NetFyr är per enhet och ger
// inget globalt "vi arbetar nu"-läge att undanta. I stället hålls
// porten uppe en stund efter att bevakningen pausats (inställningen
// watchdogGraceMin, desktop: graceMin). Planerat arbete hinner bli
// klart utan larm, men glöms bevakningen avstängd kommer larmet ändå.
// Noll stänger porten direkt.
//
// Respit-räkningen lever i minnet, som på desktop: startas servern om
// medan bevakningen är pausad börjar respiten om. Det är acceptabelt —
// det värsta som händer är att larmet dröjer ytterligare en respit.
// =====================================================================

use crate::db::Db;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Svaret till den som ansluter. Samma som desktop, så att en vakthund
/// som provar mot konsolen kan pekas om mot servern utan ändring.
const BANNER: &str = "NetFyr watching\n";

/// Hur ofta styrslingan läser om inställningarna. Två sekunder är
/// tillräckligt snabbt för att en ändring i UI:t ska kännas direkt,
/// och tillräckligt sällan för att inte märkas i databasen.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// En klient som ansluter men inte läser ska inte kunna hålla
/// servern upptagen.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

// ---- Inställningar ---------------------------------------------------

/// De tre vakthundsnycklarna plus motorns `live`, lästa rått ur
/// nyckel/värde-tabellen. Samma gränser och standardvärden som
/// desktop (heartbeat.ts).
#[derive(Debug, Clone, PartialEq)]
struct WatchdogConfig {
    enabled: bool,
    port: u16,
    grace_min: u32,
    live: bool,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7999,
            grace_min: 30,
            // Motorns standard är bevakning på (repo::Settings::default).
            live: true,
        }
    }
}

fn parse_u32(raw: Option<&String>, fallback: u32, min: u32, max: u32) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .map(|v| v.clamp(min, max))
        .unwrap_or(fallback)
}

fn load_config(map: &std::collections::HashMap<String, String>) -> WatchdogConfig {
    let d = WatchdogConfig::default();
    WatchdogConfig {
        enabled: map
            .get("watchdogEnabled")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(d.enabled),
        // Under 1024 kräver förhöjda rättigheter på de flesta system.
        port: parse_u32(map.get("watchdogPort"), d.port as u32, 1024, 65535) as u16,
        grace_min: parse_u32(map.get("watchdogGraceMin"), d.grace_min, 0, 1440),
        live: map
            .get("live")
            .map(|v| v != "0" && v != "false")
            .unwrap_or(d.live),
    }
}

// ---- Lyssnaren -------------------------------------------------------

struct Running {
    port: u16,
    task: JoinHandle<()>,
}

/// Svara och stäng. En uppgift per anslutning — en klient som hänger
/// får aldrig blockera nästa vakthund som provar.
async fn serve(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((mut socket, _peer)) => {
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(WRITE_TIMEOUT, async {
                        socket.write_all(BANNER.as_bytes()).await
                    })
                    .await;
                    // Socketen stängs när den tas bort här.
                });
            }
            Err(e) => {
                // Ett accept-fel är oftast övergående (t.ex. filgräns
                // tillfälligt nådd). Logga och andas innan nästa försök
                // så att slingan inte spinner.
                tracing::warn!("vakthund: accept misslyckades: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Se till att lyssnaren kör på rätt port. Idempotent: körs den redan
/// på samma port händer ingenting. Har porten ändrats stoppas den gamla
/// först. Misslyckas bindningen loggas det — men bara när felet ändras,
/// så att loggen inte fylls av samma rad varannan sekund.
fn ensure_running(current: &mut Option<Running>, port: u16, last_bind_err: &mut Option<String>) {
    if let Some(r) = current.as_ref() {
        if r.port == port && !r.task.is_finished() {
            last_bind_err.take();
            return;
        }
        stop(current);
    }

    match std::net::TcpListener::bind(("0.0.0.0", port)) {
        Ok(std_listener) => {
            if let Err(e) = std_listener.set_nonblocking(true) {
                tracing::warn!("vakthund: kunde inte sätta port {port} icke-blockerande: {e}");
                return;
            }
            match TcpListener::from_std(std_listener) {
                Ok(listener) => {
                    *current = Some(Running {
                        port,
                        task: tokio::spawn(serve(listener)),
                    });
                    tracing::info!("vakthund: lyssnar på port {port}");
                    last_bind_err.take();
                }
                Err(e) => {
                    let msg = format!("{e}");
                    if last_bind_err.as_deref() != Some(&msg) {
                        tracing::warn!("vakthund: kunde inte lyssna på port {port}: {e}");
                        *last_bind_err = Some(msg);
                    }
                }
            }
        }
        Err(e) => {
            // Vanligaste orsaken är att porten redan är upptagen.
            let msg = format!("{e}");
            if last_bind_err.as_deref() != Some(&msg) {
                tracing::warn!("vakthund: kunde inte lyssna på port {port}: {e}");
                *last_bind_err = Some(msg);
            }
        }
    }
}

/// Stoppa lyssnaren. Att avbryta accept-loopen och släppa socketen
/// räcker — tokio befriar porten direkt, till skillnad från desktopens
/// std-lyssnare som måste väckas ur accept().
fn stop(current: &mut Option<Running>) {
    if let Some(r) = current.take() {
        r.task.abort();
        tracing::info!("vakthund: port {} stängd", r.port);
    }
}

// ---- Styrslingan ------------------------------------------------------

/// Körs som egen task hela processens liv. Läser inställningarna ur
/// databasen varannan sekund och håller lyssnaren i rätt läge:
///
///   vakthund av          -> porten stängd
///   bevakning på (live)  -> porten öppen
///   bevakning pausad     -> porten öppen under respiten, sedan stängd
pub async fn run(db: Db) {
    let mut current: Option<Running> = None;
    let mut paused_since: Option<Instant> = None;
    let mut last_bind_err: Option<String> = None;

    let mut interval = tokio::time::interval(POLL_INTERVAL);
    loop {
        interval.tick().await;

        let cfg = db
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT key, value FROM settings
                     WHERE key IN ('live', 'watchdogEnabled', 'watchdogPort', 'watchdogGraceMin')",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                let mut map = std::collections::HashMap::new();
                for row in rows {
                    let (k, v) = row?;
                    map.insert(k, v);
                }
                Ok(load_config(&map))
            })
            .await;

        let cfg = match cfg {
            Ok(c) => c,
            Err(e) => {
                // Databasen ska inte halka, men om den gör det ändrar
                // vi inget — det senaste kända läget är ett bättre
                // svar utåt än att stänga porten i blindo.
                tracing::warn!("vakthund: kunde inte läsa inställningarna: {e:#}");
                continue;
            }
        };

        if !cfg.enabled {
            stop(&mut current);
            paused_since = None;
            continue;
        }

        if cfg.live {
            // Bevakning pågår: porten ska vara uppe, och en paus under
            // respiten räknas som avslutad.
            paused_since = None;
            ensure_running(&mut current, cfg.port, &mut last_bind_err);
            continue;
        }

        // Bevakningen är pausad. Håll porten uppe under respiten så att
        // planerat arbete inte larmar — löper den ut går porten ner, och
        // den som glömt slå på bevakningen igen får veta det.
        let since = *paused_since.get_or_insert_with(Instant::now);
        let grace = Duration::from_secs(u64::from(cfg.grace_min) * 60);
        if cfg.grace_min == 0 || since.elapsed() >= grace {
            if current.is_some() {
                tracing::warn!("vakthund: bevakning fortfarande pausad, port stängs");
            }
            stop(&mut current);
        } else {
            ensure_running(&mut current, cfg.port, &mut last_bind_err);
        }
    }
}
