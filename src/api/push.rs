// =====================================================================
// api/push.rs
// Web Push: status, aktivering av kanalen och prenumerationshantering.
//
// Kanalens på/av läggs i den globala kanallistan (settings "channels",
// kommaseparerad — samma format motorn läser). Prenumerationerna ligger
// i "channel.push.config" som JSON (PushConfig i channels/push.rs).
// VAPID-nyckeln genereras vid första aktiveringen och ligger i
// secrets.enc — den skrivs aldrig om, för en ersatt nyckel ogiltig-
// förklarar tyst alla befintliga prenumerationer.
//
// Behörighet: status och prenumerera/avprenumerera för alla inloggade
// (det är per-enhetsval), kanalens på/av är admin (påverkar larmflödet
// för alla).
// =====================================================================

use axum::extract::{ConnectInfo, State};
use axum::{Extension, Json};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{ApiError, ApiResult, audit};
use crate::auth::AuthUser;
use crate::channels::push::{self, PushConfig, PushSubscription};
use crate::db::Db;
use crate::engine::repo;
use crate::routes::AppState;

const CONFIG_KEY: &str = "channel.push.config";
const MAX_SUBSCRIPTIONS: usize = 64;
const MAX_SUBSCRIPTIONS_PER_USER: usize = 8;

#[derive(Debug, PartialEq, Eq)]
enum SubscriptionAdmission {
    Add,
    Replace(usize),
    OwnedByOther,
    UserFull,
    GlobalFull,
}

fn subscription_admission(cfg: &PushConfig, endpoint: &str, owner: &str) -> SubscriptionAdmission {
    if let Some(index) = cfg
        .subscriptions
        .iter()
        .position(|existing| existing.endpoint == endpoint)
    {
        let existing = &cfg.subscriptions[index];
        return if existing.owner.is_empty() || existing.owner == owner {
            SubscriptionAdmission::Replace(index)
        } else {
            SubscriptionAdmission::OwnedByOther
        };
    }
    if cfg
        .subscriptions
        .iter()
        .filter(|existing| existing.owner == owner)
        .count()
        >= MAX_SUBSCRIPTIONS_PER_USER
    {
        SubscriptionAdmission::UserFull
    } else if cfg.subscriptions.len() >= MAX_SUBSCRIPTIONS {
        SubscriptionAdmission::GlobalFull
    } else {
        SubscriptionAdmission::Add
    }
}

enum SubscribeOutcome {
    Saved,
    Disabled,
    Full,
    OwnedByOther,
}

// ---- Databashjälpare --------------------------------------------------

async fn read_config(db: &Db, lang: crate::i18n::Lang) -> ApiResult<PushConfig> {
    let raw: Option<String> = db
        .call(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    [CONFIG_KEY],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await?;
    match raw {
        Some(v) => serde_json::from_str(&v)
            .map_err(|_| ApiError::bad_request(crate::i18n::push_config_corrupt(lang))),
        None => Ok(PushConfig::default()),
    }
}

/// Läser kanallistan ("channels" — kommaseparerad, som motorn tolkar den).
async fn channel_list(db: &Db) -> ApiResult<Vec<String>> {
    let raw: Option<String> = db
        .call(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM settings WHERE key = 'channels'",
                    [],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await?;
    Ok(raw
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

fn updated_channel_list(raw: &str, enabled: bool) -> String {
    let mut channels: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if enabled {
        if !channels.iter().any(|channel| channel == "push") {
            channels.push("push".to_string());
        }
    } else {
        channels.retain(|channel| channel != "push");
    }
    channels.join(",")
}

/// Atomisk read-modify-write av den globala kanallistan. Det förhindrar att
/// samtidiga adminändringar tappas mellan separata DB-closures.
async fn set_push_enabled(db: &Db, enabled: bool) -> ApiResult<()> {
    db.call(move |conn| {
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'channels'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let value = updated_channel_list(raw.as_deref().unwrap_or(""), enabled);
        repo::set_setting(conn, "channels", &value)
    })
    .await?;
    Ok(())
}

// ---- Status ------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushStatus {
    pub enabled: bool,
    pub public_key: Option<String>,
    pub subscriptions: usize,
}

pub async fn status(State(state): State<Arc<AppState>>) -> ApiResult<Json<PushStatus>> {
    let lang = crate::i18n::load_db(&state.db).await;
    let enabled = channel_list(&state.db).await?.iter().any(|c| c == "push");
    // Publika nyckeln är ingen hemlighet — den behövs i klienten för att
    // prenumerera, så alla inloggade roller får den.
    let public_key = push::public_key_b64(&state.secrets).ok();
    let subscriptions = read_config(&state.db, lang).await?.subscriptions.len();
    Ok(Json(PushStatus {
        enabled,
        public_key,
        subscriptions,
    }))
}

// ---- Kanal på/av (admin) ------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnableResult {
    pub public_key: String,
}

pub async fn enable(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> ApiResult<Json<EnableResult>> {
    let public_key = push::ensure_keypair(&state.secrets)?;

    set_push_enabled(&state.db, true).await?;

    // Se till att det finns en config att skriva prenumerationer i.
    let db = state.db.clone();
    db.call(|conn| {
        let exists: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [CONFIG_KEY],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            repo::set_setting(
                conn,
                CONFIG_KEY,
                &serde_json::to_string(&PushConfig::default())?,
            )?;
        }
        Ok(())
    })
    .await?;

    audit::record(
        &state.db,
        &user.username,
        "enable_push",
        Some("push"),
        None,
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(EnableResult { public_key }))
}

pub async fn disable(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> ApiResult<Json<serde_json::Value>> {
    set_push_enabled(&state.db, false).await?;

    // Nyckel och prenumerationer lämnas kvar — återaktivering ska inte
    // tvinga någon att prenumerera om.
    audit::record(
        &state.db,
        &user.username,
        "disable_push",
        Some("push"),
        None,
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- Prenumerationer (inloggad) -----------------------------------------

#[derive(Deserialize)]
pub struct SubscribeRequest {
    pub endpoint: String,
    pub keys: SubscribeKeys,
    #[serde(default)]
    pub label: String,
}

#[derive(Deserialize)]
pub struct SubscribeKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Deserialize)]
pub struct UnsubscribeRequest {
    pub endpoint: String,
}

/// Grundvalidering — värdet ska komma från webbläsarens egen
/// PushSubscription, men servern litar aldrig på klienten.
fn validate_subscription(req: &SubscribeRequest, lang: crate::i18n::Lang) -> ApiResult<()> {
    if req.endpoint.len() > 2048 || push::parse_push_endpoint(&req.endpoint).is_err() {
        return Err(ApiError::bad_request(crate::i18n::push_bad_endpoint(lang)));
    }

    let p256dh = push::b64url_decode(&req.keys.p256dh)
        .ok()
        .filter(|raw| raw.len() == 65 && raw.first() == Some(&0x04));
    if p256dh
        .as_deref()
        .and_then(|raw| p256::PublicKey::from_sec1_bytes(raw).ok())
        .is_none()
    {
        return Err(ApiError::bad_request(crate::i18n::push_bad_p256dh(lang)));
    }

    if push::b64url_decode(&req.keys.auth)
        .ok()
        .filter(|raw| raw.len() == 16)
        .is_none()
    {
        return Err(ApiError::bad_request(crate::i18n::push_bad_auth(lang)));
    }
    if req.label.len() > 100 || req.label.chars().any(|c| c.is_control()) {
        return Err(ApiError::bad_request(crate::i18n::push_bad_label(lang)));
    }
    Ok(())
}

pub async fn subscribe(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<SubscribeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let lang = crate::i18n::load_db(&state.db).await;
    validate_subscription(&req, lang)?;

    let endpoint = req.endpoint.clone();
    let audit_target = push::endpoint_log_target(&endpoint);
    let owner = user.username.clone();
    let sub = PushSubscription {
        endpoint: req.endpoint,
        p256dh: req.keys.p256dh,
        auth: req.keys.auth,
        label: req.label,
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        owner: owner.clone(),
    };

    let outcome = state
        .db
        .call(move |conn| {
            // Aktiveringskontroll och prenumerations-RMW sker i samma
            // serialiserade DB-closure, så disable kan inte interfolieras.
            let channels: Option<String> = conn
                .query_row(
                    "SELECT value FROM settings WHERE key = 'channels'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let enabled = channels
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .any(|channel| channel == "push");
            if !enabled {
                return Ok(SubscribeOutcome::Disabled);
            }

            let raw: Option<String> = conn
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    [CONFIG_KEY],
                    |r| r.get(0),
                )
                .optional()?;
            let mut cfg: PushConfig = match raw {
                Some(v) => serde_json::from_str(&v)?,
                None => PushConfig::default(),
            };
            match subscription_admission(&cfg, &sub.endpoint, &owner) {
                SubscriptionAdmission::Replace(index) => cfg.subscriptions[index] = sub,
                SubscriptionAdmission::Add => cfg.subscriptions.push(sub),
                SubscriptionAdmission::OwnedByOther => {
                    return Ok(SubscribeOutcome::OwnedByOther);
                }
                SubscriptionAdmission::UserFull | SubscriptionAdmission::GlobalFull => {
                    return Ok(SubscribeOutcome::Full);
                }
            }
            repo::set_setting(conn, CONFIG_KEY, &serde_json::to_string(&cfg)?)?;
            Ok(SubscribeOutcome::Saved)
        })
        .await?;

    match outcome {
        SubscribeOutcome::Saved => {}
        SubscribeOutcome::Disabled => {
            return Err(ApiError::bad_request(crate::i18n::push_not_enabled(lang)));
        }
        SubscribeOutcome::Full => {
            return Err(ApiError::bad_request(crate::i18n::push_too_many(lang)));
        }
        SubscribeOutcome::OwnedByOther => {
            return Err(ApiError::bad_request(crate::i18n::push_owned_by_other(
                lang,
            )));
        }
    }

    audit::record(
        &state.db,
        &user.username,
        "push_subscribe",
        Some(&audit_target),
        None,
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn unsubscribe(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<UnsubscribeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    // Idempotent: klienten kan redan ha tappat prenumerationen lokalt,
    // och ett borttag av en post som inte finns är inte ett fel.
    let endpoint = req.endpoint;
    let audit_target = push::endpoint_log_target(&endpoint);
    let owner = user.username.clone();
    state
        .db
        .call({
            let endpoint = endpoint.clone();
            move |conn| {
                let raw: Option<String> = conn
                    .query_row(
                        "SELECT value FROM settings WHERE key = ?1",
                        [CONFIG_KEY],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(v) = raw {
                    let mut cfg: PushConfig = serde_json::from_str(&v)?;
                    cfg.subscriptions.retain(|subscription| {
                        subscription.endpoint != endpoint
                            || (!subscription.owner.is_empty() && subscription.owner != owner)
                    });
                    repo::set_setting(conn, CONFIG_KEY, &serde_json::to_string(&cfg)?)?;
                }
                Ok(())
            }
        })
        .await?;

    audit::record(
        &state.db,
        &user.username,
        "push_unsubscribe",
        Some(&audit_target),
        None,
        Some(&addr.ip().to_string()),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_toggle_bevarar_övriga_kanaler_utan_duplikat() {
        assert_eq!(updated_channel_list("smtp,push,sms", true), "smtp,push,sms");
        assert_eq!(updated_channel_list("smtp,push,sms", false), "smtp,sms");
        assert_eq!(updated_channel_list("smtp,sms", true), "smtp,sms,push");
    }

    #[test]
    fn prenumeration_validerar_url_och_nyckellängder() {
        let valid = SubscribeRequest {
            endpoint: "https://fcm.googleapis.com/fcm/send/token".into(),
            keys: SubscribeKeys {
                p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
                auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
            },
            label: "Telefon".into(),
        };
        assert!(validate_subscription(&valid, crate::i18n::Lang::Sv).is_ok());

        let mut malformed = SubscribeRequest {
            endpoint: "https://".into(),
            ..valid
        };
        assert!(validate_subscription(&malformed, crate::i18n::Lang::Sv).is_err());
        malformed.endpoint = "https://fcm.googleapis.com/fcm/send/token".into();
        malformed.keys.auth = "AA".into();
        assert!(validate_subscription(&malformed, crate::i18n::Lang::Sv).is_err());
    }

    #[test]
    fn prenumerationer_är_ägda_och_kvoterade_per_användare() {
        let subscription = |endpoint: &str, owner: &str| PushSubscription {
            endpoint: endpoint.into(),
            p256dh: String::new(),
            auth: String::new(),
            label: String::new(),
            created_at: String::new(),
            owner: owner.into(),
        };
        let mut cfg = PushConfig::default();
        cfg.subscriptions
            .push(subscription("https://example.com/a", "alice"));
        assert_eq!(
            subscription_admission(&cfg, "https://example.com/a", "bob"),
            SubscriptionAdmission::OwnedByOther
        );
        assert!(matches!(
            subscription_admission(&cfg, "https://example.com/a", "alice"),
            SubscriptionAdmission::Replace(0)
        ));

        for i in 0..MAX_SUBSCRIPTIONS_PER_USER {
            cfg.subscriptions
                .push(subscription(&format!("https://example.com/{i}"), "bob"));
        }
        assert_eq!(
            subscription_admission(&cfg, "https://example.com/new", "bob"),
            SubscriptionAdmission::UserFull
        );
    }
}
