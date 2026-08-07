// =====================================================================
// api/transfer.rs
// Export och import av konfiguration.
//
// Samma filformat som desktopvarianten — en JSON-fil med grupper och
// enheter. Filen ska kunna resa båda hållen: exportera i desktop,
// importera på servern, och tvärtom.
//
// Hemligheter och kanalkonfigurationer följer ALDRIG med. Det är
// avsikten: en export ska kunna mailas utan att läcka lösenord.
//
// Bara admin.
// =====================================================================

use axum::{
    extract::{ConnectInfo, Extension, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{audit, now_ms, ApiError};
use crate::auth::AuthUser;
use crate::engine::repo::find_or_create_group;
use crate::routes::AppState;

/// En enhet i exportfilen. Serverns SMS-val följer med men har default PÅ,
/// så äldre filer och desktop-exporter behåller tidigare beteende.
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExportedHost {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub group: String,
    #[serde(default = "default_interval")]
    pub interval_sec: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub sms_enabled: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub fail_period_sec: Option<u32>,
    #[serde(default)]
    pub success_period_sec: Option<u32>,
    #[serde(default)]
    pub slow_threshold_ms: Option<u32>,
    #[serde(default)]
    pub packet_size: Option<u32>,
    #[serde(default)]
    pub depends_on_address: Option<String>,
    /// Mättyp (etapp 8). Saknas i äldre exportfiler = icmp.
    #[serde(default)]
    pub probe_type: Option<String>,
    #[serde(default)]
    pub probe_port: Option<u16>,
}

fn default_interval() -> u32 {
    10
}
fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFile {
    pub app: String,
    pub version: u32,
    pub exported_at: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<ExportedHost>,
}

// ---- Export -----------------------------------------------------------

pub async fn export(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<ConfigFile>, ApiError> {
    let ip = addr.ip().to_string();
    let actor_name = user.username.clone();

    let file = state
        .db
        .call(|conn| {
            let groups: Vec<String> = {
                let mut s = conn.prepare("SELECT name FROM groups ORDER BY name")?;
                let rows = s
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                rows
            };

            let mut s = conn.prepare(
                "SELECT h.name, h.address, COALESCE(g.name, '') AS grp,
                        h.interval_sec, h.enabled, COALESCE(h.note, ''),
                        h.fail_period_sec, h.success_period_sec,
                        h.slow_threshold_ms, h.packet_size, h.depends_on_address,
                        h.probe_type, h.probe_port, COALESCE(h.sms_enabled, 1)
                 FROM hosts h LEFT JOIN groups g ON g.id = h.group_id
                 ORDER BY h.name",
            )?;
            let hosts = s
                .query_map([], |r| {
                    Ok(ExportedHost {
                        name: r.get(0)?,
                        address: r.get(1)?,
                        group: r.get(2)?,
                        interval_sec: r.get::<_, Option<i64>>(3)?.unwrap_or(10) as u32,
                        enabled: r.get::<_, i64>(4)? != 0,
                        note: r.get(5)?,
                        fail_period_sec: r.get::<_, Option<i64>>(6)?.map(|v| v as u32),
                        success_period_sec: r.get::<_, Option<i64>>(7)?.map(|v| v as u32),
                        slow_threshold_ms: r.get::<_, Option<i64>>(8)?.map(|v| v as u32),
                        packet_size: r.get::<_, Option<i64>>(9)?.map(|v| v as u32),
                        depends_on_address: r.get(10)?,
                        probe_type: r.get(11)?,
                        probe_port: r.get::<_, Option<i64>>(12)?.map(|v| v as u16),
                        sms_enabled: r.get::<_, i64>(13)? != 0,
                    })
                })?
                .collect::<rusqlite::Result<Vec<ExportedHost>>>()?;

            Ok(ConfigFile {
                app: "netfyr".into(),
                version: 1,
                exported_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                groups,
                hosts,
            })
        })
        .await?;

    audit::record(&state.db, &actor_name, "export_config", None, None, Some(&ip)).await;
    Ok(Json(file))
}

// ---- Import -----------------------------------------------------------

#[derive(Serialize)]
pub struct ImportResult {
    imported: usize,
    /// Hoppades över för att adressen redan fanns.
    skipped: usize,
}

pub async fn import(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<ConfigFile>,
) -> Result<Json<ImportResult>, ApiError> {
    let ip = addr.ip().to_string();
    let actor_name = user.username.clone();

    let lang = crate::i18n::load_db(&state.db).await;
    if body.app != "netfyr" {
        return Err(ApiError::bad_request(crate::i18n::not_netfyr_file(lang)));
    }
    if body.hosts.len() > 1000 {
        return Err(ApiError::bad_request(crate::i18n::too_many_hosts(lang)));
    }

    let now = now_ms();
    let result = state
        .db
        .call(move |conn| {
            let tx = conn.unchecked_transaction()?;

            // Grupper först — enheter pekar på dem.
            for g in &body.groups {
                find_or_create_group(&tx, g)?;
            }

            let mut imported = 0usize;
            let mut skipped = 0usize;

            for h in &body.hosts {
                let name = h.name.trim();
                let address = h.address.trim();
                if name.is_empty() || address.is_empty() {
                    skipped += 1;
                    continue;
                }

                // Adressen är nyckeln — finns den redan rör vi den inte.
                let exists: bool = tx
                    .query_row(
                        "SELECT COUNT(*) FROM hosts WHERE address = ?1",
                        [address],
                        |r| r.get::<_, i64>(0),
                    )?
                    > 0;
                if exists {
                    skipped += 1;
                    continue;
                }

                let group_id = find_or_create_group(&tx, &h.group)?;
                let note: Option<String> = if h.note.is_empty() {
                    None
                } else {
                    Some(h.note.clone())
                };
                // Mättypen följer samma regler som API:t — en trasig
                // exportfil ska inte skapa en enhet som mäter mot port 0.
                let probe_type = h
                    .probe_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("icmp");
                let t = crate::engine::types::ProbeType::parse(probe_type);
                let (probe_type, probe_port) = if t.needs_port() && h.probe_port.is_none() {
                    // Port saknas: fall tillbaka på ping — en halv sanning
                    // är värre än att behålla desktopbeteendet.
                    ("icmp", None)
                } else if !t.needs_port() {
                    ("icmp", None)
                } else {
                    (t.as_str(), h.probe_port)
                };
                tx.execute(
                    "INSERT INTO hosts
                        (name, address, group_id, note, enabled, interval_sec,
                         fail_period_sec, success_period_sec,
                         slow_threshold_ms, packet_size, depends_on_address,
                         probe_type, probe_port, sms_enabled, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    rusqlite::params![
                        name,
                        address,
                        group_id,
                        note,
                        h.enabled as i64,
                        h.interval_sec.max(1) as i64,
                        h.fail_period_sec.map(|v| v as i64),
                        h.success_period_sec.map(|v| v as i64),
                        h.slow_threshold_ms.map(|v| v as i64),
                        h.packet_size.map(|v| v as i64),
                        h.depends_on_address,
                        probe_type,
                        probe_port,
                        h.sms_enabled as i64,
                        now,
                        now,
                    ],
                )?;
                imported += 1;
            }

            tx.commit()?;
            Ok(ImportResult { imported, skipped })
        })
        .await?;

    let detail = crate::i18n::imported_detail(lang, result.imported, result.skipped);
    audit::record(&state.db, &actor_name, "import_config", Some(&detail), None, Some(&ip)).await;
    Ok(Json(result))
}
