// =====================================================================
// channels/webhook.rs
// HTTP-anrop till valfri mottagare.
//
// Adressen ligger i hemlighetslagret snarare än i konfigurationen —
// den innehåller ofta en token i sökvägen och ska inte hamna i
// exporter eller loggar.
// =====================================================================

use anyhow::{bail, Context, Result};
use std::time::Duration;

pub async fn send(url: &str, payload: &str, lang: crate::i18n::Lang) -> Result<()> {
    if url.trim().is_empty() {
        bail!(crate::i18n::webhook_no_url(lang));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context(crate::i18n::sms_http_client(lang))?;

    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .send()
        .await
        .context(crate::i18n::webhook_unreachable(lang))?;

    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        bail!("HTTP {}", status.as_u16())
    }
}
