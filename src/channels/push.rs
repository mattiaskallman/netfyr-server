// =====================================================================
// channels/push.rs
// Web Push-kanalen (RFC 8030).
//
// Prenumerationer ligger i settings-nyckeln "channel.push.config" som
// JSON — samma mönster som övriga kanaler, så kö/dispatch/export
// fungerar utan specialfall. VAPID-privatnyckeln ligger i secrets.enc
// under namnet "vapid" (base64url, 32 råa bytes); den publika nyckeln
// härleds vid behov och lagras aldrig separat.
//
// KRYPTON ÄR IMPLEMENTERAD DIREKT på ren Rust-krypto:
//  - RFC 8291: ECDH + HKDF-härledning av CEK/nonce
//  - RFC 8188: aes128gcm content coding (salt || rs || idlen || nyckel)
//  - RFC 8292: VAPID (ES256-JWT via jwt-simple)
// Orsaken är att web-push/ece-craterna drar in openssl, medan hela
// projektet annars är rustls/pure-rust. Korrektheten är förankrad i
// RFC 8291 Appendix A:s testvektorer — se testerna längst ned.
// =====================================================================

use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashSet;

use crate::db::Db;
use crate::engine::repo;
use crate::i18n::Lang;
use crate::secrets::Secrets;

type HmacSha256 = Hmac<Sha256>;

/// En webbläsarprenumeration (PushSubscription.toJSON() i klienten,
/// plattat — keys.p256dh/keys.auth lyfts upp i API-lagret).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// Valfri etikett, t.ex. "Tias iPhone" — visas i UI:t.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub created_at: String,
    /// Serverbestämt ägarskap för kvot och borttagning. Äldre poster utan
    /// fältet behandlas som legacy-capabilities och kan tas över vid upsert.
    #[serde(default)]
    pub owner: String,
}

/// Kanalens config-JSON i settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushConfig {
    /// VAPID "sub"-claim — kontakt-URI som push-tjänsten kan nå oss på.
    #[serde(default = "default_subject")]
    pub subject: String,
    #[serde(default)]
    pub subscriptions: Vec<PushSubscription>,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            subject: default_subject(),
            subscriptions: Vec::new(),
        }
    }
}

fn default_subject() -> String {
    "mailto:netfyr@localhost".into()
}

// ---------------------------------------------------------------------
// Base64url utan padding — det format Web Push använder overallt.
// ---------------------------------------------------------------------

pub fn b64url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn b64url_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        // Webbläsare skickar ibland paddat, ibland inte — tolerera båda.
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .context("ogiltig base64url")
}

pub fn validate_vapid_subject(subject: &str) -> Result<()> {
    if subject.is_empty() || subject.len() > 2048 || subject.trim() != subject {
        bail!("push: VAPID subject måste vara en giltig kontakt-URI");
    }
    let url = reqwest::Url::parse(subject)
        .map_err(|_| anyhow!("push: VAPID subject måste vara en giltig kontakt-URI"))?;
    let valid = match url.scheme() {
        "mailto" => {
            let Some((local, domain)) = url.path().split_once('@') else {
                return Err(anyhow!(
                    "push: VAPID subject måste vara mailto: eller https:"
                ));
            };
            !local.is_empty()
                && !domain.is_empty()
                && !domain.contains('@')
                && url.query().is_none()
                && url.fragment().is_none()
        }
        "https" => {
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        }
        _ => false,
    };
    if !valid {
        bail!("push: VAPID subject måste vara mailto: eller https:");
    }
    Ok(())
}

/// Säker logg-/auditrepresentation av en endpoint. Själva sökvägen och
/// queryn är en bearer-capability och får aldrig hamna i loggar.
pub fn parse_push_endpoint(endpoint: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| anyhow!("push: ogiltig endpoint"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
        || url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some()
    {
        bail!("push: endpoint måste vara en publik https-origin på port 443");
    }
    Ok(url)
}

fn ipv6_in_prefix(ip: std::net::Ipv6Addr, base: std::net::Ipv6Addr, bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    };
    (u128::from(ip) & mask) == (u128::from(base) & mask)
}

fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_ip(std::net::IpAddr::V4(v4));
            }
            // Konservativ allowlist för global unicast (2000::/3), med IANA:s
            // special-/översättnings-/dokumentationsprefix explicit borttagna.
            // Pushleverantörer använder normala globala unicast-adresser; det
            // finns ingen anledning att acceptera transitionstekniker här.
            let global_unicast = ipv6_in_prefix(ip, "2000::".parse().unwrap(), 3);
            let special = ipv6_in_prefix(ip, "64:ff9b::".parse().unwrap(), 96)
                || ipv6_in_prefix(ip, "64:ff9b:1::".parse().unwrap(), 48)
                || ipv6_in_prefix(ip, "100::".parse().unwrap(), 64)
                || ipv6_in_prefix(ip, "2001::".parse().unwrap(), 23)
                || ipv6_in_prefix(ip, "2001:db8::".parse().unwrap(), 32)
                || ipv6_in_prefix(ip, "2002::".parse().unwrap(), 16)
                || ipv6_in_prefix(ip, "3fff::".parse().unwrap(), 20);
            global_unicast && !special
        }
    }
}

const DNS_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn dns_lookup_with_timeout<F>(
    lookup: F,
    deadline: std::time::Duration,
) -> Result<Vec<std::net::SocketAddr>>
where
    F: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    tokio::time::timeout(deadline, lookup)
        .await
        .map_err(|_| anyhow!("push: DNS-uppslaget tog för lång tid"))?
        .map_err(|_| anyhow!("push: kunde inte slå upp push-tjänsten"))
}

async fn pinned_push_client(url: &reqwest::Url) -> Result<reqwest::Client> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("push: ogiltig endpoint"))?;
    let addrs = dns_lookup_with_timeout(
        async {
            tokio::net::lookup_host((host, 443))
                .await
                .map(|addresses| addresses.collect())
        },
        DNS_LOOKUP_TIMEOUT,
    )
    .await?;
    if addrs.is_empty() || addrs.iter().any(|addr| !is_public_ip(addr.ip())) {
        bail!("push: endpoint löser inte enbart till publika adresser");
    }
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addrs)
        .build()
        .map_err(|_| anyhow!("push: kunde inte skapa HTTP-klient"))
}

pub fn push_endpoint_origin(url: &reqwest::Url) -> String {
    url.origin().ascii_serialization()
}

pub fn endpoint_log_target(endpoint: &str) -> String {
    parse_push_endpoint(endpoint)
        .map(|url| push_endpoint_origin(&url))
        .unwrap_or_else(|_| "<invalid-push-endpoint>".to_string())
}

// ---------------------------------------------------------------------
// VAPID-nyckelhantering.
// ---------------------------------------------------------------------

fn vapid_public_key_b64(kp: &jwt_simple::algorithms::ES256KeyPair) -> String {
    let compressed = kp.public_key().to_bytes();
    let public_key = p256::PublicKey::from_sec1_bytes(&compressed)
        .expect("jwt-simple skapade en ogiltig P-256-nyckel");
    b64url_encode(public_key.to_encoded_point(false).as_bytes())
}

fn public_key_from_private_b64(priv_b64: &str) -> Result<String> {
    let raw = b64url_decode(priv_b64)?;
    let kp = jwt_simple::algorithms::ES256KeyPair::from_bytes(&raw)
        .map_err(|_| anyhow!("push: korrupt VAPID-nyckel i secrets"))?;
    Ok(vapid_public_key_b64(&kp))
}

/// Publik VAPID-nyckel (okomprimerad punkt, base64url) — det värde
/// webbläsarens pushManager.subscribe vill ha som applicationServerKey.
pub fn public_key_b64(secrets: &Secrets) -> Result<String> {
    let priv_b64 = secrets
        .get("vapid")
        .ok_or_else(|| anyhow!("push: VAPID-nyckel saknas — aktivera push först"))?;
    public_key_from_private_b64(&priv_b64)
}

/// Generera och persistiera högst ett nyckelpar även vid samtidiga enable.
/// Cachevärdet publiceras först när secrets.enc har skrivits atomiskt.
pub fn ensure_keypair(secrets: &Secrets) -> Result<String> {
    let priv_b64 = secrets.get_or_try_insert_with("vapid", || {
        let kp = jwt_simple::algorithms::ES256KeyPair::generate();
        Ok(b64url_encode(kp.to_bytes().as_ref()))
    })?;
    public_key_from_private_b64(&priv_b64)
}

/// Authorization-headern för ett sändningstillfälle:
/// `vapid t=<jwt>, k=<publik nyckel>` (RFC 8292 §3).
fn vapid_auth_header(secrets: &Secrets, endpoint: &str, subject: &str) -> Result<String> {
    use jwt_simple::prelude::*;

    validate_vapid_subject(subject)?;
    let priv_b64 = secrets
        .get("vapid")
        .ok_or_else(|| anyhow!("push: VAPID-nyckel saknas"))?;
    let raw = b64url_decode(&priv_b64)?;
    let kp = jwt_simple::algorithms::ES256KeyPair::from_bytes(&raw)
        .map_err(|_| anyhow!("push: korrupt VAPID-nyckel i secrets"))?;

    // aud = push-tjänstens kanoniska origin (scheme + host + ev. port).
    let endpoint_url = parse_push_endpoint(endpoint)?;
    let aud = push_endpoint_origin(&endpoint_url);

    // VAPID tillåter max 24 h framåt; 12 h marginal räcker gott.
    let claims = Claims::create(Duration::from_hours(12))
        .with_audience(&aud)
        .with_subject(subject);
    let jwt = kp
        .sign(claims)
        .map_err(|e| anyhow!("push: VAPID-signering misslyckades: {e}"))?;

    Ok(format!("vapid t={jwt}, k={}", public_key_b64(secrets)?))
}

// ---------------------------------------------------------------------
// Payloadkryptering (RFC 8291 + RFC 8188, aes128gcm).
// ---------------------------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key)
        .expect("HMAC-SHA256 tar vilken nyckellängd som helst");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// HKDF-Expand för ett enda block (alla våra utdata <= 32 byte):
/// T(1) = HMAC-SHA256(PRK, info || 0x01).
fn hkdf_expand1(prk: &[u8; 32], info: &[u8], len: usize) -> Vec<u8> {
    debug_assert!(len <= 32);
    let mut data = info.to_vec();
    data.push(0x01);
    hmac_sha256(prk, &data)[..len].to_vec()
}

/// Max payload för en enda record (rs=4096 minus 16 byte GCM-tag minus
/// 1 byte paddingdelimiter). Vår AlarmPayload-JSON ligger långt under.
const MAX_PLAINTEXT: usize = 4096 - 16 - 1;

/// Kryptera ett push-meddelande. Returnerar hela HTTP-body:n:
/// salt(16) || rs(4, BE) || idlen(1) || as_public(65) || ciphertext.
///
/// `test_key`/`test_salt` är None i produktion — de finns för att
/// testet ska kunna reproducera RFC 8291 Appendix A exakt.
fn encrypt_payload(
    ua_public_b64: &str,
    auth_b64: &str,
    plaintext: &[u8],
    test_key: Option<&[u8]>,
    test_salt: Option<[u8; 16]>,
) -> Result<Vec<u8>> {
    use aes_gcm::{Aes128Gcm, KeyInit, aead::Aead};
    use rand_core::RngCore;

    if plaintext.len() > MAX_PLAINTEXT {
        bail!(
            "push: payload för stor ({} byte, max {MAX_PLAINTEXT})",
            plaintext.len()
        );
    }

    let ua_public_bytes = b64url_decode(ua_public_b64)?;
    let auth = b64url_decode(auth_b64)?;
    let ua_public = p256::PublicKey::from_sec1_bytes(&ua_public_bytes)
        .map_err(|_| anyhow!("push: ogiltig p256dh i prenumerationen"))?;

    // Efemärt applikationsserver-nyckelpar per meddelande (RFC 8291 §3.1).
    let secret_key = match test_key {
        Some(raw) => {
            p256::SecretKey::from_slice(raw).map_err(|_| anyhow!("push: ogiltig testnyckel"))?
        }
        None => p256::SecretKey::random(&mut rand_core::OsRng),
    };
    let as_public_bytes = secret_key
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();

    let shared = p256::ecdh::diffie_hellman(secret_key.to_nonzero_scalar(), ua_public.as_affine());
    let ecdh_secret = shared.raw_secret_bytes();

    // RFC 8291 §3.3: kombinera ECDH-hemligheten med auth-secret.
    // key_info = "WebPush: info" || 0x00 || ua_public || as_public
    let mut key_info = b"WebPush: info".to_vec();
    key_info.push(0x00);
    key_info.extend_from_slice(&ua_public_bytes);
    key_info.extend_from_slice(&as_public_bytes);
    let prk_key = hmac_sha256(&auth, ecdh_secret.as_slice());
    let ikm = hkdf_expand1(&prk_key, &key_info, 32);

    // RFC 8188 §2.3: CEK och nonce ur salt + IKM.
    let salt: [u8; 16] = match test_salt {
        Some(s) => s,
        None => {
            let mut s = [0u8; 16];
            rand_core::OsRng.fill_bytes(&mut s);
            s
        }
    };
    let prk = hmac_sha256(&salt, &ikm);
    let cek = hkdf_expand1(&prk, b"Content-Encoding: aes128gcm\x00", 16);
    let nonce = hkdf_expand1(&prk, b"Content-Encoding: nonce\x00", 12);

    // En enda record: plaintext || 0x02 (slutmarkör), AES-128-GCM.
    // Sekvensnummer 0 => nonce används oförändrad (RFC 8188 §2.3).
    let mut padded = plaintext.to_vec();
    padded.push(0x02);
    let cipher = Aes128Gcm::new_from_slice(&cek).expect("CEK är alltid 16 byte");
    let ciphertext = cipher
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), padded.as_ref())
        .map_err(|_| anyhow!("push: kryptering misslyckades"))?;

    let mut body = Vec::with_capacity(86 + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65u8);
    body.extend_from_slice(&as_public_bytes);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

// ---------------------------------------------------------------------
// Kanalgränssnittet — samma form som övriga kanaler.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryClass {
    Delivered,
    Retryable,
    TerminalPartial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpFailureClass {
    Expired,
    Transient,
    Permanent,
}

fn classify_http_failure(status: u16) -> HttpFailureClass {
    match status {
        404 | 410 => HttpFailureClass::Expired,
        408 | 429 | 500..=599 => HttpFailureClass::Transient,
        _ => HttpFailureClass::Permanent,
    }
}

#[derive(Debug)]
pub struct DeliveryReport {
    pub sent: usize,
    pub transient_failed: usize,
    pub permanent_failed: usize,
    pub expired_endpoints: Vec<String>,
    pub last_error: Option<anyhow::Error>,
}

pub fn classify_prune_failure(sent: usize, permanent_failed: usize) -> DeliveryClass {
    if sent > 0 || permanent_failed > 0 {
        DeliveryClass::TerminalPartial
    } else {
        DeliveryClass::Retryable
    }
}

pub fn classify_delivery(
    sent: usize,
    transient_failed: usize,
    permanent_failed: usize,
    expired: usize,
) -> DeliveryClass {
    if permanent_failed > 0 {
        // Ett identiskt helkanalsförsök kan inte reparera permanenta svar.
        // Gör därför utfallet terminalt även om andra mottagare hade nätfel.
        DeliveryClass::TerminalPartial
    } else if transient_failed > 0 {
        if sent == 0 {
            DeliveryClass::Retryable
        } else {
            // Några mottagare har redan fått notisen. Ett kanalretry skulle
            // duplicera den, så utfallet är terminalt och synligt i kön.
            DeliveryClass::TerminalPartial
        }
    } else if sent > 0 {
        // Utgångna 404/410-prenumerationer prunas; de räknas inte som en
        // retrybar mottagare när minst en aktiv prenumeration lyckades.
        DeliveryClass::Delivered
    } else if expired > 0 {
        DeliveryClass::TerminalPartial
    } else {
        DeliveryClass::Retryable
    }
}

/// Ta bort exakt de capability-endpoints som push-tjänsten svarat 404/410 på.
/// Read-modify-write körs i DB-aktörens enda closure och kan därför inte
/// skriva över en samtidig subscribe/unsubscribe.
pub async fn prune_expired(db: &Db, endpoints: &[String]) -> Result<usize> {
    if endpoints.is_empty() {
        return Ok(0);
    }
    let expired: HashSet<String> = endpoints.iter().cloned().collect();
    db.call(move |conn| {
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'channel.push.config'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(0);
        };
        let mut cfg: PushConfig = serde_json::from_str(&raw)?;
        let before = cfg.subscriptions.len();
        cfg.subscriptions
            .retain(|subscription| !expired.contains(&subscription.endpoint));
        let removed = before - cfg.subscriptions.len();
        if removed > 0 {
            repo::set_setting(conn, "channel.push.config", &serde_json::to_string(&cfg)?)?;
        }
        Ok(removed)
    })
    .await
}

/// Skicka ett larm till varje prenumeration och returnera ett per-mottagarutfall.
#[derive(Debug)]
pub enum PushSendError {
    // Alla fel före fan-out är lokala konfigurationsfel. Nät/DNS uppstår
    // först per prenumeration och klassas i DeliveryReport.
    Permanent(anyhow::Error),
}

fn prepare_config_for_send(
    config: &str,
    lang: Lang,
) -> std::result::Result<PushConfig, PushSendError> {
    let cfg: PushConfig = serde_json::from_str(config)
        .map_err(|_| PushSendError::Permanent(anyhow!(crate::i18n::push_bad_config(lang))))?;
    validate_vapid_subject(&cfg.subject).map_err(PushSendError::Permanent)?;
    if cfg.subscriptions.is_empty() {
        return Err(PushSendError::Permanent(anyhow!(
            crate::i18n::push_no_subscriptions(lang)
        )));
    }
    Ok(cfg)
}

/// Skicka till samtliga prenumerationer och bevara utfallet per fan-out.
pub async fn send(
    payload: &str,
    config: &str,
    secrets: &Secrets,
    lang: Lang,
) -> std::result::Result<DeliveryReport, PushSendError> {
    let cfg = prepare_config_for_send(config, lang)?;

    let urgency = if serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("status")?.as_str().map(str::to_owned))
        .as_deref()
        == Some("down")
    {
        "high"
    } else {
        "normal"
    };

    let mut report = DeliveryReport {
        sent: 0,
        transient_failed: 0,
        permanent_failed: 0,
        expired_endpoints: Vec::new(),
        last_error: None,
    };

    for sub in &cfg.subscriptions {
        match send_one(secrets, &cfg.subject, sub, payload, urgency, lang).await {
            Ok(()) => report.sent += 1,
            Err(SendOneError::Expired(error)) => {
                tracing::warn!(
                    push_service = %endpoint_log_target(&sub.endpoint),
                    error = %error,
                    "push: utgången prenumeration prunas"
                );
                report.expired_endpoints.push(sub.endpoint.clone());
            }
            Err(SendOneError::Transient(error)) => {
                report.transient_failed += 1;
                tracing::warn!(
                    push_service = %endpoint_log_target(&sub.endpoint),
                    error = %error,
                    "push: tillfälligt sändningsfel"
                );
                report.last_error = Some(error);
            }
            Err(SendOneError::Permanent(error)) => {
                report.permanent_failed += 1;
                tracing::warn!(
                    push_service = %endpoint_log_target(&sub.endpoint),
                    error = %error,
                    "push: permanent sändningsfel"
                );
                report.last_error = Some(error);
            }
        }
    }

    Ok(report)
}

#[derive(Debug)]
enum SendOneError {
    Expired(anyhow::Error),
    Transient(anyhow::Error),
    Permanent(anyhow::Error),
}

async fn send_one(
    secrets: &Secrets,
    subject: &str,
    sub: &PushSubscription,
    payload: &str,
    urgency: &str,
    lang: Lang,
) -> std::result::Result<(), SendOneError> {
    let permanent = |error| SendOneError::Permanent(error);
    let endpoint = parse_push_endpoint(&sub.endpoint).map_err(permanent)?;
    let client = pinned_push_client(&endpoint)
        .await
        .map_err(SendOneError::Transient)?;
    let body = encrypt_payload(&sub.p256dh, &sub.auth, payload.as_bytes(), None, None)
        .map_err(SendOneError::Permanent)?;
    let auth_header =
        vapid_auth_header(secrets, endpoint.as_str(), subject).map_err(SendOneError::Permanent)?;

    let resp = client
        .post(endpoint)
        .header("TTL", "3600")
        .header("Urgency", urgency)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", auth_header)
        .body(body)
        .send()
        .await
        .map_err(|_| SendOneError::Transient(anyhow!("push: nätverksfel mot push-tjänsten")))?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let error = || anyhow!("push: push-tjänsten avvisade sändningen ({status})");
    match classify_http_failure(status.as_u16()) {
        HttpFailureClass::Expired => Err(SendOneError::Expired(anyhow!(
            crate::i18n::push_subscription_expired(lang, status.as_u16())
        ))),
        HttpFailureClass::Transient => Err(SendOneError::Transient(error())),
        HttpFailureClass::Permanent => Err(SendOneError::Permanent(error())),
    }
}

// ---------------------------------------------------------------------
// Tester: RFC 8291 Appendix A utgör kända svar för hela kedjan —
// ECDH, HKDF-härledning, header och slutlig ciphertext. Om dessa
// matcherar är krypteringen definitionsmässigt kompatibel med varje
// standardpush-tjänst.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Värden från RFC 8291 Appendix A (whitespace borttaget).
    const PLAINTEXT_B64: &str = "V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24";
    const AS_PRIVATE_B64: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const AS_PUBLIC_B64: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const UA_PUBLIC_B64: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH_SECRET_B64: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const SALT_B64: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const EXPECTED_HEADER_B64: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const EXPECTED_CIPHERTEXT_B64: &str =
        "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ";

    #[test]
    fn rfc8291_appendix_a_known_answer() {
        let plaintext = b64url_decode(PLAINTEXT_B64).unwrap();
        let as_private = b64url_decode(AS_PRIVATE_B64).unwrap();
        let salt: [u8; 16] = b64url_decode(SALT_B64).unwrap().try_into().unwrap();

        let body = encrypt_payload(
            UA_PUBLIC_B64,
            AUTH_SECRET_B64,
            &plaintext,
            Some(&as_private),
            Some(salt),
        )
        .unwrap();

        let expected_header = b64url_decode(EXPECTED_HEADER_B64).unwrap();
        let expected_ciphertext = b64url_decode(EXPECTED_CIPHERTEXT_B64).unwrap();

        // Headern (86 byte) och ciphertext ska matcha RFC:n exakt.
        assert_eq!(&body[..86], expected_header.as_slice(), "RFC 8188-header");
        assert_eq!(
            &body[86..],
            expected_ciphertext.as_slice(),
            "AES-128-GCM-ciphertext"
        );
    }

    #[test]
    fn mellanleden_stämmer() {
        // Verifiera varje härledningssteg mot RFC:ns mellanvärden — vid
        // framtida regressioner pekar detta ut exakt vilket steg som bröts.
        let ua_public_bytes = b64url_decode(UA_PUBLIC_B64).unwrap();
        let as_private = b64url_decode(AS_PRIVATE_B64).unwrap();
        let auth = b64url_decode(AUTH_SECRET_B64).unwrap();
        let salt: [u8; 16] = b64url_decode(SALT_B64).unwrap().try_into().unwrap();

        let sk = p256::SecretKey::from_slice(&as_private).unwrap();
        let as_public_bytes = sk.public_key().to_encoded_point(false).as_bytes().to_vec();
        assert_eq!(b64url_encode(&as_public_bytes), AS_PUBLIC_B64, "as_public");

        let ua_public = p256::PublicKey::from_sec1_bytes(&ua_public_bytes).unwrap();
        let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), ua_public.as_affine());
        assert_eq!(
            b64url_encode(shared.raw_secret_bytes().as_slice()),
            "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs",
            "ecdh_secret"
        );

        let mut key_info = b"WebPush: info".to_vec();
        key_info.push(0x00);
        key_info.extend_from_slice(&ua_public_bytes);
        key_info.extend_from_slice(&as_public_bytes);
        let prk_key = hmac_sha256(&auth, shared.raw_secret_bytes().as_slice());
        assert_eq!(
            b64url_encode(&prk_key),
            "Snr3JMxaHVDXHWJn5wdC52WjpCtd2EIEGBykDcZW32k",
            "PRK_key"
        );
        let ikm = hkdf_expand1(&prk_key, &key_info, 32);
        assert_eq!(
            b64url_encode(&ikm),
            "S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg",
            "IKM"
        );

        let prk = hmac_sha256(&salt, &ikm);
        assert_eq!(
            b64url_encode(&prk),
            "09_eUZGrsvxChDCGRCdkLiDXrReGOEVeSCdCcPBSJSc",
            "PRK"
        );
        let cek = hkdf_expand1(&prk, b"Content-Encoding: aes128gcm\x00", 16);
        assert_eq!(b64url_encode(&cek), "oIhVW04MRdy2XN9CiKLxTg", "CEK");
        let nonce = hkdf_expand1(&prk, b"Content-Encoding: nonce\x00", 12);
        assert_eq!(b64url_encode(&nonce), "4h_95klXJ5E_qnoN", "NONCE");
    }

    #[test]
    fn för_stor_payload_avvisas() {
        let big = vec![b'x'; MAX_PLAINTEXT + 1];
        let err = encrypt_payload(UA_PUBLIC_B64, AUTH_SECRET_B64, &big, None, None);
        assert!(err.is_err());
    }

    #[test]
    fn ogiltig_p256dh_avvisas() {
        let err = encrypt_payload("inteennyckel", AUTH_SECRET_B64, b"hej", None, None);
        assert!(err.is_err());
    }

    #[test]
    fn config_parses_tom_default() {
        let cfg: PushConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.subject, "mailto:netfyr@localhost");
        assert!(cfg.subscriptions.is_empty());
    }

    #[test]
    fn log_target_döljer_push_endpointens_capability() {
        let endpoint = "https://fcm.googleapis.com/fcm/send/hemlig-token?auth=ännu-hemligare";
        let target = endpoint_log_target(endpoint);
        assert_eq!(target, "https://fcm.googleapis.com");
        assert!(!target.contains("hemlig"));
        assert!(!target.contains("token"));
    }

    #[tokio::test]
    async fn dns_uppslag_har_egen_deadline() {
        let never = std::future::pending::<std::io::Result<Vec<std::net::SocketAddr>>>();
        let result = dns_lookup_with_timeout(never, std::time::Duration::from_millis(1)).await;
        assert!(result.is_err());
    }

    #[test]
    fn vapid_subject_måste_vara_giltig_kontakt_uri() {
        for valid in [
            "mailto:netfyr@localhost",
            "mailto:ops@example.com",
            "https://example.com/contact",
        ] {
            assert!(validate_vapid_subject(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "ops@example.com",
            "mailto:",
            "mailto:not-an-address",
            "http://example.com",
            "https://user:pass@example.com/contact",
            "javascript:alert(1)",
        ] {
            assert!(validate_vapid_subject(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn http_status_skils_mellan_utgången_transient_och_permanent() {
        for status in [404, 410] {
            assert_eq!(classify_http_failure(status), HttpFailureClass::Expired);
        }
        for status in [408, 429, 500, 502, 503, 599] {
            assert_eq!(classify_http_failure(status), HttpFailureClass::Transient);
        }
        for status in [300, 301, 400, 401, 403, 413, 422] {
            assert_eq!(classify_http_failure(status), HttpFailureClass::Permanent);
        }
    }

    #[test]
    fn top_level_pushkonfigfel_är_permanenta() {
        assert!(matches!(
            prepare_config_for_send("{", Lang::Sv),
            Err(PushSendError::Permanent(_))
        ));
        assert!(matches!(
            prepare_config_for_send(
                r#"{"subject":"mailto:ops@example.com","subscriptions":[]}"#,
                Lang::Sv
            ),
            Err(PushSendError::Permanent(_))
        ));
    }

    #[test]
    fn prune_fel_respekterar_redan_permanent_eller_levererat_utfall() {
        assert_eq!(classify_prune_failure(0, 0), DeliveryClass::Retryable);
        assert_eq!(classify_prune_failure(0, 1), DeliveryClass::TerminalPartial);
        assert_eq!(classify_prune_failure(1, 0), DeliveryClass::TerminalPartial);
    }

    #[test]
    fn permanenta_pushfel_retryas_aldrig() {
        assert_eq!(
            classify_delivery(0, 0, 1, 0),
            DeliveryClass::TerminalPartial
        );
        assert_eq!(classify_delivery(0, 1, 0, 0), DeliveryClass::Retryable);
        assert_eq!(
            classify_delivery(1, 1, 0, 0),
            DeliveryClass::TerminalPartial
        );
    }

    #[test]
    fn config_default_har_giltigt_vapid_subject() {
        assert_eq!(PushConfig::default().subject, "mailto:netfyr@localhost");
    }

    #[test]
    fn vapid_public_key_är_okomprimerad_p256() {
        let kp = jwt_simple::algorithms::ES256KeyPair::generate();
        let encoded = vapid_public_key_b64(&kp);
        let raw = b64url_decode(&encoded).unwrap();
        assert_eq!(raw.len(), 65);
        assert_eq!(raw[0], 0x04);
        p256::PublicKey::from_sec1_bytes(&raw).unwrap();
    }

    #[test]
    fn endpoint_origin_är_kanonisk_och_utan_capability() {
        let url = parse_push_endpoint("https://fcm.googleapis.com/fcm/send/hemlig?x=1").unwrap();
        assert_eq!(push_endpoint_origin(&url), "https://fcm.googleapis.com");
        assert!(parse_push_endpoint("https://").is_err());
        assert!(parse_push_endpoint("https://user:pass@example.com/x").is_err());
        assert!(parse_push_endpoint("https://example.com:8443/x").is_err());
    }

    #[test]
    fn partiella_pushresultat_dupliceras_inte_vid_retry() {
        assert_eq!(classify_delivery(2, 0, 0, 0), DeliveryClass::Delivered);
        // 404/410 prunas och de mottagare som finns kvar ska inte få dublett.
        assert_eq!(classify_delivery(1, 0, 0, 1), DeliveryClass::Delivered);
        // Blandad framgång + transient fel är terminalt synligt men får inte retryas.
        assert_eq!(
            classify_delivery(1, 1, 0, 0),
            DeliveryClass::TerminalPartial
        );
        // När ingen fått meddelandet är retry säker.
        assert_eq!(classify_delivery(0, 1, 0, 0), DeliveryClass::Retryable);
    }

    #[test]
    fn ssrf_skydd_tillåter_bara_publika_adresser() {
        for private in [
            "0.0.0.0",
            "127.0.0.1",
            "192.168.1.10",
            "169.254.1.2",
            "192.0.0.9",
            "198.18.0.1",
            "::1",
            "fd00::1",
            "64:ff9b::1",
            "64:ff9b:1::1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_ip(private.parse().unwrap()), "{private}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "netfyr-push-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn samtidiga_ensure_keypair_returnerar_samma_persistenta_nyckel() {
        let dir = test_dir("vapid-single-flight");
        let secrets = Secrets::open(&dir).unwrap();
        let mut workers = Vec::new();
        for _ in 0..12 {
            let secrets = secrets.clone();
            workers.push(std::thread::spawn(move || {
                ensure_keypair(&secrets).unwrap();
                (
                    secrets.get("vapid").unwrap(),
                    public_key_b64(&secrets).unwrap(),
                )
            }));
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(results.windows(2).all(|pair| pair[0] == pair[1]));

        let reopened = Secrets::open(&dir).unwrap();
        assert_eq!(reopened.get("vapid").unwrap(), results[0].0);
        assert_eq!(public_key_b64(&reopened).unwrap(), results[0].1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn utgångna_prenumerationer_prunas_utan_att_skriva_over_andra() {
        let dir = test_dir("prune");
        let db_path = dir.join("netfyr.db");
        let db = Db::open(&db_path).unwrap();
        let expired = "https://push.example/expired".to_string();
        let retained = "https://push.example/retained".to_string();
        let cfg = PushConfig {
            subscriptions: vec![
                PushSubscription {
                    endpoint: expired.clone(),
                    p256dh: "key-a".into(),
                    auth: "auth-a".into(),
                    label: String::new(),
                    created_at: String::new(),
                    owner: "alice".into(),
                },
                PushSubscription {
                    endpoint: retained.clone(),
                    p256dh: "key-b".into(),
                    auth: "auth-b".into(),
                    label: String::new(),
                    created_at: String::new(),
                    owner: "bob".into(),
                },
            ],
            ..PushConfig::default()
        };
        let raw = serde_json::to_string(&cfg).unwrap();
        db.call(move |conn| repo::set_setting(conn, "channel.push.config", &raw))
            .await
            .unwrap();

        assert_eq!(
            prune_expired(&db, std::slice::from_ref(&expired))
                .await
                .unwrap(),
            1
        );
        let raw: String = db
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT value FROM settings WHERE key = 'channel.push.config'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        let after: PushConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(after.subscriptions.len(), 1);
        assert_eq!(after.subscriptions[0].endpoint, retained);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
