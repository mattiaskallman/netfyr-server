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
pub mod push;
pub mod sms;
pub mod smtp;
pub mod webhook;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::secrets::Secrets;

#[derive(Debug)]
pub enum SendOutcome {
    Delivered,
    Retryable(anyhow::Error),
    /// Några mottagare har redan fått leveransen. Felet ska synas men ett
    /// kanalretry skulle duplicera notisen till de lyckade mottagarna.
    Terminal(anyhow::Error),
}

fn retryable(result: Result<()>) -> SendOutcome {
    match result {
        Ok(()) => SendOutcome::Delivered,
        Err(error) => SendOutcome::Retryable(error),
    }
}

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
    db: &Db,
    lang: crate::i18n::Lang,
) -> SendOutcome {
    let regular: Result<()> = match channel {
        "webhook" => {
            let url = match secrets.require("webhook") {
                Ok(url) => url,
                Err(error) => return SendOutcome::Retryable(error),
            };
            webhook::send(&url, payload, lang).await
        }
        "smtp" => {
            let password = match secrets.require("smtp") {
                Ok(password) => password,
                Err(error) => return SendOutcome::Retryable(error),
            };
            let (payload, config) = (payload.to_string(), config.to_string());
            match tokio::task::spawn_blocking(move || {
                smtp::send(&payload, &config, &password, lang)
            })
            .await
            {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            }
        }
        "mqtt" => {
            let password = secrets.get("mqtt").unwrap_or_default();
            let (payload, config) = (payload.to_string(), config.to_string());
            match tokio::task::spawn_blocking(move || {
                mqtt::send(&payload, &config, &password, lang)
            })
            .await
            {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            }
        }
        "sms" => {
            let password = match secrets.require("sms") {
                Ok(password) => password,
                Err(error) => return SendOutcome::Retryable(error),
            };
            sms::send(payload, config, &password, lang).await
        }
        "push" => {
            let report = match push::send(payload, config, secrets, lang).await {
                Ok(report) => report,
                Err(push::PushSendError::Permanent(error)) => {
                    return SendOutcome::Terminal(error);
                }
            };
            let expired = report.expired_endpoints.len();
            if expired > 0 {
                if let Err(error) = push::prune_expired(db, &report.expired_endpoints).await {
                    let error =
                        anyhow::anyhow!("push: kunde inte pruna utgångna prenumerationer: {error}");
                    return match push::classify_prune_failure(report.sent, report.permanent_failed)
                    {
                        push::DeliveryClass::Retryable => SendOutcome::Retryable(error),
                        push::DeliveryClass::Delivered | push::DeliveryClass::TerminalPartial => {
                            SendOutcome::Terminal(error)
                        }
                    };
                }
            }
            let class = push::classify_delivery(
                report.sent,
                report.transient_failed,
                report.permanent_failed,
                expired,
            );
            let error = || {
                report.last_error.unwrap_or_else(|| {
                    anyhow::anyhow!(
                        "push: ofullständig leverans (skickade={}, tillfälliga fel={}, permanenta fel={}, utgångna={expired})",
                        report.sent,
                        report.transient_failed,
                        report.permanent_failed
                    )
                })
            };
            return match class {
                push::DeliveryClass::Delivered => SendOutcome::Delivered,
                push::DeliveryClass::Retryable => SendOutcome::Retryable(error()),
                push::DeliveryClass::TerminalPartial => SendOutcome::Terminal(error()),
            };
        }
        "radio" => Err(anyhow::anyhow!(crate::i18n::radio_not_in_server(lang))),
        other => Err(anyhow::anyhow!(crate::i18n::unknown_channel_named(
            lang, other
        ))),
    };
    retryable(regular)
}
