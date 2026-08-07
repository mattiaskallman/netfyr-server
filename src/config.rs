// =====================================================================
// config.rs
// Konfiguration från TOML-fil.
//
// Sökvägen tas från argumentet --config, annars /etc/netfyr/config.toml.
// Saknas filen används inbyggda standardvärden — tjänsten ska gå att
// starta utan konfiguration under utveckling.
// =====================================================================

use anyhow::{Context, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/netfyr/config.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Adress att lyssna på.
    ///
    /// Standard är localhost: tjänsten ska inte exponeras på nätet förrän
    /// autentiseringen finns (etapp 5). Att öppna den innan dess vore att
    /// lägga ut en okontrollerad konsol.
    pub listen: SocketAddr,

    /// Sökväg till databasen.
    pub database: PathBuf,

    /// Katalog med frontend-filer. Saknas den serveras bara API:t.
    pub static_dir: Option<PathBuf>,

    /// Loggnivå: error, warn, info, debug, trace.
    pub log_level: String,

    /// Sätt Secure-flaggan på sessionskakan. Ska vara true när tjänsten
    /// nås över HTTPS — utan flaggan skickas kakan i klartext på nätet.
    /// Lämna false bara i ren utveckling över HTTP.
    pub secure_cookies: bool,

    /// Sessionens livslängd i timmar. Tolv timmar täcker ett nattpass
    /// utan att lämna en öppen session över veckoslutet.
    pub session_hours: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".parse().expect("giltig standardadress"),
            database: PathBuf::from("/var/lib/netfyr/netfyr.db"),
            static_dir: None,
            log_level: "info".to_string(),
            secure_cookies: false,
            session_hours: 12,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::warn!(
                "ingen konfigurationsfil på {}, använder standardvärden",
                path.display()
            );
            return Ok(Self::default());
        }

        let text = std::fs::read_to_string(path)
            .with_context(|| format!("kunde inte läsa {}", path.display()))?;

        toml::from_str(&text).with_context(|| format!("ogiltig konfiguration i {}", path.display()))
    }
}

/// Läs --config ur argumenten. Medvetet minimal argumenthantering —
/// en tjänst som startas av systemd behöver inte mer.
pub fn config_path_from_args() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" || arg == "-c" {
            if let Some(p) = args.next() {
                return PathBuf::from(p);
            }
        }
    }
    PathBuf::from(DEFAULT_CONFIG_PATH)
}

/// Underhållskommando från argumenten.
pub enum SecretAction {
    Set { name: String, value: String },
    Remove { name: String },
    List,
}

/// Läs --set-secret, --remove-secret eller --list-secrets.
///
/// Medvetet minimal tolkning. Blir det fler kommandon än så är det dags
/// för en riktig argumentparser — men inte innan dess.
pub fn secret_action_from_args() -> Option<SecretAction> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--set-secret" if i + 2 < args.len() => {
                return Some(SecretAction::Set {
                    name: args[i + 1].clone(),
                    value: args[i + 2].clone(),
                })
            }
            "--remove-secret" if i + 1 < args.len() => {
                return Some(SecretAction::Remove {
                    name: args[i + 1].clone(),
                })
            }
            "--list-secrets" => return Some(SecretAction::List),
            _ => {}
        }
        i += 1;
    }
    None
}
