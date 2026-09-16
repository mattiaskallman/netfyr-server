// =====================================================================
// engine/monitor.rs
// Svepslingan — den som binder ihop allt.
//
// PER-ENHET INTERVALL. Varje enhet har egen förfallotid. En kärnswitch
// kan behöva kontrolleras var femte sekund medan en skrivare räcker med
// varje minut — ett gemensamt intervall tvingar fram det snabbaste för
// alla och belastar både nät och databas i onödan.
//
// Slingan tickar därför en gång i sekunden och plockar de enheter vars
// tid gått ut, i stället för att svepa allt på en gång. Inställningar
// och enheter läses om varje tick, vilket kostar några mikrosekunder
// mot SQLite och gör att en ändring i gränssnittet får effekt inom en
// sekund utan omstart.
//
// Varje omgång:
//   1. läs inställningar, enheter och underhållsfönster
//   2. pinga alla påslagna enheter parallellt
//   3. mata utfallen genom flap-grinden
//   4. avgör undertryckning
//   5. köa larm för de enheter vars bekräftade status skiljer sig från
//      den senast rapporterade
//
// LARMGRINDENS PRINCIP, ärvd från desktopvarianten:
// tillståndsmaskinen går ALLTID vidare, bara leveransen är villkorad.
// Ett larm som hålls tillbaka av avslagna larm eller undertryckning
// levereras när hindret upphör — det tappas inte.
//
// Genomförandet skiljer sig dock från desktop. I stället för en kö av
// väntande larm jämförs bekräftad status mot senast rapporterad varje
// svep. Effekten blir densamma, med en skillnad som är en förbättring:
// en enhet som hinner gå ner OCH upp igen medan larmen är tysta ger
// inget larm alls, eftersom rapporterat läge då stämmer med bekräftat.
// =====================================================================

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

use super::flap::{register_poll, HostMonitorState};
use super::ping::Pinger;
use super::polls::PollCounters;
use super::repo;
use super::slow::{SlowTracker, Verdict as SlowVerdict};
use super::suppression::suppression_reason;
use super::types::{Host, Outcome, ProbeType, RawStatus, Status};
use crate::db::Db;

/// Enhetens SMS-val filtrerar bara SMS-kanalen. Händelsen loggas och alla
/// andra konfigurerade kanaler fortsätter precis som vanligt.
fn channel_enabled_for_host(channel: &str, sms_enabled: bool) -> bool {
    channel != "sms" || sms_enabled
}

/// Resultatet av en enskild ping i ett svep.
struct PollResult {
    address: String,
    outcome: Outcome,
    /// Mikrosekunder. Millisekunder avrundar bort allt på ett lokalt nät.
    latency_us: Option<u32>,
    error: Option<String>,
}

pub struct Monitor {
    db: Db,
    polls: PollCounters,
    pinger: Arc<Pinger>,
    /// Delad HTTP-klient för http-prober. En klient per svep hade
    /// slösat anslutningspoolen som är hela poängen med reqwest.
    http: reqwest::Client,
    /// Flap-tillstånd per adress. Lever i minnet mellan svep; endast den
    /// bekräftade statusen persisteras.
    state: HashMap<String, HostMonitorState>,
    /// Latenslarmets tillstånd per adress (etapp 8). Samma princip som
    /// flap-tillståndet: minnesresident, börjar om vid omstart.
    slow: HashMap<String, SlowTracker>,
    /// När varje adress ska kontrolleras nästa gång.
    ///
    /// Ligger i minnet med flit. Efter en omstart kontrolleras allt en
    /// gång direkt, vilket är rätt: läget kan ha ändrats medan tjänsten
    /// var nere, och att vänta ut ett intervall först vore att blunda.
    next_due: HashMap<String, i64>,
}

impl Monitor {
    pub fn new(db: Db, polls: PollCounters) -> Result<Self> {
        Ok(Self {
            db,
            polls,
            pinger: Arc::new(Pinger::new()?),
            http: reqwest::Client::new(),
            state: HashMap::new(),
            slow: HashMap::new(),
            next_due: HashMap::new(),
        })
    }

    /// Kör slingan tills processen avslutas.
    pub async fn run(mut self) {
        // Återskapa flap-tillstånd från persisterad status. Sviterna
        // börjar färska — en svit från före omstarten säger inget om
        // läget efteråt.
        match self.db.call(repo::load_status).await {
            Ok(stored) => {
                let now = now_ms();
                for (addr, s) in stored {
                    self.state
                        .insert(addr, HostMonitorState::from_persisted(s.status, now));
                }
                tracing::info!("återskapade {} statusposter", self.state.len());
            }
            Err(e) => tracing::error!("kunde inte läsa host_status: {e:#}"),
        }

        // En tick per sekund. Upplösningen räcker gott för intervall som
        // mäts i sekunder, och håller slingan enkel att resonera om.
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            if let Err(e) = self.sweep().await {
                tracing::error!("svep misslyckades: {e:#}");
            }
        }
    }

    /// En omgång: kontrollera de enheter vars tid gått ut.
    async fn sweep(&mut self) -> Result<()> {
        let settings = self.db.call(repo::load_settings).await?;

        if !settings.live {
            // Pausad bevakning ska inte spara upp ett berg av förfallna
            // enheter som alla pingas i samma sekund när den slås på
            // igen. Nollställ i stället, så börjar allt om från noll.
            self.next_due.clear();
            return Ok(());
        }

        let hosts = self.db.call(repo::load_hosts).await?;
        let windows = self.db.call(repo::load_windows).await?;
        let stored = self.db.call(repo::load_status).await?;

        let now_due = now_ms();

        // Enheter vars tid gått ut. Okända adresser kontrolleras direkt.
        let active: Vec<Host> = hosts
            .iter()
            .filter(|h| h.enabled)
            .filter(|h| self.next_due.get(&h.address).map_or(true, |t| *t <= now_due))
            .cloned()
            .collect();

        if active.is_empty() {
            return Ok(());
        }

        // Sätt nästa tidpunkt innan pingen, inte efter. Tar en enhet lång
        // tid att svara ska nästa kontroll ändå ligga ett helt intervall
        // från den här — annars glider långsamma enheter isär från de
        // snabba och kontrolleras allt mer sällan.
        for h in &active {
            let sec = h.interval_sec.unwrap_or(settings.sweep_interval_sec as u32).max(1);
            self.next_due
                .insert(h.address.clone(), now_due + i64::from(sec) * 1000);
        }

        // Adresser som försvunnit ur listan ska inte ligga kvar och äta
        // minne i en tjänst som kör i månader.
        if self.next_due.len() > hosts.len() * 2 {
            let live: std::collections::HashSet<&str> =
                hosts.iter().map(|h| h.address.as_str()).collect();
            self.next_due.retain(|k, _| live.contains(k.as_str()));
            self.state.retain(|k, _| live.contains(k.as_str()));
            self.slow.retain(|k, _| live.contains(k.as_str()));
        }

        // ---- Mät parallellt ----
        //
        // Samma felkontrakt för alla prober: Ok(None) är ett mätvärde
        // (inget svar), Err är att mätningen inte kunde utföras.
        let timeout = Duration::from_secs(settings.ping_timeout_sec);
        let mut set = JoinSet::new();

        for h in &active {
            let pinger = Arc::clone(&self.pinger);
            let http = self.http.clone();
            let address = h.address.clone();
            // Per-host-överskrivning vinner över den globala storleken.
            // None betyder ärv, inte noll — skillnaden är betydelsebärande.
            let size = h.packet_size.unwrap_or(settings.packet_size) as usize;
            let probe = h.probe_type;
            // Port 0 för tcp/http utan port lyssnar aldrig — anslutningen
            // misslyckas och mätvärdet blir "inget svar". API:t validerar
            // att porten finns; nollan är bara en sköld mot trasig data.
            let port = h.probe_port.unwrap_or(0);
            set.spawn(async move {
                let measured = match probe {
                    ProbeType::Icmp => pinger.ping(&address, size, timeout).await,
                    ProbeType::Tcp => super::probe::tcp_check(&address, port, timeout).await,
                    ProbeType::Http => super::probe::http_check(&http, &address, port, timeout).await,
                };
                match measured {
                    Ok(Some(rtt)) => PollResult {
                        address,
                        outcome: Outcome::Success,
                        latency_us: Some(rtt.as_micros().min(u128::from(u32::MAX)) as u32),
                        error: None,
                    },
                    Ok(None) => PollResult {
                        address,
                        outcome: Outcome::Fail,
                        latency_us: None,
                        error: None,
                    },
                    Err(e) => PollResult {
                        address,
                        outcome: Outcome::Fail,
                        latency_us: None,
                        error: Some(e.to_string()),
                    },
                }
            });
        }

        let mut results: HashMap<String, PollResult> = HashMap::new();
        let mut socket_errors = 0usize;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(r) => {
                    if r.error.is_some() {
                        socket_errors += 1;
                    }
                    results.insert(r.address.clone(), r);
                }
                Err(e) => tracing::error!("ping-task kraschade: {e}"),
            }
        }

        // Socketfel loggas en gång per svep, inte per enhet. Går ingen
        // ping igenom är det nästan alltid ping_group_range som är fel
        // satt, och hundra identiska rader hjälper ingen.
        if socket_errors == active.len() && socket_errors > 0 {
            let sample = results
                .values()
                .find_map(|r| r.error.clone())
                .unwrap_or_default();
            tracing::error!("samtliga {socket_errors} pingar misslyckades: {sample}");
        }

        // Alla prober ovan är redan genomförda. Registrera därför hela
        // svepets resultat innan någon databaswrite kan avbryta loopen.
        // Räknaren beskriver faktiska pollningar, inte lyckade commits.
        for result in results.values() {
            self.polls
                .record(&result.address, result.outcome == Outcome::Success);
        }

        // ---- Uppdatera status och avgör larm ----
        let now = now_ms();
        let mut updated: Vec<Host> = Vec::with_capacity(active.len());
        let mut changes: Vec<(Host, Status, Status)> = Vec::new();
        // Övergångar som flyttar fram rapporterat läge utan att larma.
        let mut quiet: Vec<(String, Status)> = Vec::new();
        // Latenslarm: (enhet, är-larm-inte-återställning, svarstid ms).
        // Separat från changes — slow är ett VARNINGSlarm, inte en
        // statusövergång, och eskalerar aldrig via SMS (se slow.rs).
        let mut slow_changes: Vec<(Host, bool, Option<u32>)> = Vec::new();

        for host in &active {
            let Some(res) = results.get(&host.address) else {
                continue;
            };

            let cfg = repo::hysteresis_for(host, &settings);
            let entry = self
                .state
                .entry(host.address.clone())
                .or_insert_with(|| HostMonitorState::from_persisted(host.confirmed, now));

            let change = register_poll(entry, res.outcome, now, &cfg);
            let confirmed = entry.status;

            // Tröskeln anges i millisekunder, mätningen är i mikrosekunder.
            let slow_us = host
                .slow_threshold_ms
                .unwrap_or(settings.slow_threshold_ms)
                .saturating_mul(1000);
            let raw = match (res.outcome, res.latency_us) {
                (Outcome::Fail, _) => RawStatus::Offline,
                (Outcome::Success, Some(us)) if us >= slow_us => RawStatus::Warning,
                _ => RawStatus::Online,
            };

            let mut h = host.clone();
            h.confirmed = confirmed;
            h.raw = raw;

            if let Some(c) = change {
                tracing::info!(
                    "{} ({}): {} → {}",
                    h.name,
                    h.address,
                    c.from.as_str(),
                    c.to.as_str()
                );
            }

            let reported = stored
                .get(&host.address)
                .map(|s| s.reported)
                .unwrap_or(Status::Unknown);

            // Rapportera bara verkliga nyheter.
            //
            // "up" utan föregående "down" är ingen nyhet — det är
            // normaltillståndet. Utan den här regeln ger varje omstart
            // ett "uppe"-larm per frisk enhet, alltså hundra
            // meddelanden i en anläggning med hundra enheter.
            //
            // Rapporterat läge flyttas ändå fram, så att den enhet som
            // senare går ner får ett korrekt "down → up"-par.
            let is_news = confirmed != reported
                && confirmed != Status::Unknown
                && !(confirmed == Status::Up && reported != Status::Down);

            if is_news {
                changes.push((h.clone(), reported, confirmed));
            } else if confirmed != reported && confirmed != Status::Unknown {
                // Tyst framflyttning: ingen rapport, men läget noteras.
                quiet.push((host.address.clone(), confirmed));
            }

            // ---- Latenslarm (etapp 8) ----
            //
            // Matas bara när grinden bekräftat UPP — ett latenslarm på
            // en enhet som är på väg NER är brus; NER-larmet berättar
            // den viktigare historien. Misslyckad mätning nollställer
            // sviten tyst (None), se slow.rs.
            if settings.slow_alarm {
                let tracker = self
                    .slow
                    .entry(host.address.clone())
                    .or_insert_with(SlowTracker::new);
                let feeding = if confirmed != Status::Up || res.outcome == Outcome::Fail {
                    None
                } else {
                    Some(raw == RawStatus::Warning)
                };
                match tracker.register(feeding, now, settings.slow_alarm_sec) {
                    SlowVerdict::ShouldAlarm => {
                        slow_changes.push((h.clone(), true, res.latency_us.map(|us| us / 1000)))
                    }
                    SlowVerdict::Recovered => {
                        slow_changes.push((h.clone(), false, res.latency_us.map(|us| us / 1000)))
                    }
                    SlowVerdict::Nothing => {}
                }
            }

            updated.push(h);
        }

        // ---- Undertryckning ----
        //
        // Uppslagningen sker mot den FÄRSKA listan, så ett beroende
        // bedöms på förälderns status i det här svepet och inte på
        // förra svepets.
        let by_address: HashMap<String, Host> = updated
            .iter()
            .map(|h| (h.address.clone(), h.clone()))
            .collect();
        let lookup = |addr: &str| by_address.get(addr).cloned();

        // ---- Skriv ----
        for h in &updated {
            let Some(res) = results.get(&h.address) else {
                continue;
            };
            let online = res.outcome == Outcome::Success;
            let latency = res.latency_us;
            let addr = h.address.clone();
            let confirmed = h.confirmed;
            let raw = h.raw;

            // Rapporterat läge ändras bara när larmet faktiskt går ut.
            let reported = stored
                .get(&h.address)
                .map(|s| s.reported)
                .unwrap_or(Status::Unknown);

            self.db
                .call(move |conn| {
                    repo::record_poll(
                        conn,
                        repo::PollRecord {
                            address: &addr,
                            ts: now,
                            online,
                            latency_us: latency,
                            status: confirmed,
                            reported,
                            raw,
                        },
                    )
                })
                .await?;
        }

        // Tysta framflyttningar skrivs innan grinden, så att ett
        // efterföljande larm jämförs mot rätt utgångsläge.
        for (addr, status) in quiet {
            self.db
                .call(move |conn| repo::save_transition(conn, &addr, status, status, now))
                .await?;
        }

        // ---- Larmgrinden ----
        for (host, from, to) in changes {
            let reason = suppression_reason(&host, now, &windows, &lookup);

            if let Some(r) = reason {
                tracing::info!(
                    "{}: {} → {} undertryckt ({:?})",
                    host.name,
                    from.as_str(),
                    to.as_str(),
                    r
                );
                continue;
            }

            if !settings.alarms {
                tracing::info!(
                    "{}: {} → {} hålls tillbaka, larm avslagna",
                    host.name,
                    from.as_str(),
                    to.as_str()
                );
                continue;
            }

            let event = if to == Status::Down { "down" } else { "up" };
            let time_str = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let payload = serde_json::json!({
                "app": "netfyr",
                "device": host.name,
                "address": host.address,
                "status": event,
                "latencyMs": null,
                "time": time_str,
            })
            .to_string();

            let channels = settings.channels.clone();
            let name = host.name.clone();
            let addr = host.address.clone();
            let sms_enabled = host.sms_enabled;
            let level = if event == "down" { "alarm" } else { "ok" };
            let down = event == "down";

            self.db
                .call(move |conn| {
                    let lang = crate::i18n::load(conn);
                    let text = crate::i18n::ev_alarm_confirmed(lang, down, &name, &addr);
                    repo::log_event(conn, now, level, &text)?;
                    for ch in &channels {
                        if !channel_enabled_for_host(ch, sms_enabled) {
                            continue;
                        }
                        // SMS-kanalen kan ha eskalering påslagen: då
                        // skapas en session och bara första mottagaren
                        // köas. sms-motorn sköter resten.
                        let final_payload = if ch == "sms" {
                            crate::sms_engine::prepare(conn, &name, &addr, event, &payload, now)?
                        } else {
                            payload.clone()
                        };
                        repo::enqueue_delivery(conn, ch, &name, event, &final_payload, now)?;
                    }
                    // Rapporterat läge sätts först när larmet är köat.
                    // Faller processen mellan grind och kö larmar den om
                    // nästa svep — hellre dubbelt än uteblivet.
                    repo::save_transition(conn, &addr, to, to, now)?;
                    Ok(())
                })
                .await?;

            if settings.channels.is_empty() {
                tracing::warn!("{}: larm men inga kanaler konfigurerade", host.name);
            }
        }

        // ---- Latenslarmets grind ----
        //
        // Samma regler som statuslarmen: undertryckning och avslagna
        // larm håller tillbaka, men inget tappas — ShouldAlarm upprepas
        // varje svep tills leveransen bekräftas. Återställningen är det
        // enda som kan tappas (dokumenterat i slow.rs).
        for (host, is_alarm, latency_ms) in slow_changes {
            let addr0 = host.address.clone();

            if let Some(r) = suppression_reason(&host, now, &windows, &lookup) {
                let tracker = self.slow.entry(addr0.clone()).or_insert_with(SlowTracker::new);
                if is_alarm && tracker.mark_held() {
                    tracing::info!("{}: latenslarm undertryckt ({:?})", host.name, r);
                }
                continue;
            }
            if !settings.alarms {
                let tracker = self.slow.entry(addr0.clone()).or_insert_with(SlowTracker::new);
                if is_alarm && tracker.mark_held() {
                    tracing::info!("{}: latenslarm hålls tillbaka, larm avslagna", host.name);
                }
                continue;
            }

            let event = if is_alarm { "slow" } else { "slow_ok" };
            let time_str = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let payload = serde_json::json!({
                "app": "netfyr",
                "device": host.name,
                "address": host.address,
                "status": event,
                "latencyMs": latency_ms,
                "time": time_str,
            })
            .to_string();

            let channels = settings.channels.clone();
            let name = host.name.clone();
            let addr = host.address.clone();
            let sms_enabled = host.sms_enabled;
            let level = if is_alarm { "warn" } else { "ok" };

            self.db
                .call(move |conn| {
                    let lang = crate::i18n::load(conn);
                    let text = crate::i18n::ev_slow(lang, is_alarm, &name, &addr, latency_ms);
                    repo::log_event(conn, now, level, &text)?;
                    for ch in &channels {
                        if !channel_enabled_for_host(ch, sms_enabled) {
                            continue;
                        }
                        // INGEN sms_engine::prepare här: slow eskalerar
                        // aldrig. Payloaden går till kanalens globala
                        // mottagarlista som den är.
                        repo::enqueue_delivery(conn, ch, &name, event, &payload, now)?;
                    }
                    Ok(())
                })
                .await?;

            if is_alarm {
                tracing::info!(
                    "{} ({}): LÅNGSAM bekräftad (>{} ms i {} s)",
                    host.name,
                    host.address,
                    host.slow_threshold_ms.unwrap_or(settings.slow_threshold_ms),
                    settings.slow_alarm_sec
                );
                self.slow
                    .entry(addr0)
                    .or_insert_with(SlowTracker::new)
                    .mark_delivered();
            } else {
                tracing::info!("{} ({}): normal svarstid igen", host.name, host.address);
            }
        }

        Ok(())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod sms_tests {
    use super::channel_enabled_for_host;

    #[test]
    fn sms_hoppas_over_nar_enheten_har_sms_av() {
        assert!(!channel_enabled_for_host("sms", false));
    }

    #[test]
    fn andra_kanaler_paverkas_inte_av_enhetens_sms_val() {
        for channel in ["smtp", "mqtt", "webhook"] {
            assert!(channel_enabled_for_host(channel, false));
        }
    }

    #[test]
    fn sms_koas_nar_enheten_har_sms_pa() {
        assert!(channel_enabled_for_host("sms", true));
    }
}
