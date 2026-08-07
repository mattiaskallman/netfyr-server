// =====================================================================
// api/settings.rs
// Globala inställningar och kanalkonfiguration.
//
// Inställningar lagras som nyckel/värde. Uppdateringen är partiell —
// bara skickade nycklar rörs, så två samtidiga klienter inte skriver
// över varandras ändringar i onödan.
// =====================================================================

use axum::extract::{ConnectInfo, Path, State};
use axum::Extension;
use axum::Json;
use serde::Serialize;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::engine::repo;
use crate::routes::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub live: bool,
    pub alarms: bool,
    pub sweep_interval_sec: u64,
    pub fail_period_sec: u32,
    pub success_period_sec: u32,
    pub packet_size: u32,
    pub slow_threshold_ms: u32,
    pub ping_timeout_sec: u64,
    /// Latenslarm (etapp 8): larma när svarstiderna legat över
    /// tröskeln tillräckligt länge. Av som standard.
    pub slow_alarm: bool,
    pub slow_alarm_sec: u32,
    pub channels: Vec<String>,
    /// Om SMS-gatewayens statusruta syns i sidopanelen. En ren
    /// gränssnittsinställning — den styr inget i motorn.
    pub show_sms_rail: bool,
    /// Om motorkortet syns i sidopanelen. Samma sorts ren
    /// gränssnittsinställning.
    pub show_engine_rail: bool,
    /// Vakthunden: TCP-port som svarar medan bevakningen pågår
    /// (desktopens heartbeat). Standardvärdena speglar desktop.
    pub watchdog_enabled: bool,
    pub watchdog_port: u32,
    pub watchdog_grace_min: u32,
    /// Motorns språk — "sv" eller "en". Styr API-fel, händelseloggen,
    /// leveransfel och SMS-standardtexter. Gränssnittets eget språkval
    /// är personligt per webbläsare och lagras inte här.
    pub lang: String,
}

/// Läser en rå gränssnittsnyckel ur nyckel/värde-tabellen.
/// Saknas nyckeln är standard att rutan visas.
fn rail_visible(raw: &std::collections::HashMap<String, String>, key: &str) -> bool {
    raw.get(key).map(|v| v != "0" && v != "false").unwrap_or(true)
}

/// Tolkar en numerisk inställning med desktopens gränser. Ett trasigt
/// eller tomt värde faller tillbaka på standard — samma beteende som
/// desktopens clampInt, och ett skydd mot att en blank sparning från
/// ett formulär nollställer funktionen.
fn clamp_u32(raw: &std::collections::HashMap<String, String>, key: &str, fallback: u32, min: u32, max: u32) -> u32 {
    raw.get(key)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|v| v.clamp(min, max))
        .unwrap_or(fallback)
}

pub async fn get(State(state): State<Arc<AppState>>) -> ApiResult<Json<SettingsView>> {
    let s = state.db.call(repo::load_settings).await?;

    // UI-inställningarna ligger utanför motorns typade Settings-struct —
    // de läses rått ur nyckel/värde-tabellen.
    let raw = state
        .db
        .call(|conn| {
            let mut stmt = conn.prepare(
                "SELECT key, value FROM settings WHERE key IN (
                    'showSmsRail', 'showEngineRail',
                    'watchdogEnabled', 'watchdogPort', 'watchdogGraceMin',
                    'lang'
                )",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut map = std::collections::HashMap::new();
            for row in rows {
                let (k, v) = row?;
                map.insert(k, v);
            }
            Ok(map)
        })
        .await?;
    let show_sms_rail = rail_visible(&raw, "showSmsRail");
    let show_engine_rail = rail_visible(&raw, "showEngineRail");
    // Vakthunden är AV som standard, precis som på desktop — en
    // lyssnande port ska vara ett aktivt val, inte något som dyker
    // upp vid uppgradering.
    let watchdog_enabled = raw
        .get("watchdogEnabled")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    // Samma gränser som desktop: port 1024-65535, respit 0-1440 min.
    let watchdog_port = clamp_u32(&raw, "watchdogPort", 7999, 1024, 65535);
    let watchdog_grace_min = clamp_u32(&raw, "watchdogGraceMin", 30, 0, 1440);
    // Saknas nyckeln är svenska standard — beteendet före i18n.
    let lang = raw
        .get("lang")
        .map(|v| crate::i18n::Lang::from_setting(v).as_str().to_string())
        .unwrap_or_else(|| "sv".to_string());

    Ok(Json(SettingsView {
        live: s.live,
        alarms: s.alarms,
        sweep_interval_sec: s.sweep_interval_sec,
        fail_period_sec: s.fail_period_sec,
        success_period_sec: s.success_period_sec,
        packet_size: s.packet_size,
        slow_threshold_ms: s.slow_threshold_ms,
        ping_timeout_sec: s.ping_timeout_sec,
        slow_alarm: s.slow_alarm,
        slow_alarm_sec: s.slow_alarm_sec,
        channels: s.channels,
        show_sms_rail,
        show_engine_rail,
        watchdog_enabled,
        watchdog_port,
        watchdog_grace_min,
        lang,
    }))
}

/// Godkända nycklar.
///
/// En vitlista i stället för fritt skrivande: annars kan en felstavning
/// tyst skapa en inställning som aldrig läses, och felet syns först när
/// någon undrar varför ändringen inte fick effekt.
const ALLOWED: &[&str] = &[
    "live",
    "alarms",
    "sweepIntervalSec",
    "flapFailSec",
    "flapSuccessSec",
    "packetSize",
    "slowThresholdMs",
    "pingTimeoutSec",
    // Latenslarm (etapp 8): på/av + sammanhängande tid över tröskeln.
    "slowAlarm",
    "slowAlarmSec",
    "channels",
    // Gränssnittsinställning: visa SMS-gatewayens statusruta i sidopanelen.
    "showSmsRail",
    // Gränssnittsinställning: visa motorkortet i sidopanelen.
    "showEngineRail",
    // Vakthunden (desktopens heartbeat): TCP-port uppe medan
    // bevakningen pågår, med respit vid paus.
    "watchdogEnabled",
    "watchdogPort",
    "watchdogGraceMin",
    // Motorns språk ("sv"/"en") — API-fel, händelser, leveransfel,
    // SMS-standardtexter. Se i18n.rs.
    "lang",
];

pub async fn update(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<BTreeMap<String, String>>,
) -> ApiResult<Json<SettingsView>> {
    // Felmeddelandena följer det språk som gäller NU — ändrar just
    // den här requesten "lang" slår det igenom på nästa anrop.
    let lang = crate::i18n::load_db(&state.db).await;
    for key in body.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return Err(ApiError::bad_request(crate::i18n::unknown_setting(lang, key)));
        }
    }
    if let Some(v) = body.get("lang") {
        if !crate::i18n::Lang::valid(v) {
            return Err(ApiError::bad_request(crate::i18n::lang_invalid(lang)));
        }
    }

    let keys: Vec<String> = body.keys().cloned().collect();
    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();

    state
        .db
        .call(move |conn| {
            for (k, v) in &body {
                repo::set_setting(conn, k, v)?;
            }
            Ok(())
        })
        .await?;

    // Värdena loggas inte — "channels" kan lista mottagare, och en
    // auditlogg ska aldrig bli en sekundär hemlighetsbutik.
    super::audit::record(
        &state.db,
        &actor_name,
        "settings_update",
        None,
        Some(&format!("nycklar: {}", keys.join(", "))),
        Some(&ip),
    )
    .await;

    get(State(state)).await
}

// ---- Kanalkonfiguration ----------------------------------------------

/// Konfigurationen för en kanal, som fri JSON.
///
/// Servern tolkar den inte — det gör kanalen vid sändning. Att validera
/// här skulle betyda att kanalernas fältdefinitioner fanns på två
/// ställen, och de skulle glida isär.
pub async fn get_channel(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let key = format!("channel.{name}.config");
    let raw = state
        .db
        .call(move |conn| {
            use rusqlite::OptionalExtension;
            let v: Option<String> = conn
                .query_row("SELECT value FROM settings WHERE key = ?1", [&key], |r| r.get(0))
                .optional()?;
            Ok(v)
        })
        .await?;

    let value = raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(Json(value))
}

pub async fn set_channel(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<()>> {
    if !body.is_object() {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::bad_request(crate::i18n::config_not_object(lang)));
    }
    let key = format!("channel.{name}.config");
    let value = body.to_string();
    state
        .db
        .call(move |conn| repo::set_setting(conn, &key, &value))
        .await?;

    super::audit::record(
        &state.db,
        &actor.username,
        "channel_config",
        Some(&name),
        None,
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(()))
}
