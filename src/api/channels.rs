// =====================================================================
// api/channels.rs
// Testa en larmkanal.
//
// Skickar ett testlarm direkt genom kanalen — samma kodväg som riktiga
// larm, men utanför kön. Desktopvariantens "Testa"-knappar.
//
// Bara admin: ett test avslöjar om kanalen når sin mottagare, och
// kanalen kan peka på externa system.
// =====================================================================

use axum::{
    Json,
    extract::{ConnectInfo, Extension, Path, State},
};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Arc;

use super::{ApiError, audit};
use crate::auth::AuthUser;
use crate::channels::{self, AlarmPayload};
use crate::routes::AppState;

#[derive(Serialize)]
pub struct TestResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn test(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
) -> Result<Json<TestResult>, ApiError> {
    let ip = addr.ip().to_string();
    let actor_name = user.username.clone();

    // Kända kanaler — samma lista som dispatchen i channels/mod.rs.
    let lang = crate::i18n::load_db(&state.db).await;
    if !["webhook", "smtp", "mqtt", "sms", "push"].contains(&name.as_str()) {
        return Err(ApiError::bad_request(crate::i18n::unknown_channel(lang)));
    }

    let config = state
        .db
        .call({
            let name = name.clone();
            move |conn| {
                use rusqlite::OptionalExtension;
                let key = format!("channel.{name}.config");
                let v: Option<String> = conn
                    .query_row("SELECT value FROM settings WHERE key = ?1", [&key], |r| {
                        r.get(0)
                    })
                    .optional()?;
                Ok(v.unwrap_or_else(|| "{}".to_string()))
            }
        })
        .await?;

    let payload = serde_json::to_string(&AlarmPayload {
        app: "netfyr".into(),
        device: crate::i18n::test_device(lang).into(),
        address: "127.0.0.1".into(),
        status: "down".into(),
        message: crate::i18n::test_message(lang).into(),
        latency_ms: None,
        time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        sms_recipients: vec![],
        sms_session_id: String::new(),
    })
    .map_err(anyhow::Error::from)?;

    let result = channels::send(&name, &payload, &config, &state.secrets, &state.db, lang).await;

    audit::record(
        &state.db,
        &actor_name,
        "test_channel",
        Some(&name),
        None,
        Some(&ip),
    )
    .await;

    match result {
        channels::SendOutcome::Delivered => Ok(Json(TestResult {
            ok: true,
            error: None,
        })),
        channels::SendOutcome::Retryable(e) | channels::SendOutcome::Terminal(e) => {
            Ok(Json(TestResult {
                ok: false,
                error: Some(format!("{e:#}")),
            }))
        }
    }
}

// ---- SMS-specifika endpoints (desktopens kanalkort) -------------------

/// Hämta kanalens konfiguration ur settings. Saknas den är svaret "{}".
async fn sms_config(state: &AppState) -> Result<String, ApiError> {
    state
        .db
        .call(|conn| {
            use rusqlite::OptionalExtension;
            let v: Option<String> = conn
                .query_row(
                    "SELECT value FROM settings WHERE key = 'channel.sms.config'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(v.unwrap_or_else(|| "{}".to_string()))
        })
        .await
        .map_err(|e: anyhow::Error| e.into())
}

/// Modemstatus från gatewayen. Returnerar alltid 200 med ett
/// statusobjekt — fel ligger i fältet `error`, precis som desktopens
/// motsvarighet, så gränssnittet alltid har något att visa.
///
/// Ingen auditpost här: sidopanelen pollar statusen var 30:e sekund,
/// och en ren läsning ska inte fylla loggen (det blev 500+ brusposter
/// per dygn). Verifieringen (sms_verify) auditloggas däremot — det är
/// en medveten åtgärd.
pub async fn sms_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::channels::sms::SmsStatus>, ApiError> {
    let config = sms_config(&state).await?;
    let password = state.secrets.get("sms").unwrap_or_default();
    let lang = crate::i18n::load_db(&state.db).await;
    let status = crate::channels::sms::status(&config, &password, lang).await;
    Ok(Json(status))
}

/// Behörighetskontroll: inloggning, modemstatus, läsrätt för inkorgen
/// och nätregistrering. Skickar inget SMS — en kontroll ska inte kosta
/// pengar.
pub async fn sms_verify(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<crate::channels::sms::SmsVerify>, ApiError> {
    let config = sms_config(&state).await?;
    let password = state.secrets.get("sms").unwrap_or_default();
    let lang = crate::i18n::load_db(&state.db).await;
    let result = crate::channels::sms::verify(&config, &password, lang).await;
    audit::record(
        &state.db,
        &user.username,
        "sms_verify",
        None,
        None,
        Some(&addr.ip().to_string()),
    )
    .await;
    Ok(Json(result))
}

/// Senaste eskaleringssessionerna, nyast först — kanalkortets
/// sessionslista.
pub async fn sms_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<crate::sms_engine::Session>>, ApiError> {
    let sessions = state
        .db
        .call(|conn| crate::sms_engine::list_recent(conn, 50))
        .await?;
    Ok(Json(sessions))
}
