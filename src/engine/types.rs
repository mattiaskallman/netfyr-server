// =====================================================================
// engine/types.rs
// Domäntyper för övervakningsmotorn.
//
// Portade från desktopvariantens TypeScript. Namnen är avsiktligt
// desamma, så att de två implementationerna går att jämföra rad för rad
// när något beter sig olika.
// =====================================================================

use serde::{Deserialize, Serialize};

/// Utfallet av en enskild ping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Fail,
}

/// Bekräftad status från flap-grinden. Det enda som får styra larm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Unknown,
    Up,
    Down,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Unknown => "unknown",
            Status::Up => "up",
            Status::Down => "down",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "up" => Status::Up,
            "down" => Status::Down,
            _ => Status::Unknown,
        }
    }
}

/// Rått ping-utfall som det visas, före grinden.
///
/// Skilt från Status med flit: det här är vad som hände nyss, Status är
/// vad grinden bekräftat. Att blanda ihop dem var precis det fel som
/// gjorde att en enstaka missad ping färgade gränssnittet rött.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawStatus {
    Online,
    /// Svarar, men långsammare än tröskeln.
    Warning,
    Offline,
}

impl RawStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RawStatus::Online => "online",
            RawStatus::Warning => "warning",
            RawStatus::Offline => "offline",
        }
    }

    /// Okänt värde tolkas som Online — samma eftergift som vid
    /// omstart, där enheten visas som uppe tills motorn hunnit mäta.
    pub fn parse(s: &str) -> Self {
        match s {
            "warning" => RawStatus::Warning,
            "offline" => RawStatus::Offline,
            _ => RawStatus::Online,
        }
    }
}

/// Varför larmen för en enhet är undertryckta.
///
/// Prioritet: snooze (manuell, omedelbar) > underhåll (planerat) >
/// beroende (rotorsak).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuppressionReason {
    Snooze,
    Maintenance,
    Dependency,
}

/// Vad en enhet ska visas som.
///
/// Härleds ur enabled, undertryckning, bekräftad status och rått utfall —
/// aldrig ur rådatat ensamt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayStatus {
    Online,
    Warning,
    /// Missad ping, men grinden har ännu inte bekräftat NER.
    Uncertain,
    /// Bekräftat NER, svar börjar komma in — larmet lever dock kvar.
    Recovering,
    Suppressed,
    Offline,
    /// Avstängd i konfigurationen.
    Paused,
}

/// Hur enheten mäts (etapp 8).
///
/// NULL i databasen tolkas som Icmp — desktopens beteende och default
/// för alla enheter som fanns före etapp 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeType {
    /// ICMP-ping — desktopparitet.
    Icmp,
    /// TCP-anslutning mot en port: "lyssnar tjänsten?"
    Tcp,
    /// HTTP GET mot /: "svarar tjänsten 2xx/3xx?"
    Http,
}

impl ProbeType {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeType::Icmp => "icmp",
            ProbeType::Tcp => "tcp",
            ProbeType::Http => "http",
        }
    }

    /// Okänt värde tolkas som Icmp — samma eftergift som NULL. En trasig
    /// sträng i databasen ska inte tysta övervakningen av enheten.
    pub fn parse(s: &str) -> Self {
        match s {
            "tcp" => ProbeType::Tcp,
            "http" => ProbeType::Http,
            _ => ProbeType::Icmp,
        }
    }

    /// Behöver proben en port?
    pub fn needs_port(self) -> bool {
        !matches!(self, ProbeType::Icmp)
    }
}

/// Enhet som motorn ser den.
///
/// Beroendet pekar på ADRESS, inte id. Desktopvarianten använder id
/// internt men adress vid export. Adressen är unik i databasen och
/// överlever både export, import och id-omnumrering, så den är den
/// stabilare nyckeln för en server som ska kunna flytta konfiguration
/// mellan installationer.
#[derive(Debug, Clone)]
pub struct Host {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub group_id: Option<i64>,
    /// Gruppens namn. Gränssnittet arbetar med namn, databasen med id.
    pub group: Option<String>,
    pub note: Option<String>,
    pub enabled: bool,
    /// Om just den här enhetens larm får skickas via SMS. Andra kanaler,
    /// händelseloggen och själva övervakningen påverkas inte.
    pub sms_enabled: bool,

    /// Bekräftad status från grinden.
    pub confirmed: Status,
    /// Senaste råa utfallet.
    pub raw: RawStatus,

    /// Manuell tystnad till och med denna tidpunkt (ms).
    pub snooze_until: Option<i64>,
    /// Larmar inte om denna adress också är bekräftat nere.
    pub depends_on_address: Option<String>,

    // Override, None = ärv globalt
    /// Egen svepfrekvens i sekunder.
    pub interval_sec: Option<u32>,
    pub packet_size: Option<u32>,
    pub slow_threshold_ms: Option<u32>,
    pub fail_period_sec: Option<u32>,
    pub success_period_sec: Option<u32>,

    /// Hur enheten mäts. NULL i databasen = Icmp.
    pub probe_type: ProbeType,
    /// Port för tcp/http. Betydelselös för icmp.
    pub probe_port: Option<u16>,
}
