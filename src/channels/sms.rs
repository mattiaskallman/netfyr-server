// =====================================================================
// channels/sms.rs
// SMS via Teltonika-gateway (RutOS Web API).
//
// Portad från desktopvariantens pro/sms/mod.rs, med reqwest bytt från
// blockerande till asynk. Logiken är oförändrad.
//
// Fyra fällor som kostade felsökning och därför är hårdkodade krav:
//
//  1. Post/Get-gränssnittet (/cgi-bin/sms_send) togs bort i RutOS 7.14.
//     Web API är enda vägen på modern firmware.
//  2. Sändningens body kräver ett "data"-omslag. Utan det svarar enheten
//     med kod 102 "No arguments provided for action".
//  3. Modem-ID är enhetsspecifikt — TRB160 rapporterar "3-1", inte
//     "1-1" som communityexemplen visar. Hämtas därför dynamiskt.
//  4. uhttpd återanvänder inte keep-alive-anslutningar väl.
//     pool_max_idle_per_host(0) krävs, annars kommer sporadiska
//     anslutningsfel på andra anropet i en följd.
//
// Sessionstoken lever 299 sekunder och cachas mellan larm.
//
// Eskalering och kvittering (sessioner, inkorgspollning) ligger i
// src/sms_engine.rs — kanalen exponerar här de primitiv den behöver:
// status, verify, inbox och remove_messages. Samma uppdelning som
// desktopens frontend/Rust-gräns, men med SQLite som sessionslager.
// =====================================================================

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::AlarmPayload;

/// Token lever 299 s enligt enheten. Marginalen gör att vi förnyar
/// innan den hinner gå ut mitt i en sändning.
const TOKEN_MARGIN: Duration = Duration::from_secs(30);

/// RutOS-felkod för nekad behörighet — både utgången token och ACL.
const ERR_UNAUTHORIZED: i64 = 120;

fn default_scheme() -> String {
    "https".to_string()
}

fn default_timeout() -> u64 {
    15_000
}

fn default_escalation_sec() -> u64 {
    300
}

fn default_ack_window_min() -> u64 {
    60
}

fn default_poll_sec() -> u64 {
    30
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SmsConfig {
    /// Värdnamn eller IP, får innehålla port.
    pub host: String,
    #[serde(default = "default_scheme")]
    pub scheme: String,
    pub username: String,
    /// Tomt = hämta automatiskt.
    #[serde(default)]
    pub modem_id: String,
    /// Gatewayen har normalt ett självsignerat certifikat.
    #[serde(default)]
    pub allow_self_signed: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub recipients: Vec<String>,
    // Standardmallarna sätts inte vid deserialisering — de beror på
    // motorspråket och väljs i send() när fältet är tomt. Se i18n.rs.
    #[serde(default)]
    pub message_template: String,
    #[serde(default)]
    pub recovery_template: String,
    // ---- Eskalering och kvittering (desktopparitet) -------------------
    #[serde(default)]
    pub escalation_enabled: bool,
    #[serde(default = "default_escalation_sec")]
    pub escalation_sec: u64,
    #[serde(default = "default_ack_window_min")]
    pub ack_window_min: u64,
    #[serde(default = "default_poll_sec")]
    pub poll_sec: u64,
    #[serde(default)]
    pub delete_after_ack: bool,
}

impl SmsConfig {
    fn base_url(&self) -> String {
        let scheme = if self.scheme == "http" { "http" } else { "https" };
        format!("{}://{}", scheme, self.host.trim())
    }

    /// Cachenyckel. Byter värd eller konto blir cachad token ogiltig
    /// direkt i stället för att ge ett förvirrande 120-fel.
    fn cache_key(&self) -> String {
        format!("{}|{}", self.base_url(), self.username.trim())
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.clamp(2_000, 120_000))
    }

    fn validate(&self, l: crate::i18n::Lang) -> Result<()> {
        if self.host.trim().is_empty() {
            bail!(crate::i18n::sms_no_host(l));
        }
        if self.username.trim().is_empty() {
            bail!(crate::i18n::sms_no_username(l));
        }
        Ok(())
    }
}

// ---- Delat tillstånd -------------------------------------------------

struct Cached {
    key: String,
    token: String,
    expires_at: Instant,
    modem_id: Option<String>,
}

#[derive(Default)]
struct SmsState {
    cached: Option<Cached>,
    login_failures: u32,
    next_login_at: Option<Instant>,
}

fn state() -> &'static Mutex<SmsState> {
    static SLOT: OnceLock<Mutex<SmsState>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(SmsState::default()))
}

fn lock_state() -> std::sync::MutexGuard<'static, SmsState> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// Backoff mellan misslyckade inloggningar: 5 s, 30 s, 2 min, 10 min.
///
/// Skyddar mot att en felaktig konfiguration hamrar mot enheten och
/// triggar dess egen inloggningsspärr — då hade NetFyr låst ut sig
/// själv från sin egen larmväg.
fn login_backoff(failures: u32) -> Duration {
    match failures {
        0 | 1 => Duration::from_secs(5),
        2 => Duration::from_secs(30),
        3 => Duration::from_secs(120),
        _ => Duration::from_secs(600),
    }
}

// ---- HTTP ------------------------------------------------------------

fn build_client(cfg: &SmsConfig, l: crate::i18n::Lang) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(cfg.timeout())
        // uhttpd stänger anslutningen direkt efter svar. Utan detta får
        // vi "connection closed before message completed" på nästa anrop.
        .pool_max_idle_per_host(0)
        .danger_accept_invalid_certs(cfg.allow_self_signed)
        .build()
        .context(crate::i18n::sms_http_client(l))
}

/// Tolka ett API-svar. Andra värdet säger om felet var behörighetsrelaterat.
fn parse_api(body: &str, l: crate::i18n::Lang) -> std::result::Result<Value, (String, bool)> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| (crate::i18n::sms_invalid_response(l, &e.to_string()), false))?;

    if v.get("success").and_then(Value::as_bool) == Some(true) {
        return Ok(v.get("data").cloned().unwrap_or(Value::Null));
    }

    let mut unauthorized = false;
    let mut text = String::new();

    if let Some(errors) = v.get("errors").and_then(Value::as_array) {
        for e in errors {
            let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
            if code == ERR_UNAUTHORIZED {
                unauthorized = true;
            }
            let msg = e.get("error").and_then(Value::as_str).unwrap_or(crate::i18n::sms_unknown_error(l));
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push_str(&format!("{msg} (kod {code})"));
        }
    }

    if text.is_empty() {
        text = crate::i18n::sms_gateway_rejected(l).to_string();
    }
    Err((text, unauthorized))
}

async fn do_login(client: &reqwest::Client, cfg: &SmsConfig, password: &str, l: crate::i18n::Lang) -> Result<String> {
    // Backoff-kontrollen håller inte låset över nätverksanropet.
    {
        let st = lock_state();
        if let Some(at) = st.next_login_at {
            if Instant::now() < at {
                let left = at.saturating_duration_since(Instant::now()).as_secs();
                bail!(crate::i18n::sms_login_paused(l, left));
            }
        }
    }

    let body = json!({ "username": cfg.username.trim(), "password": password }).to_string();

    let outcome = async {
        let text = client
            .post(format!("{}/api/login", cfg.base_url()))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context(crate::i18n::sms_gateway_unreachable(l))?
            .text()
            .await
            .context(crate::i18n::sms_read_response(l))?;
        parse_api(&text, l).map_err(|(msg, _)| anyhow::anyhow!(msg))
    }
    .await;

    let data = match outcome {
        Ok(d) => d,
        Err(e) => {
            let mut st = lock_state();
            st.login_failures = st.login_failures.saturating_add(1);
            st.next_login_at = Some(Instant::now() + login_backoff(st.login_failures));
            return Err(e);
        }
    };

    let token = data
        .get("token")
        .and_then(Value::as_str)
        .context(crate::i18n::sms_no_token(l))?
        .to_string();

    let expires = data.get("expires").and_then(Value::as_u64).unwrap_or(299);
    let life = Duration::from_secs(expires.max(30));
    let expires_at = Instant::now() + life.saturating_sub(TOKEN_MARGIN).max(Duration::from_secs(5));

    let mut st = lock_state();
    st.login_failures = 0;
    st.next_login_at = None;
    st.cached = Some(Cached {
        key: cfg.cache_key(),
        token: token.clone(),
        expires_at,
        modem_id: None,
    });

    Ok(token)
}

async fn ensure_token(
    client: &reqwest::Client,
    cfg: &SmsConfig,
    password: &str,
    force: bool,
    l: crate::i18n::Lang,
) -> Result<String> {
    if !force {
        let st = lock_state();
        if let Some(c) = st.cached.as_ref() {
            if c.key == cfg.cache_key() && Instant::now() < c.expires_at {
                return Ok(c.token.clone());
            }
        }
    }
    do_login(client, cfg, password, l).await
}

/// Anropa API:t med automatisk omlogin vid behörighetsfel.
///
/// En omgång räcker: lyckas det inte efter en färsk token är det inte
/// tokenlivslängden som är problemet utan ACL:en på användaren.
async fn api_call(
    client: &reqwest::Client,
    cfg: &SmsConfig,
    password: &str,
    method: &str,
    path: &str,
    body: Option<String>,
    l: crate::i18n::Lang,
) -> Result<Value> {
    let mut forced = false;

    loop {
        let token = ensure_token(client, cfg, password, forced, l).await?;
        let url = format!("{}{}", cfg.base_url(), path);

        let mut req = if method == "POST" {
            client.post(&url).header("Content-Type", "application/json")
        } else {
            client.get(&url)
        }
        .header("Authorization", format!("Bearer {token}"));

        if let Some(ref b) = body {
            req = req.body(b.clone());
        }

        let response = req.send().await.context(crate::i18n::sms_gateway_unreachable(l))?;
        let http_unauthorized = matches!(response.status().as_u16(), 401 | 403);
        let text = response.text().await.context(crate::i18n::sms_read_response(l))?;

        match parse_api(&text, l) {
            Ok(data) => return Ok(data),
            Err((msg, unauthorized)) => {
                if (unauthorized || http_unauthorized) && !forced {
                    lock_state().cached = None;
                    forced = true;
                    continue;
                }
                if unauthorized || http_unauthorized {
                    bail!(crate::i18n::sms_acl_hint(l, &msg));
                }
                bail!(msg);
            }
        }
    }
}

fn text_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty() && s != "N/A")
}

fn num_field(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

/// Modem-ID från konfigurationen, annars automatiskt och cachat.
///
/// Hårdkodning är inte ett alternativ: TRB160 svarar "3-1" medan de
/// exempel som cirkulerar använder "1-1".
async fn resolve_modem_id(
    client: &reqwest::Client,
    cfg: &SmsConfig,
    password: &str,
    l: crate::i18n::Lang,
) -> Result<String> {
    let manual = cfg.modem_id.trim();
    if !manual.is_empty() {
        return Ok(manual.to_string());
    }

    {
        let st = lock_state();
        if let Some(c) = st.cached.as_ref() {
            if c.key == cfg.cache_key() {
                if let Some(id) = c.modem_id.as_ref() {
                    return Ok(id.clone());
                }
            }
        }
    }

    let data = api_call(client, cfg, password, "GET", "/api/modems/status", None, l).await?;
    let list = data.as_array().context(crate::i18n::sms_no_modem(l))?;
    let modem = list
        .iter()
        .find(|m| m.get("primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| list.first())
        .context(crate::i18n::sms_no_modem(l))?;

    let id = text_field(modem, "id").context(crate::i18n::sms_no_modem_id(l))?;

    {
        let mut st = lock_state();
        let key = cfg.cache_key();
        if let Some(c) = st.cached.as_mut() {
            if c.key == key {
                c.modem_id = Some(id.clone());
            }
        }
    }

    Ok(id)
}

// ---- Sändning --------------------------------------------------------

pub fn render(template: &str, a: &AlarmPayload) -> String {
    template
        .replace("{enhet}", &a.device)
        .replace("{adress}", &a.address)
        .replace("{status}", &a.status)
        .replace("{tid}", &a.time)
        .replace("{id}", &a.sms_session_id)
}

pub async fn send(payload: &str, config: &str, password: &str, l: crate::i18n::Lang) -> Result<()> {
    let cfg: SmsConfig = serde_json::from_str(config).context(crate::i18n::sms_invalid_config(l))?;
    cfg.validate(l)?;

    let a: AlarmPayload = serde_json::from_str(payload).context(crate::i18n::invalid_alarm(l))?;

    // Eskaleringen köar varje steg med sin egen mottagarlista. En tom
    // överskrivning betyder att kanalens globala lista gäller.
    let source: &[String] = if a.sms_recipients.is_empty() {
        &cfg.recipients
    } else {
        &a.sms_recipients
    };
    let recipients: Vec<String> = source
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if recipients.is_empty() {
        bail!(crate::i18n::sms_no_recipients(l));
    }

    let template = if a.status == "up" || a.status == "slow_ok" {
        &cfg.recovery_template
    } else {
        &cfg.message_template
    };
    // En tom mall (sparad som "" från gränssnittet, eller saknad i en
    // äldre konfiguration) betyder standardmallen — på motorspråket.
    // Latenslarmet har egen standardmall: "svarar inte" vore en lögn
    // om en enhet som svarar, bara långsamt.
    let template = if template.trim().is_empty() {
        match a.status.as_str() {
            "up" | "slow_ok" => crate::i18n::sms_default_recovery(l),
            "slow" => crate::i18n::sms_default_slow(l),
            _ => crate::i18n::sms_default_message(l),
        }
    } else {
        template.clone()
    };
    let message = render(&template, &a);

    let client = build_client(&cfg, l)?;
    let modem = resolve_modem_id(&client, &cfg, password, l).await?;

    let mut failures: Vec<String> = Vec::new();
    for number in &recipients {
        // "data"-omslaget är obligatoriskt. Utan det svarar enheten med
        // kod 102 "No arguments provided for action".
        let body = json!({
            "data": { "number": number, "message": message, "modem": modem }
        })
        .to_string();

        if let Err(e) = api_call(
            &client,
            &cfg,
            password,
            "POST",
            "/api/messages/actions/send",
            Some(body),
            l,
        )
        .await
        {
            failures.push(format!("{number}: {e}"));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        // Fel på någon mottagare fäller hela leveransen, vilket gör att
        // kön försöker igen mot samtliga. Medvetet val: ett dubbelt
        // larm-SMS är ett mindre problem än ett uteblivet.
        bail!("{}", failures.join(" | "))
    }
}

// ---- Konfigurationshjälpare (motorn och API:t) ------------------------

pub fn parse_config(config: &str) -> Result<SmsConfig> {
    serde_json::from_str(config).context(crate::i18n::sms_invalid_config(crate::i18n::Lang::Sv))
}

/// Som parse_config, men med feltext på motorns aktiva språk. Den
/// språklösa varianten finns kvar för kodvägar där en felaktig
/// konfiguration ändå bara ger ett tyst hopp-över.
pub fn parse_config_lang(config: &str, l: crate::i18n::Lang) -> Result<SmsConfig> {
    serde_json::from_str(config).context(crate::i18n::sms_invalid_config(l))
}

/// Motsvarigheten till desktopens trbConnectionReady: kanalen kan bara
/// användas när adress, konto, lösenord och minst en mottagare finns.
pub fn connection_ready(cfg: &SmsConfig, password: &str) -> bool {
    !cfg.host.trim().is_empty()
        && !cfg.username.trim().is_empty()
        && !password.trim().is_empty()
        && cfg.recipients.iter().any(|r| !r.trim().is_empty())
}

// ---- Modemstatus ------------------------------------------------------

/// Avläst status från gatewayen. Speglar /api/modems/status.
/// Enhetens svar varierar mellan firmwareversioner, så allt läses
/// defensivt ur JSON i stället för via en fast struct.
#[derive(serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SmsStatus {
    pub connected: bool,
    pub modem_id: Option<String>,
    pub modem_name: Option<String>,
    pub operator: Option<String>,
    pub operator_state: Option<String>,
    pub conn_type: Option<String>,
    pub network_type: Option<String>,
    pub signal_quality: Option<i64>,
    pub rssi: Option<i64>,
    pub sinr: Option<i64>,
    pub sim_state: Option<String>,
    pub sim_count: Option<i64>,
    pub temperature: Option<i64>,
    pub error: Option<String>,
}

pub fn error_status(error: &str) -> SmsStatus {
    SmsStatus {
        error: Some(error.to_string()),
        ..Default::default()
    }
}

fn pick_modem(data: &Value) -> Option<&Value> {
    let list = data.as_array()?;
    list.iter()
        .find(|m| m.get("primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| list.first())
}

async fn read_status(
    client: &reqwest::Client,
    cfg: &SmsConfig,
    password: &str,
    l: crate::i18n::Lang,
) -> Result<SmsStatus> {
    let data = api_call(client, cfg, password, "GET", "/api/modems/status", None, l).await?;
    let modem = pick_modem(&data).context(crate::i18n::sms_no_modem(l))?;

    let registered = text_field(modem, "operator_state")
        .map(|s| s.to_lowercase().contains("registered"))
        .unwrap_or(false);

    Ok(SmsStatus {
        connected: registered,
        modem_id: text_field(modem, "id"),
        modem_name: text_field(modem, "name"),
        operator: text_field(modem, "operator").or_else(|| text_field(modem, "provider")),
        operator_state: text_field(modem, "operator_state"),
        conn_type: text_field(modem, "conntype"),
        network_type: text_field(modem, "ntype"),
        signal_quality: num_field(modem, "signal_quality"),
        rssi: num_field(modem, "rssi"),
        sinr: num_field(modem, "sinr"),
        sim_state: text_field(modem, "simstate"),
        sim_count: num_field(modem, "sim_count"),
        temperature: num_field(modem, "temperature"),
        error: None,
    })
}

/// Läs modemstatus. Fel returneras som statusobjekt med error satt,
/// så gränssnittet alltid har något att rendera.
pub async fn status(config: &str, password: &str, l: crate::i18n::Lang) -> SmsStatus {
    let cfg = match parse_config_lang(config, l).and_then(|c| c.validate(l).map(|_| c)) {
        Ok(c) => c,
        Err(e) => return error_status(&e.to_string()),
    };
    let client = match build_client(&cfg, l) {
        Ok(c) => c,
        Err(e) => return error_status(&e.to_string()),
    };
    match read_status(&client, &cfg, password, l).await {
        Ok(s) => s,
        Err(e) => error_status(&e.to_string()),
    }
}

// ---- Inkorg ------------------------------------------------------------

/// Ett mottaget SMS. `id` är ett löpande index i inkorgen, INTE ett
/// stabilt ID — avduplicering får aldrig ske på det fältet ensamt.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsInboxItem {
    pub id: String,
    pub modem_id: String,
    pub sender: String,
    pub message: String,
    pub date: String,
    pub read: bool,
}

pub async fn inbox(config: &str, password: &str, l: crate::i18n::Lang) -> Result<Vec<SmsInboxItem>> {
    let cfg = parse_config_lang(config, l)?;
    cfg.validate(l)?;
    let client = build_client(&cfg, l)?;

    let data = api_call(&client, &cfg, password, "GET", "/api/messages/status", None, l).await?;
    let list = match data.as_array() {
        Some(l) => l,
        None => return Ok(Vec::new()),
    };

    Ok(list
        .iter()
        .map(|m| SmsInboxItem {
            id: text_field(m, "id").unwrap_or_default(),
            modem_id: text_field(m, "modem_id").unwrap_or_default(),
            sender: text_field(m, "sender").unwrap_or_default(),
            message: text_field(m, "message").unwrap_or_default(),
            date: text_field(m, "date").unwrap_or_default(),
            read: text_field(m, "status").as_deref() == Some("read"),
        })
        .collect())
}

/// Radera meddelanden ur inkorgen.
///
/// Fältnamnen skiljer sig från sändningen: här heter modemfältet
/// `modem_id`, medan actions/send använder `modem`. Samma enhet, samma
/// actions-gren, olika namn — verifierat mot RutOS 7.23.7.
pub async fn remove_messages(
    config: &str,
    password: &str,
    modem_id: &str,
    ids: Vec<String>,
    l: crate::i18n::Lang,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let cfg = parse_config_lang(config, l)?;
    cfg.validate(l)?;
    let client = build_client(&cfg, l)?;

    let modem = if modem_id.trim().is_empty() {
        resolve_modem_id(&client, &cfg, password, l).await?
    } else {
        modem_id.trim().to_string()
    };

    let body = json!({
        "data": { "modem_id": modem, "sms_id": ids }
    })
    .to_string();

    api_call(
        &client,
        &cfg,
        password,
        "POST",
        "/api/messages/actions/remove_messages",
        Some(body),
        l,
    )
    .await
    .map(|_| ())
}

// ---- Behörighetskontroll ------------------------------------------------

/// Resultat av behörighetskontrollen.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsVerify {
    /// Inloggningen lyckades.
    pub login_ok: bool,
    /// Användaren får läsa modemstatus.
    pub modems_ok: bool,
    /// Användaren får LÄSA meddelandelistan. RutOS skiljer på läs- och
    /// skrivrätt för /messages — läsrätten behövs för SMS-kvittering,
    /// inte för larmutskick.
    pub messages_read_ok: bool,
    /// Modemet är registrerat i nätet.
    pub registered: bool,
    pub modem_id: Option<String>,
    pub error: Option<String>,
}

/// Kontrollera hela kedjan innan kanalen tas i drift. Skiljer på fel
/// inloggning, otillräcklig ACL och avsaknad av nättäckning — utan detta
/// upptäcks ACL-problem först när ett skarpt larm ska ut.
///
/// Sändningsbehörigheten går INTE att kontrollera utan att faktiskt
/// skicka ett SMS, och en kontroll ska inte kosta pengar. Vi läser
/// därför meddelandelistan i stället och redovisar det som läsrätt.
pub async fn verify(config: &str, password: &str, l: crate::i18n::Lang) -> SmsVerify {
    let failed = |e: String| SmsVerify {
        login_ok: false,
        modems_ok: false,
        messages_read_ok: false,
        registered: false,
        modem_id: None,
        error: Some(e),
    };

    let cfg = match parse_config_lang(config, l).and_then(|c| c.validate(l).map(|_| c)) {
        Ok(c) => c,
        Err(e) => return failed(e.to_string()),
    };
    let client = match build_client(&cfg, l) {
        Ok(c) => c,
        Err(e) => return failed(e.to_string()),
    };

    // Tvinga fram en färsk inloggning så resultatet speglar de uppgifter
    // som just skrivits in.
    lock_state().cached = None;

    let mut out = SmsVerify {
        login_ok: false,
        modems_ok: false,
        messages_read_ok: false,
        registered: false,
        modem_id: None,
        error: None,
    };

    match ensure_token(&client, &cfg, password, true, l).await {
        Ok(_) => out.login_ok = true,
        Err(e) => {
            out.error = Some(e.to_string());
            return out;
        }
    }

    match read_status(&client, &cfg, password, l).await {
        Ok(s) => {
            out.modems_ok = true;
            out.registered = s.connected;
            out.modem_id = s.modem_id;
            if !s.connected {
                out.error = Some(crate::i18n::sms_modem_not_registered(l, s.operator_state.as_deref()));
            }
            if s.sim_state.as_deref() == Some("Absent") {
                out.error = Some(crate::i18n::sms_no_sim(l).to_string());
            }
        }
        Err(e) => {
            out.error = Some(e.to_string());
            return out;
        }
    }

    // Samma sökväg som pollningen använder. /api/messages ensamt ger
    // kod 120 även för en användare med full läsrätt — resursen finns
    // men är inte läsbar direkt, bara via underresursen.
    match api_call(&client, &cfg, password, "GET", "/api/messages/status", None, l).await {
        Ok(_) => out.messages_read_ok = true,
        Err(e) => {
            if out.error.is_none() {
                out.error = Some(e.to_string());
            }
        }
    }

    out
}
