// =====================================================================
// channels/smtp.rs
// E-post.
//
// Blockerande, körs genom spawn_blocking. lettre startar ingen egen
// runtime, så det är säkert.
//
// Portad oförändrad från desktopvariantens lib.rs.
// =====================================================================

use anyhow::{bail, Context, Result};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use serde::Deserialize;

use super::AlarmPayload;

#[derive(Deserialize)]
struct SmtpConfig {
    host: String,
    port: u16,
    /// "ssl", "starttls" eller något annat för okrypterat.
    security: String,
    from: String,
    to: String,
    #[serde(default)]
    user: String,
}

pub fn send(payload: &str, config: &str, password: &str, lang: crate::i18n::Lang) -> Result<()> {
    let cfg: SmtpConfig = serde_json::from_str(config).context(crate::i18n::smtp_invalid_config(lang))?;
    let a: AlarmPayload = serde_json::from_str(payload).context(crate::i18n::invalid_alarm(lang))?;

    if cfg.host.trim().is_empty() {
        bail!(crate::i18n::smtp_no_host(lang));
    }

    let from: Mailbox = cfg
        .from
        .parse()
        .map_err(|e: lettre::address::AddressError| {
            anyhow::anyhow!(crate::i18n::smtp_invalid_from(lang, &e.to_string()))
        })?;
    let to: Mailbox = cfg
        .to
        .parse()
        .map_err(|e: lettre::address::AddressError| {
            anyhow::anyhow!(crate::i18n::smtp_invalid_to(lang, &e.to_string()))
        })?;

    let subject = if a.message.trim().is_empty() {
        format!("NetFyr: {} {}", a.device, a.status)
    } else {
        format!("NetFyr: {}", a.message)
    };

    let body = crate::i18n::smtp_body(lang, &a.message, &a.device, &a.address, &a.status, &a.time);

    let email = Message::builder()
        .from(from)
        .to(to)
        .subject(subject)
        .body(body)
        .context(crate::i18n::smtp_build_message(lang))?;

    let mut builder = match cfg.security.as_str() {
        "ssl" => SmtpTransport::relay(&cfg.host)?,
        "starttls" => SmtpTransport::starttls_relay(&cfg.host)?,
        _ => SmtpTransport::builder_dangerous(&cfg.host),
    }
    .port(cfg.port);

    if !cfg.user.trim().is_empty() {
        builder = builder.credentials(Credentials::new(cfg.user.clone(), password.to_string()));
    }

    builder.build().send(&email).context(crate::i18n::smtp_send_failed(lang))?;
    Ok(())
}
