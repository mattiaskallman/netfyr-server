// =====================================================================
// i18n.rs
// Motorns språk — svenska eller engelska.
//
// Gränssnittet (web/) har sitt eget, personliga språkval per
// webbläsare. Det här modulen gäller det DELADE: API-fel, händelse-
// loggen, leveransfel, auditdetaljer och SMS-standardtexter. De
// skrivs en gång och läses av alla, därför är valet globalt och
// lagras som nyckeln "lang" i settings ("sv" eller "en", saknas =
// svenska — befintliga installationer beter sig som förut).
//
// Användning:
//   let lang = i18n::load(conn);            // inne i en db.call
//   let lang = i18n::load_db(&state.db).await;  // i en handler
//   return Err(ApiError::bad_request(i18n::name_missing(lang)));
//
// En sträng per funktion, aldrig strängjämförelser mellan språken —
// logik som skiljer på fall använder typer (se LoginReject i
// api/auth.rs), inte meddelandetext.
// =====================================================================

use rusqlite::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Sv,
    En,
}

impl Lang {
    /// Inställningsvärdet: bara "en" ger engelska, allt annat är
    /// svenska. Snävt håller konfigurationsytan liten — nya språk är
    /// ett medvetet tillägg här, inte något som råkar skapas via API:t.
    pub fn from_setting(v: &str) -> Lang {
        if v.trim().eq_ignore_ascii_case("en") {
            Lang::En
        } else {
            Lang::Sv
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Sv => "sv",
            Lang::En => "en",
        }
    }

    pub fn valid(v: &str) -> bool {
        matches!(v.trim().to_ascii_lowercase().as_str(), "sv" | "en")
    }
}

/// Läser aktivt motorspråk ur settings. Saknas nyckeln (eller läsningen
/// faller) är svenska standard — samma beteende som före i18n.
pub fn load(conn: &Connection) -> Lang {
    conn.query_row("SELECT value FROM settings WHERE key = 'lang'", [], |r| {
        r.get::<_, String>(0)
    })
    .map(|v| Lang::from_setting(&v))
    .unwrap_or(Lang::Sv)
}

/// Async-varianten för handlers och middleware som inte redan sitter
/// i en db.call. Fel här får aldrig fälla requesten — svenska är
/// fallback.
pub async fn load_db(db: &crate::db::Db) -> Lang {
    db.call(|conn| Ok(load(conn))).await.unwrap_or(Lang::Sv)
}

/// En sträng per språkvariant. Läser du den här makron fel: den tar
/// funktionsnamn, svensk text, engelsk text — i den ordningen.
macro_rules! t {
    ($(#[$m:meta])* $name:ident, $sv:expr, $en:expr) => {
        $(#[$m])*
        pub fn $name(l: Lang) -> &'static str {
            match l {
                Lang::Sv => $sv,
                Lang::En => $en,
            }
        }
    };
}

// ---- Generellt ---------------------------------------------------------

t!(internal_error, "internt fel", "internal error");
t!(not_logged_in, "inte inloggad", "not signed in");
t!(requires_admin, "kräver administratörsbehörighet", "administrator privileges required");

// ---- Inloggning och konto (api/auth.rs, auth.rs) ------------------------

t!(login_fail, "fel användarnamn eller lösenord", "wrong username or password");
t!(login_locked, "för många försök — kontot är låst en kvart", "too many attempts — the account is locked for fifteen minutes");
t!(account_disabled, "kontot är avstängt", "the account is disabled");
t!(username_password_required, "användarnamn och lösenord behövs", "username and password are required");
t!(current_password_wrong, "det nuvarande lösenordet stämmer inte", "the current password is incorrect");
t!(password_too_long, "lösenordet är för långt", "the password is too long");

pub fn password_min_len(l: Lang, n: usize) -> String {
    match l {
        Lang::Sv => format!("lösenordet måste vara minst {n} tecken"),
        Lang::En => format!("the password must be at least {n} characters"),
    }
}

// ---- Användare (api/users.rs) ------------------------------------------

t!(username_chars, "användarnamnet får innehålla a-z, 0-9, punkt, understreck och bindestreck",
   "the username may contain a-z, 0-9, period, underscore and hyphen");
t!(role_invalid, "rollen måste vara admin eller user", "the role must be admin or user");
t!(username_taken, "användarnamnet är upptaget", "the username is taken");
t!(self_change_forbidden, "du kan inte ändra roll eller stänga av ditt eget konto",
   "you cannot change the role or disable your own account");
t!(user_not_found, "användaren finns inte", "the user does not exist");
t!(last_admin_change, "den sista administratören kan inte tas bort eller degraderas",
   "the last administrator cannot be removed or demoted");
t!(last_admin_delete, "den sista administratören kan inte tas bort",
   "the last administrator cannot be removed");
t!(self_delete_forbidden, "du kan inte ta bort ditt eget konto",
   "you cannot delete your own account");
// Auditdetaljer vid kontobyte.
t!(audit_disabled, "avstängt", "disabled");
t!(audit_enabled, "aktiverat", "enabled");
t!(audit_password_reset, "lösenord återställt", "password reset");
pub fn audit_role_change(l: Lang, role: &str) -> String {
    match l {
        Lang::Sv => format!("roll \u{2192} {role}"),
        Lang::En => format!("role \u{2192} {role}"),
    }
}

// ---- Enheter och grupper (api/hosts.rs, api/groups.rs, engine/repo.rs) --

t!(name_missing, "namn saknas", "name is missing");
t!(address_missing, "adress saknas", "address is missing");
t!(address_exists, "adressen finns redan", "the address already exists");
t!(host_not_found, "enheten finns inte", "the device does not exist");
t!(host_no_status, "enheten finns inte, eller har ingen status än",
   "the device does not exist, or has no status yet");
t!(group_exists, "gruppen finns redan", "the group already exists");
t!(name_taken, "namnet är upptaget", "the name is taken");
t!(group_not_found, "gruppen finns inte", "the group does not exist");
t!(dep_self, "en enhet kan inte bero på sig själv", "a device cannot depend on itself");
t!(dep_cycle, "beroendet skapar en cykel", "the dependency creates a cycle");
t!(dep_too_deep, "beroendekedjan är för djup, misstänkt cykel",
   "the dependency chain is too deep, suspected cycle");

// ---- Inställningar och hemligheter (api/settings.rs, api/secrets.rs) ----

pub fn unknown_setting(l: Lang, key: &str) -> String {
    match l {
        Lang::Sv => format!("okänd inställning: {key}"),
        Lang::En => format!("unknown setting: {key}"),
    }
}
t!(config_not_object, "konfigurationen måste vara ett objekt", "the configuration must be an object");
t!(lang_invalid, "ogiltigt språk — tillåtna värden är sv och en",
   "invalid language — allowed values are sv and en");
t!(secret_empty, "tomt värde — använd DELETE för att ta bort",
   "empty value — use DELETE to remove");

// ---- Import/export och statistik (api/transfer.rs, api/stats.rs) --------

t!(not_netfyr_file, "det här är ingen NetFyr-fil", "this is not a NetFyr file");
t!(too_many_hosts, "för många enheter i filen", "too many devices in the file");
pub fn imported_detail(l: Lang, imported: usize, skipped: usize) -> String {
    match l {
        Lang::Sv => format!("{imported} importerade, {skipped} hoppades över"),
        Lang::En => format!("{imported} imported, {skipped} skipped"),
    }
}
pub fn unknown_window(l: Lang, window: &str) -> String {
    match l {
        Lang::Sv => format!("okänt fönster: {window}"),
        Lang::En => format!("unknown window: {window}"),
    }
}
pub fn cleared_detail(l: Lang, removed: usize) -> String {
    match l {
        Lang::Sv => format!("{removed} mätningar raderade"),
        Lang::En => format!("{removed} measurements deleted"),
    }
}

// ---- Underhåll (api/maintenance.rs) -------------------------------------

t!(end_before_start, "sluttiden måste ligga efter starttiden",
   "the end time must be after the start time");
t!(invalid_time, "ogiltig tid eller längd", "invalid time or duration");
t!(kind_invalid, "kind måste vara once eller daily", "kind must be once or daily");
t!(target_invalid, "ogiltigt mål", "invalid target");
t!(window_not_found, "fönstret hittades inte", "the window was not found");

// ---- Kanaler (api/channels.rs, channels/mod.rs, smtp, mqtt) -------------

t!(unknown_channel, "okänd kanal", "unknown channel");
pub fn unknown_channel_named(l: Lang, name: &str) -> String {
    match l {
        Lang::Sv => format!("okänd kanal: {name}"),
        Lang::En => format!("unknown channel: {name}"),
    }
}
t!(radio_not_in_server, "radiokanalen finns inte i serverutgåvan än",
   "the radio channel is not in the server edition yet");
t!(test_device, "Test från NetFyr Server", "Test from NetFyr Server");
t!(test_message, "Testlarm — kanalen når sin mottagare.",
   "Test alarm — the channel reaches its recipient.");
t!(invalid_alarm, "ogiltigt larm", "invalid alarm");
t!(push_no_subscriptions, "push: inga prenumerationer registrerade",
   "push: no subscriptions registered");
t!(push_bad_config, "push: ogiltig kanalconfig", "push: invalid channel configuration");
t!(push_vapid_missing, "push: VAPID-nyckel saknas — aktivera push först",
   "push: VAPID key missing — enable push first");
t!(push_all_failed, "push: alla sändningar misslyckades", "push: all deliveries failed");
t!(push_config_corrupt, "push: lagrad kanalconfig är korrupt",
   "push: stored channel configuration is corrupt");
t!(push_not_enabled, "push: kanalen är inte aktiverad — be en administratör slå på den först",
   "push: the channel is not enabled — ask an administrator to enable it first");
t!(push_bad_endpoint, "push: endpoint måste vara en giltig publik https-adress på port 443 (max 2048 tecken)",
   "push: endpoint must be a valid public https address on port 443 (max 2048 chars)");
t!(push_bad_p256dh, "push: ogiltig p256dh-nyckel", "push: invalid p256dh key");
t!(push_bad_auth, "push: ogiltig auth-hemlighet", "push: invalid auth secret");
t!(push_bad_label, "push: ogiltig enhetsetikett", "push: invalid device label");
t!(push_too_many, "push: maximalt 8 prenumerationer per användare och 64 totalt är tillåtna",
   "push: at most 8 subscriptions per user and 64 total are allowed");
t!(push_owned_by_other, "push: prenumerationen tillhör en annan användare",
   "push: the subscription belongs to another user");
pub fn push_subscription_expired(l: Lang, status: u16) -> String {
    match l {
        Lang::Sv => format!("push: prenumerationen har upphört ({status}) — ta bort enheten i inställningarna"),
        Lang::En => format!("push: subscription has expired ({status}) — remove the device in settings"),
    }
}
t!(webhook_no_url, "ingen webhook-adress angiven", "no webhook URL given");
t!(webhook_unreachable, "webhook-anropet gick inte fram", "the webhook call did not get through");
t!(smtp_invalid_config, "ogiltig SMTP-konfiguration", "invalid SMTP configuration");
t!(smtp_no_host, "ingen SMTP-server angiven", "no SMTP server given");
t!(smtp_build_message, "kunde inte bygga meddelandet", "could not build the message");
t!(smtp_send_failed, "SMTP-fel", "SMTP error");
pub fn smtp_invalid_from(l: Lang, e: &str) -> String {
    match l {
        Lang::Sv => format!("ogiltig avsändaradress: {e}"),
        Lang::En => format!("invalid sender address: {e}"),
    }
}
pub fn smtp_invalid_to(l: Lang, e: &str) -> String {
    match l {
        Lang::Sv => format!("ogiltig mottagaradress: {e}"),
        Lang::En => format!("invalid recipient address: {e}"),
    }
}
/// Själva larmmejlet. Etiketterna följer motorspråket; status värdet
/// ("down"/"up") är rådata och lämnas orört.
pub fn smtp_body(l: Lang, message: &str, device: &str, address: &str, status: &str, time: &str) -> String {
    match l {
        Lang::Sv => format!("{message}\n\nEnhet: {device}\nAdress: {address}\nStatus: {status}\nTid: {time}"),
        Lang::En => format!("{message}\n\nDevice: {device}\nAddress: {address}\nStatus: {status}\nTime: {time}"),
    }
}
t!(mqtt_invalid_config, "ogiltig MQTT-konfiguration", "invalid MQTT configuration");
t!(mqtt_no_host, "ingen MQTT-server angiven", "no MQTT server given");
t!(mqtt_no_topic, "inget MQTT-ämne angivet", "no MQTT topic given");
pub fn mqtt_error(l: Lang, e: &str) -> String {
    match l {
        Lang::Sv => format!("MQTT-fel: {e}"),
        Lang::En => format!("MQTT error: {e}"),
    }
}

// ---- SMS-gateway (channels/sms.rs) ---------------------------------------
// Dessa texter hamnar i leveransens lastError och i kanalkortets
// statusfält — operatören ser dem i Terminal-vyn och på Larm-fliken.

t!(sms_no_host, "ingen adress till SMS-gateway angiven", "no SMS gateway address given");
t!(sms_no_username, "inget användarnamn till SMS-gateway angivet", "no SMS gateway username given");
t!(sms_http_client, "kunde inte skapa HTTP-klient", "could not create the HTTP client");
t!(sms_unknown_error, "okänt fel", "unknown error");
t!(sms_gateway_rejected, "gateway avvisade anropet", "the gateway rejected the call");
t!(sms_gateway_unreachable, "nådde inte SMS-gateway", "could not reach the SMS gateway");
t!(sms_read_response, "kunde inte läsa svar", "could not read the response");
t!(sms_no_token, "gateway lämnade ingen token", "the gateway returned no token");
t!(sms_no_modem, "gateway rapporterade inget modem", "the gateway reported no modem");
t!(sms_no_modem_id, "kunde inte läsa modem-ID", "could not read the modem ID");
t!(sms_invalid_config, "ogiltig SMS-konfiguration", "invalid SMS configuration");
t!(sms_no_recipients, "inga SMS-mottagare angivna", "no SMS recipients given");
t!(sms_no_sim, "Inget SIM-kort i gatewayen", "No SIM card in the gateway");

pub fn sms_invalid_response(l: Lang, e: &str) -> String {
    match l {
        Lang::Sv => format!("ogiltigt svar från gateway: {e}"),
        Lang::En => format!("invalid response from gateway: {e}"),
    }
}
pub fn sms_login_paused(l: Lang, secs_left: u64) -> String {
    match l {
        Lang::Sv => format!("inloggning mot SMS-gateway pausad efter upprepade fel ({secs_left} s kvar)"),
        Lang::En => format!("login to the SMS gateway paused after repeated failures ({secs_left} s left)"),
    }
}
pub fn sms_acl_hint(l: Lang, msg: &str) -> String {
    match l {
        Lang::Sv => format!(
            "{msg}. Användaren saknar behörighet — kontrollera ACL under \
             System → Users i gatewayens webbgränssnitt"
        ),
        Lang::En => format!(
            "{msg}. The user lacks permission — check the ACL under \
             System → Users in the gateway web interface"
        ),
    }
}
pub fn sms_modem_not_registered(l: Lang, state: Option<&str>) -> String {
    match (l, state) {
        (Lang::Sv, Some(st)) => format!("Modemet är inte registrerat i nätet ({st})"),
        (Lang::Sv, None) => "Modemet är inte registrerat i nätet".to_string(),
        (Lang::En, Some(st)) => format!("The modem is not registered on the network ({st})"),
        (Lang::En, None) => "The modem is not registered on the network".to_string(),
    }
}

/// Standardmallar när användaren lämnat fältet tomt. Tokennamnen
/// ({enhet} m.fl.) är mallens språk och översätts aldrig.
pub fn sms_default_message(l: Lang) -> String {
    match l {
        Lang::Sv => "NetFyr: {enhet} ({adress}) svarar inte".to_string(),
        Lang::En => "NetFyr: {enhet} ({adress}) is not responding".to_string(),
    }
}
pub fn sms_default_recovery(l: Lang) -> String {
    match l {
        Lang::Sv => "NetFyr: {enhet} ({adress}) \u{e5}ter i drift".to_string(),
        Lang::En => "NetFyr: {enhet} ({adress}) is back in service".to_string(),
    }
}
/// Standardmall för latenslarm (etapp 8). Egen mall: "svarar inte"
/// hade varit en lögn om en enhet som svarar — bara långsamt.
pub fn sms_default_slow(l: Lang) -> String {
    match l {
        Lang::Sv => "NetFyr: {enhet} ({adress}) svarar l\u{e5}ngsamt".to_string(),
        Lang::En => "NetFyr: {enhet} ({adress}) is responding slowly".to_string(),
    }
}

// ---- Händelseloggen (engine/monitor.rs, queue.rs, sms_engine.rs) --------
// Skrivs en gång och läggs i events-tabellen — språket är det som
// gällde när händelsen inträffade, gamla rader översätts inte i
// efterhand.

pub fn ev_alarm_confirmed(l: Lang, down: bool, name: &str, addr: &str) -> String {
    match (l, down) {
        (Lang::Sv, true) => format!("NER bekräftad: {name} ({addr})"),
        (Lang::Sv, false) => format!("UPP bekräftad: {name} ({addr})"),
        (Lang::En, true) => format!("DOWN confirmed: {name} ({addr})"),
        (Lang::En, false) => format!("UP confirmed: {name} ({addr})"),
    }
}
pub fn ev_delivered(l: Lang, channel: &str, device: &str) -> String {
    match l {
        Lang::Sv => format!("levererat \u{2192} {channel} ({device})"),
        Lang::En => format!("delivered \u{2192} {channel} ({device})"),
    }
}
pub fn ev_gave_up(l: Lang, channel: &str, device: &str, attempts: u32, msg: &str) -> String {
    match l {
        Lang::Sv => format!("gav upp \u{2192} {channel} ({device}) efter {attempts} f\u{f6}rs\u{f6}k: {msg}"),
        Lang::En => format!("gave up \u{2192} {channel} ({device}) after {attempts} attempts: {msg}"),
    }
}
pub fn ev_escalation_queued(l: Lang, device: &str, id: &str, total: usize) -> String {
    match l {
        Lang::Sv => format!("larm k\u{f6}at \u{2192} sms: {device} (down) [{id}] 1/{total}"),
        Lang::En => format!("alarm queued \u{2192} sms: {device} (down) [{id}] 1/{total}"),
    }
}
pub fn ev_ack_window_expired(l: Lang, device: &str, id: &str) -> String {
    match l {
        Lang::Sv => format!("sms: kvittensf\u{f6}nstret gick ut f\u{f6}r {device} [{id}]"),
        Lang::En => format!("sms: acknowledgement window expired for {device} [{id}]"),
    }
}
pub fn ev_no_ack(l: Lang, device: &str, id: &str) -> String {
    match l {
        Lang::Sv => format!("sms: ingen kvittens f\u{f6}r {device} [{id}]"),
        Lang::En => format!("sms: no acknowledgement for {device} [{id}]"),
    }
}
pub fn ev_escalating(l: Lang, device: &str, id: &str, n: usize, total: usize) -> String {
    match l {
        Lang::Sv => format!("sms eskalerar: {device} [{id}] {n}/{total}"),
        Lang::En => format!("sms escalating: {device} [{id}] {n}/{total}"),
    }
}
pub fn ev_acked(l: Lang, id: &str, sender: &str) -> String {
    match l {
        Lang::Sv => format!("sms kvitterad: [{id}] av {sender}"),
        Lang::En => format!("sms acknowledged: [{id}] by {sender}"),
    }
}
/// Latenslarm i händelseloggen (etapp 8). down-fältet i ev_alarm_confirmed
/// räcker inte — slow är en varning, inte en statusövergång.
pub fn ev_slow(l: Lang, alarm: bool, name: &str, addr: &str, latency_ms: Option<u32>) -> String {
    let ms = latency_ms
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
    match (l, alarm) {
        (Lang::Sv, true) => format!("LÅNGSAM bekräftad: {name} ({addr}) — {ms} ms"),
        (Lang::Sv, false) => format!("normal svarstid igen: {name} ({addr})"),
        (Lang::En, true) => format!("SLOW confirmed: {name} ({addr}) — {ms} ms"),
        (Lang::En, false) => format!("back to normal latency: {name} ({addr})"),
    }
}
// ---- Validering av probetyper (api/hosts.rs, etapp 8) ------------------
pub fn probe_invalid(l: Lang, v: &str) -> String {
    match l {
        Lang::Sv => format!("ogiltig mättyp \"{v}\" — tillåtna: icmp, tcp, http"),
        Lang::En => format!("invalid probe type \"{v}\" — allowed: icmp, tcp, http"),
    }
}
pub fn probe_port_required(l: Lang) -> String {
    match l {
        Lang::Sv => "port krävs för mättyperna tcp och http (1-65535)".to_string(),
        Lang::En => "a port is required for probe types tcp and http (1-65535)".to_string(),
    }
}
pub fn probe_port_icmp(l: Lang) -> String {
    match l {
        Lang::Sv => "icmp har ingen port — lämna portfältet tomt".to_string(),
        Lang::En => "icmp has no port — leave the port field empty".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_parsing() {
        assert_eq!(Lang::from_setting("en"), Lang::En);
        assert_eq!(Lang::from_setting("EN"), Lang::En);
        assert_eq!(Lang::from_setting("sv"), Lang::Sv);
        assert_eq!(Lang::from_setting("norska"), Lang::Sv);
        assert!(Lang::valid("sv"));
        assert!(Lang::valid("en"));
        assert!(!Lang::valid("de"));
    }

    #[test]
    fn strings_both_languages() {
        assert_eq!(login_fail(Lang::Sv), "fel användarnamn eller lösenord");
        assert_eq!(login_fail(Lang::En), "wrong username or password");
        assert!(ev_alarm_confirmed(Lang::Sv, true, "Router", "192.168.1.1")
            .starts_with("NER bekräftad"));
        assert!(ev_alarm_confirmed(Lang::En, true, "Router", "192.168.1.1")
            .starts_with("DOWN confirmed"));
        assert!(sms_default_recovery(Lang::Sv).contains("åter i drift"));
        assert!(sms_default_recovery(Lang::En).contains("back in service"));
    }
}
