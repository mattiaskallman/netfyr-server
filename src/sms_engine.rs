// =====================================================================
// sms_engine.rs
// SMS-eskalering och kvittering.
//
// Serverns motsvarighet till desktopens smsSessions.ts + TrbContext.tsx.
// Desktop höll sessionerna i minnet och localStorage; här ligger de i
// SQLite (migration v3) så att en öppen session överlever omstarter —
// tickern återupptar den direkt, hellre ett sent SMS än ett tyst
// förlorat larm.
//
// Flödet:
//
//   monitor.rs ──prepare()──► session skapas, första mottagaren köas
//        │                     (leveranskön ger omförsök och backoff)
//        ▼
//   run() tickar var 5:e sekund:
//     • utgångna kvittensfönster stängs ("expired")
//     • förfallna eskaleringar skickar nästa mottagare, eller stänger
//       kedjan ("exhausted") när listan är slut
//     • inkorgen pollas med cfg.poll_sec intervall: ett svar som
//       innehåller sessionskoden [A7] — eller kommer från ett nummer
//       som redan larmats — kvitterar sessionen ("ack")
//     • återställning stänger sessionen ("recovered") och går bara
//       till dem som faktiskt hunnit larmats
//
// GDPR: sessionerna innehåller telefonnummer (personuppgifter). De
// städas bort 7 dagar efter att de stängts — tillräckligt för
// uppföljning i gränssnittet, inte längre.
// =====================================================================

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::time::{Duration, Instant};

use crate::channels::sms;
use crate::db::Db;
use crate::engine::repo;
use crate::secrets::Secrets;

/// Hur ofta tickern tittar på klockan. Själva besluten styrs av
/// sessionernas tidsstämplar, så en vilande motor kostar inget.
const TICK: Duration = Duration::from_secs(5);

/// Stängda sessioner behålls för granskning i gränssnittet, sedan
/// raderas de — telefonnummer ska inte ligga kvar i onödan.
const KEEP_CLOSED_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Behandlade inkorgsnycklar behålls ett dygn.
const KEEP_SEEN_MS: i64 = 24 * 60 * 60 * 1000;

/// Gatewayens och serverns klockor behöver inte vara synkroniserade.
/// Marginalen gör att en äkta kvittens inte avvisas för klockglidning,
/// samtidigt som gammal historik inte kan kvittera ett färskt larm.
const CLOCK_SKEW_MS: i64 = 10 * 60 * 1000;

/// Kodalfabetet saknar I och O — de förväxlas för lätt med 1 och 0
/// när koden läses upp eller knappas in under stress.
const CODE_LETTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub device: String,
    pub address: String,
    pub recipients: Vec<String>,
    pub sent_count: i64,
    pub next_escalation_at: Option<i64>,
    pub created_at: i64,
    pub expires_at: i64,
    pub acked_at: Option<i64>,
    pub acked_by: Option<String>,
    pub closed: bool,
    pub closed_reason: Option<String>,
}

// ---- Databas -----------------------------------------------------------

type Row = (
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    i64,
    Option<String>,
);

const SELECT: &str = "SELECT id, device, address, recipients, sent_count,
    next_escalation_at, created_at, expires_at, acked_at, acked_by,
    closed, closed_reason FROM sms_sessions";

fn row_to_session(r: Row) -> Result<Session, rusqlite::Error> {
    let recipients = serde_json::from_str(&r.3).unwrap_or_default();
    Ok(Session {
        id: r.0,
        device: r.1,
        address: r.2,
        recipients,
        sent_count: r.4,
        next_escalation_at: r.5,
        created_at: r.6,
        expires_at: r.7,
        acked_at: r.8,
        acked_by: r.9,
        closed: r.10 != 0,
        closed_reason: r.11,
    })
}

fn load_open(conn: &Connection) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(&format!("{SELECT} WHERE closed = 0"))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(row_to_session(r?)?);
    }
    Ok(out)
}

/// Senaste sessionerna, nyast först — för gränssnittets sessionslista.
pub fn list_recent(conn: &Connection, limit: usize) -> Result<Vec<Session>> {
    let mut stmt =
        conn.prepare(&format!("{SELECT} ORDER BY created_at DESC LIMIT ?1"))?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(row_to_session(r?)?);
    }
    Ok(out)
}

fn open_session_for(conn: &Connection, address: &str) -> Result<Option<Session>> {
    Ok(load_open(conn)?.into_iter().find(|s| s.address == address))
}

/// Kort kod som inte krockar med någon öppen session.
fn next_code(conn: &Connection) -> Result<String> {
    let taken: Vec<String> = load_open(conn)?.into_iter().map(|s| s.id).collect();
    for _ in 0..500 {
        let li = (rand::u8() as usize) % CODE_LETTERS.len();
        let letter = CODE_LETTERS[li] as char;
        let digit = rand::u8() % 10;
        let code = format!("{letter}{digit}");
        if !taken.contains(&code) {
            return Ok(code);
        }
    }
    // Utrymmet är 240 koder och sessioner stängs — men hellre en längre
    // kod än en dubblett som gör att fel session kvitteras.
    Ok(format!("{}", now_ms() % 100000))
}

fn create_session(
    conn: &Connection,
    device: &str,
    address: &str,
    recipients: &[String],
    escalation_sec: u64,
    ack_window_min: u64,
    now: i64,
) -> Result<Session> {
    let id = next_code(conn)?;
    let session = Session {
        id: id.clone(),
        device: device.to_string(),
        address: address.to_string(),
        recipients: recipients.to_vec(),
        sent_count: 0,
        // Sätts även för sista mottagaren: tiden avgör när kedjan ska
        // betraktas som uttömd utan kvittens.
        next_escalation_at: Some(now + escalation_sec as i64 * 1000),
        created_at: now,
        expires_at: now + ack_window_min as i64 * 60 * 1000,
        acked_at: None,
        acked_by: None,
        closed: false,
        closed_reason: None,
    };
    conn.execute(
        "INSERT INTO sms_sessions
            (id, device, address, recipients, sent_count,
             next_escalation_at, created_at, expires_at, closed)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, 0)",
        params![
            id,
            device,
            address,
            serde_json::to_string(recipients)?,
            session.next_escalation_at,
            now,
            session.expires_at
        ],
    )?;
    Ok(session)
}

fn record_escalation(conn: &Connection, id: &str, escalation_sec: u64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE sms_sessions
         SET sent_count = sent_count + 1, next_escalation_at = ?2
         WHERE id = ?1",
        params![id, now + escalation_sec as i64 * 1000],
    )?;
    Ok(())
}

fn close(conn: &Connection, id: &str, reason: &str) -> Result<()> {
    conn.execute(
        "UPDATE sms_sessions
         SET closed = 1, closed_reason = ?2, next_escalation_at = NULL
         WHERE id = ?1",
        params![id, reason],
    )?;
    Ok(())
}

fn mark_ack(conn: &Connection, id: &str, sender: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE sms_sessions
         SET acked_at = ?2, acked_by = ?3, closed = 1,
             closed_reason = 'ack', next_escalation_at = NULL
         WHERE id = ?1",
        params![id, now, sender],
    )?;
    Ok(())
}

fn is_seen(conn: &Connection, key: &str) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let n: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sms_seen_inbox WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(n.is_some())
}

fn mark_seen(conn: &Connection, keys: &[String], now: i64) -> Result<()> {
    let mut stmt =
        conn.prepare("INSERT OR IGNORE INTO sms_seen_inbox (key, seen_at) VALUES (?1, ?2)")?;
    for k in keys {
        stmt.execute(params![k, now])?;
    }
    Ok(())
}

fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM sms_sessions WHERE closed = 1 AND created_at < ?1",
        [now - KEEP_CLOSED_MS],
    )?;
    conn.execute(
        "DELETE FROM sms_seen_inbox WHERE seen_at < ?1",
        [now - KEEP_SEEN_MS],
    )?;
    Ok(())
}

fn channel_config(conn: &Connection) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'channel.sms.config'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v)
}

// ---- Larmkoppling (anropas av monitor.rs i databastransaktionen) ------

/// Förbereder ett larm för sms-kanalen. Returnerar den payload som ska
/// köas — med eskalering skrivs mottagarlistan och sessionskoden in.
///
/// Speglar desktopens registerAlarmSink i TrbContext.tsx.
pub fn prepare(
    conn: &Connection,
    device: &str,
    address: &str,
    event: &str,
    payload: &str,
    now: i64,
) -> Result<String> {
    let Some(config) = channel_config(conn)? else {
        return Ok(payload.to_string());
    };
    let cfg = match sms::parse_config(&config) {
        Ok(c) => c,
        Err(_) => return Ok(payload.to_string()),
    };

    if event == "up" {
        // Återställning eskalerar aldrig. Den går direkt till alla som
        // hunnit få larmet — att hålla tillbaka ett "åter i drift" vore
        // meningslöst, och de som aldrig larmades ska inte få beskedet.
        if let Some(s) = open_session_for(conn, address)? {
            close(conn, &s.id, "recovered")?;
            let n = (s.sent_count as usize).min(s.recipients.len());
            let targets: Vec<String> = s.recipients[..n].to_vec();
            if targets.is_empty() {
                return Ok(payload.to_string());
            }
            return Ok(with_sms_fields(payload, targets, &s.id));
        }
        return Ok(payload.to_string());
    }

    let recipients: Vec<String> = cfg
        .recipients
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if !cfg.escalation_enabled || recipients.len() < 2 {
        return Ok(payload.to_string());
    }

    // Eskalering: bara första mottagaren köas nu. Tickern släpper fram
    // nästa när tiden gått utan kvittens.
    let session = create_session(
        conn,
        device,
        address,
        &recipients,
        cfg.escalation_sec,
        cfg.ack_window_min,
        now,
    )?;
    record_escalation(conn, &session.id, cfg.escalation_sec, now)?;
    repo::log_event(
        conn,
        now,
        "info",
        &crate::i18n::ev_escalation_queued(
            crate::i18n::load(conn),
            device,
            &session.id,
            recipients.len(),
        ),
    )?;
    Ok(with_sms_fields(payload, vec![recipients[0].clone()], &session.id))
}

fn with_sms_fields(payload: &str, recipients: Vec<String>, session_id: &str) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(payload).unwrap_or_else(|_| serde_json::json!({}));
    v["smsRecipients"] = serde_json::json!(recipients);
    v["smsSessionId"] = serde_json::json!(session_id);
    v.to_string()
}

// ---- Matchningslogik (port från desktopens smsSessions.ts) ------------

/// Tolkar gatewayens tidsformat "2026-08-03 14:14:48" som lokal tid.
/// Returnerar None när formatet inte känns igen — anroparen avgör då
/// vad som är säkrast i stället för att få en tyst felaktig tidpunkt.
fn parse_gateway_date(date: &str) -> Option<i64> {
    let d = date.trim();
    let (date_part, time_part) = d.split_once(|c| c == ' ' || c == 'T')?;
    let mut dp = date_part.split('-');
    let year: i32 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    let mut tp = time_part.split(':');
    let h: u32 = tp.next()?.parse().ok()?;
    let m: u32 = tp.next()?.parse().ok()?;
    let s: u32 = tp.next()?.parse().ok()?;

    use chrono::{Local, TimeZone};
    let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(h, m, s)?;
    Some(Local.from_local_datetime(&naive).single()?.timestamp_millis())
}

/// Kan meddelandet ha kommit som svar på sessionen? Otolkbart datum
/// accepteras — baslinjemarkeringen vid uppstart har då redan sorterat
/// bort allt som fanns innan.
fn could_answer(message_date: &str, session_created_at: i64) -> bool {
    match parse_gateway_date(message_date) {
        Some(t) => t >= session_created_at - CLOCK_SKEW_MS,
        None => true,
    }
}

/// Jämför två nummer på de sista nio siffrorna. Gatewayen rapporterar
/// avsändaren i det format operatören levererar, vilket inte behöver
/// matcha hur numret skrevs in i mottagarlistan — +467****4567,
/// 0046701234567 och 0701234567 är samma abonnent.
fn same_phone(a: &str, b: &str) -> bool {
    let da: String = a.chars().filter(|c| c.is_ascii_digit()).collect();
    let db: String = b.chars().filter(|c| c.is_ascii_digit()).collect();
    if da.len() < 6 || db.len() < 6 {
        return false;
    }
    let n = da.len().min(db.len()).min(9);
    da[da.len() - n..] == db[db.len() - n..]
}

/// Hittar en sessionskod i en svarstext. Koden söks som fristående
/// token, så att "A7" i "[A7]" eller "ok a7" träffar medan "A7" inuti
/// ett längre ord inte gör det.
fn find_session_code(message: &str, codes: &[String]) -> Option<String> {
    let upper = message.to_uppercase();
    for code in codes {
        let mut start = 0;
        while let Some(pos) = upper[start..].find(code.as_str()) {
            let abs = start + pos;
            let before_ok = abs == 0
                || !upper.as_bytes()[abs - 1].is_ascii_alphanumeric();
            let after = abs + code.len();
            let after_ok = after >= upper.len()
                || !upper.as_bytes()[after].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return Some(code.clone());
            }
            start = abs + 1;
        }
    }
    None
}

/// Nyckel för ett inkorgsmeddelande. Gatewayens id är ett löpande index
/// och återanvänds när meddelanden raderas, så det duger inte ensamt.
/// Avsändare, tidsstämpel och ett textprefix är i praktiken unikt.
fn inbox_key(sender: &str, date: &str, message: &str) -> String {
    let prefix: String = message.chars().take(24).collect();
    format!("{sender}|{date}|{prefix}")
}

// ---- Bakgrundstask ------------------------------------------------------

pub async fn run(db: Db, secrets: Secrets) {
    tracing::info!("sms-motor igång (eskalering och kvittering)");

    // Baslinje vid uppstart: allt som redan ligger i inkorgen markeras
    // som behandlat utan att matchas mot någon session. Utan detta kan
    // ett gammalt SMS från ett journummer kvittera nästa larm i samma
    // sekund som det utlöses.
    let mut baseline_done = false;
    let mut last_poll = Instant::now() - Duration::from_secs(3600);

    loop {
        tokio::time::sleep(TICK).await;
        if let Err(e) = tick(&db, &secrets, &mut baseline_done, &mut last_poll).await {
            tracing::warn!("sms-motor: {e:#}");
        }
    }
}

async fn tick(
    db: &Db,
    secrets: &Secrets,
    baseline_done: &mut bool,
    last_poll: &mut Instant,
) -> Result<()> {
    let now = now_ms();

    // Del 1: livscykeln. Utgångna sessioner stängs, förfallna
    // eskaleringar släpper fram nästa mottagare. Allt i en transaktion.
    db.call(move |conn| {
        let open = load_open(conn)?;
        if open.is_empty() {
            return Ok(());
        }

        for s in &open {
            if now >= s.expires_at {
                close(conn, &s.id, "expired")?;
                repo::log_event(
                    conn,
                    now,
                    "warn",
                    &crate::i18n::ev_ack_window_expired(crate::i18n::load(conn), &s.device, &s.id),
                )?;
            }
        }

        // Eskaleringstiden läses ur konfigurationen varje gång — en
        // ändring i gränssnittet gäller direkt, utan omstart.
        let escalation_sec = channel_config(conn)?
            .and_then(|c| sms::parse_config(&c).ok())
            .map(|c| c.escalation_sec)
            .unwrap_or(300);

        for s in &open {
            if now >= s.expires_at {
                continue; // just stängd ovan
            }
            let Some(next_at) = s.next_escalation_at else {
                continue;
            };
            if now < next_at {
                continue;
            }

            let n = s.sent_count as usize;
            if n >= s.recipients.len() {
                close(conn, &s.id, "exhausted")?;
                repo::log_event(
                    conn,
                    now,
                    "warn",
                    &crate::i18n::ev_no_ack(crate::i18n::load(conn), &s.device, &s.id),
                )?;
                continue;
            }

            let recipient = s.recipients[n].clone();
            record_escalation(conn, &s.id, escalation_sec, now)?;
            repo::log_event(
                conn,
                now,
                "info",
                &crate::i18n::ev_escalating(
                    crate::i18n::load(conn),
                    &s.device,
                    &s.id,
                    n + 1,
                    s.recipients.len(),
                ),
            )?;

            // Köas som en vanlig leverans: omförsök och backoff följer
            // med gratis, och fallet syns i leveransloggen.
            let payload = serde_json::json!({
                "app": "netfyr",
                "device": s.device,
                "address": s.address,
                "status": "down",
                "latencyMs": null,
                "time": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                "smsRecipients": [recipient],
                "smsSessionId": s.id,
            })
            .to_string();
            repo::enqueue_delivery(conn, "sms", &s.device, "down", &payload, now)?;
        }

        prune(conn, now)?;
        Ok(())
    })
    .await?;

    // Del 2: kvittenspollning. Körs med konfigurationens intervall och
    // bara när det finns öppna sessioner — en tom sessionslista ska
    // aldrig belasta gatewayen.
    let poll_ctx: Option<(String, sms::SmsConfig, Vec<Session>)> = db
        .call(move |conn| {
            let open = load_open(conn)?;
            if open.is_empty() {
                return Ok(None);
            }
            let Some(config) = channel_config(conn)? else {
                return Ok(None);
            };
            let Ok(cfg) = sms::parse_config(&config) else {
                return Ok(None);
            };
            if !cfg.escalation_enabled {
                return Ok(None);
            }
            Ok(Some((config, cfg, open)))
        })
        .await?;

    let Some((config, cfg, open)) = poll_ctx else {
        return Ok(());
    };

    let poll_interval = Duration::from_secs(cfg.poll_sec.clamp(10, 3600));
    if last_poll.elapsed() < poll_interval && *baseline_done {
        return Ok(());
    }
    *last_poll = Instant::now();

    let lang = db.call(|conn| Ok(crate::i18n::load(conn))).await?;
    let password = match secrets.get("sms") {
        Some(p) if !p.trim().is_empty() => p,
        _ => return Ok(()),
    };
    if !sms::connection_ready(&cfg, &password) {
        return Ok(());
    }

    let inbox = match sms::inbox(&config, &password, lang).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("sms: kunde inte läsa inkorgen ({e:#})");
            return Ok(());
        }
    };

    if !*baseline_done {
        *baseline_done = true;
        let keys: Vec<String> = inbox
            .iter()
            .map(|m| inbox_key(&m.sender, &m.date, &m.message))
            .collect();
        let n = keys.len();
        db.call(move |conn| mark_seen(conn, &keys, now_ms()))
            .await?;
        if n > 0 {
            tracing::info!("sms: inkorgens {n} meddelanden baslinjemarkerade");
        }
        return Ok(());
    }

    // Matchning sker mot en färsk sessionslista — en eskalering kan ha
    // hunnit utlösa medan nätverksanropet pågick.
    let fresh_open: Vec<Session> = db.call(move |conn| load_open(conn)).await?;
    if fresh_open.is_empty() {
        return Ok(());
    }

    let codes: Vec<String> = fresh_open.iter().map(|s| s.id.clone()).collect();
    let mut handled: Vec<String> = Vec::new();
    let mut to_delete: Vec<(String, String)> = Vec::new();
    let mut acks: Vec<(String, String)> = Vec::new(); // (session_id, avsändare)

    for item in &inbox {
        let key = inbox_key(&item.sender, &item.date, &item.message);
        let already = db.call({
            let key = key.clone();
            move |conn| is_seen(conn, &key)
        })
        .await?;
        if already {
            continue;
        }
        handled.push(key);

        // Bara meddelanden som kan ha kommit EFTER att sessionen
        // skapades får kvittera den. Utan det kvitterar gammal historik
        // i inkorgen ett larm som inte fanns när svaret skickades.
        let candidates: Vec<&Session> = fresh_open
            .iter()
            .filter(|s| could_answer(&item.date, s.created_at))
            .collect();

        // Kod först, avsändare som reserv. Många svarar bara "OK".
        let session = find_session_code(&item.message, &codes)
            .and_then(|code| candidates.iter().find(|s| s.id == code).copied())
            .or_else(|| {
                candidates.iter().copied().find(|s| {
                    let n = (s.sent_count as usize).min(s.recipients.len());
                    s.recipients[..n].iter().any(|r| same_phone(r, &item.sender))
                })
            });

        let Some(session) = session else {
            continue;
        };

        acks.push((session.id.clone(), item.sender.clone()));
        if cfg.delete_after_ack {
            to_delete.push((item.modem_id.clone(), item.id.clone()));
        }
    }

    if !handled.is_empty() || !acks.is_empty() {
        let now2 = now_ms();
        db.call(move |conn| {
            mark_seen(conn, &handled, now2)?;
            for (id, sender) in &acks {
                mark_ack(conn, id, sender, now2)?;
                repo::log_event(
                    conn,
                    now2,
                    "info",
                    &crate::i18n::ev_acked(crate::i18n::load(conn), id, sender),
                )?;
                tracing::info!("sms kvitterad: [{id}] av {sender}");
            }
            Ok(())
        })
        .await?;
    }

    // Städning är bäst-möjligt: misslyckas den är kvitteringen redan
    // registrerad, och ett fel här får aldrig fälla larmhanteringen.
    for (modem_id, id) in to_delete {
        if let Err(e) = sms::remove_messages(&config, &password, &modem_id, vec![id], lang).await {
            tracing::warn!("sms: kunde inte radera meddelande i gateway ({e:#})");
        }
    }

    // `open` används bara för att avgöra om pollning behövs — matchningen
    // ovan gick mot den färska listan.
    drop(open);
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// Slumpmässighet utan extra beroende: tillräckligt för att välja en
// sessionskod som ändå kolliderkontrolleras mot databasen.
mod rand {
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn u8() -> u8 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        // Blanda in stackadressen så två anrop i samma nanosekund
        // (o sannolikt) ändå skiljer sig åt.
        let stack_marker = 0u8;
        let addr = &stack_marker as *const u8 as usize;
        ((nanos as usize ^ addr) & 0xFF) as u8
    }
}
