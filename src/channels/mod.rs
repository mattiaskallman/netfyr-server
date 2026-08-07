// =====================================================================
// channels/mod.rs
// Notifieringskanaler.
//
// En kanal tar ett larm och en konfiguration och skickar det vidare.
// Dispatchen nedan är den enda platsen som känner till vilka kanaler
// som finns — en ny kanal är en match-arm plus en modul.
//
// Portade från desktopvariantens lib.rs och pro/sms/mod.rs.
//
// SKILLNAD MOT DESKTOP: webhook och SMS använder ASYNK reqwest i
// stället för den blockerande varianten. reqwest::blocking startar en
// egen runtime internt, vilket inte hör hemma i en tjänst som redan
// kör tokio. SMTP och MQTT är kvar som blockerande och körs genom
// spawn_blocking — deras bibliotek startar ingen egen runtime.
// =====================================================================

pub mod mqtt;
pub mod smtp;
pub mod sms;
pub mod webhook;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::secrets::Secrets;

/// Larmet som skickas till kanalerna.
///
/// Fältnamnen speglar desktopvariantens payload, så mottagare som
/// webhookar och MQTT-prenumeranter fungerar oförändrat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmPayload {
    pub app: String,
    pub device: String,
    pub address: String,
    /// "down" eller "up".
    pub status: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub latency_ms: Option<u32>,
    pub time: String,
    /// Eskaleringen skriver över mottagarlistan per leverans. Tomt = kanalens
    /// globala lista gäller. Speglar desktopvariantens AlarmLite.
    /// Alias: prepare()/tickern skriver desktopens camelCase-nycklar i
    /// payloaden — utan alias tappas fälten tyst vid deserialisering och
    /// SMS går till hela den globala listan utan sessions-id.
    #[serde(default, alias = "smsRecipients")]
    pub sms_recipients: Vec<String>,
    /// Kort kvittenskod, t.ex. "A7". Tom när larmet inte eskalerar.
    #[serde(default, alias = "smsSessionId")]
    pub sms_session_id: String,
}

/// Skicka ett larm på en kanal.
///
/// `config` är kanalens JSON-konfiguration, hämtad ur settings.
/// `lang` styr felmeddelanden och standardtexter (i18n.rs).
pub async fn send(
    channel: &str,
    payload: &str,
    config: &str,
    secrets: &Secrets,
    lang: crate::i18n::Lang,
) -> Result<()> {
    match channel {
        "webhook" => {
            let url = secrets.require("webhook")?;
            webhook::send(&url, payload, lang).await
        }
        "smtp" => {
            let password = secrets.require("smtp")?;
            let (payload, config) = (payload.to_string(), config.to_string());
            tokio::task::spawn_blocking(move || smtp::send(&payload, &config, &password, lang)).await?
        }
        "mqtt" => {
            // Lösenord är valfritt: brokers kan tillåta anonym publicering.
            let password = secrets.get("mqtt").unwrap_or_default();
            let (payload, config) = (payload.to_string(), config.to_string());
            tokio::task::spawn_blocking(move || mqtt::send(&payload, &config, &password, lang)).await?
        }
        "sms" => {
            let password = secrets.require("sms")?;
            sms::send(payload, config, &password, lang).await
        }
        "radio" => bail!(crate::i18n::radio_not_in_server(lang)),
        other => bail!(crate::i18n::unknown_channel_named(lang, other)),
    }
}
