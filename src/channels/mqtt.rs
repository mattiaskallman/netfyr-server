// =====================================================================
// channels/mqtt.rs
// Publicering till en MQTT-broker.
//
// Blockerande, körs genom spawn_blocking. rumqttc:s synkrona Client
// startar ingen egen runtime.
//
// Portad oförändrad från desktopvariantens lib.rs.
// =====================================================================

use anyhow::{bail, Context, Result};
use rumqttc::{Client, Event, MqttOptions, Outgoing, QoS};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct MqttConfig {
    host: String,
    port: u16,
    topic: String,
    #[serde(default)]
    user: String,
    #[serde(default, rename = "clientId")]
    client_id: String,
}

pub fn send(payload: &str, config: &str, password: &str, lang: crate::i18n::Lang) -> Result<()> {
    let cfg: MqttConfig = serde_json::from_str(config).context(crate::i18n::mqtt_invalid_config(lang))?;

    if cfg.host.trim().is_empty() {
        bail!(crate::i18n::mqtt_no_host(lang));
    }
    if cfg.topic.trim().is_empty() {
        bail!(crate::i18n::mqtt_no_topic(lang));
    }

    let client_id = if cfg.client_id.trim().is_empty() {
        "netfyr".to_string()
    } else {
        cfg.client_id.clone()
    };

    let mut opts = MqttOptions::new(client_id, cfg.host.clone(), cfg.port);
    opts.set_keep_alive(Duration::from_secs(5));
    if !cfg.user.trim().is_empty() {
        opts.set_credentials(cfg.user.clone(), password.to_string());
    }

    let (client, mut connection) = Client::new(opts, 10);
    client.publish(
        &cfg.topic,
        QoS::AtLeastOnce,
        false,
        payload.as_bytes().to_vec(),
    )?;
    client.disconnect()?;

    // Driv anslutningen tills publiceringen kvitterats och vi kopplat ner.
    // Taket skyddar mot en broker som aldrig svarar.
    for (i, notification) in connection.iter().enumerate() {
        match notification {
            Ok(Event::Outgoing(Outgoing::Disconnect)) => return Ok(()),
            Ok(_) => {}
            Err(e) => bail!(crate::i18n::mqtt_error(lang, &e.to_string())),
        }
        if i > 200 {
            break;
        }
    }
    Ok(())
}
