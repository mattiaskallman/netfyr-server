// =====================================================================
// api/hosts.rs
// Enheter: läsa, lägga till, ändra, ta bort, kvittera och tysta.
//
// CYKELSKYDDET SITTER HÄR. Utan det kan A bero på B och B på A — blir
// båda nere undertrycker de varandra och inget larm går ut. En tyst
// dubbel nedgång är det värsta tänkbara utfallet, och den enda platsen
// att stoppa det är när beroendet skrivs.
// =====================================================================

use axum::extract::{ConnectInfo, Path, State};
use axum::Extension;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{now_ms, ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::engine::repo;
use crate::routes::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRow {
    pub id: i64,
    pub name: String,
    pub address: String,
    pub group_id: Option<i64>,
    /// Gruppens namn, uppslaget. Skrivning sker mot groupId.
    pub group: Option<String>,
    pub note: Option<String>,
    pub enabled: bool,
    /// Eget svepintervall i sekunder. None = ärv globalt.
    pub interval_sec: Option<u32>,
    pub packet_size: Option<u32>,
    pub slow_threshold_ms: Option<u32>,
    pub fail_period_sec: Option<u32>,
    pub success_period_sec: Option<u32>,
    pub depends_on_address: Option<String>,
    pub snooze_until: Option<i64>,
    /// Mättyp: "icmp" (default), "tcp" eller "http" (etapp 8).
    pub probe_type: String,
    /// Port för tcp/http. None för icmp.
    pub probe_port: Option<u16>,
    /// SMS-larm för just denna enhet. Övervakning och andra kanaler påverkas inte.
    pub sms_enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewHost {
    pub name: String,
    pub address: String,
    pub group_id: Option<i64>,
    pub note: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub interval_sec: Option<u32>,
    pub packet_size: Option<u32>,
    pub slow_threshold_ms: Option<u32>,
    pub fail_period_sec: Option<u32>,
    pub success_period_sec: Option<u32>,
    pub depends_on_address: Option<String>,
    pub probe_type: Option<String>,
    pub probe_port: Option<u16>,
    #[serde(default = "yes")]
    pub sms_enabled: bool,
}

fn yes() -> bool {
    true
}

/// Partiell uppdatering.
///
/// Option<Option<T>> skiljer "fältet skickades inte" från "fältet
/// sattes till null". Skillnaden är betydelsebärande: null betyder
/// ärv globalt, och ett utelämnat fält ska lämnas orört.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPatch {
    pub name: Option<String>,
    pub address: Option<String>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub group_id: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub note: Option<Option<String>>,
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub interval_sec: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub packet_size: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub slow_threshold_ms: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub fail_period_sec: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub success_period_sec: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub depends_on_address: Option<Option<String>>,
    /// None = rör inte. Mättypen är inte nullable — den har alltid ett
    /// värde (icmp är default, inte frånvaro).
    pub probe_type: Option<String>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub probe_port: Option<Option<u16>>,
    pub sms_enabled: Option<bool>,
}

/// Validerar mättyp + port som en ENHET, inte som två fält.
///
/// Port utan tcp/http är lika fel som tcp/http utan port — båda är
/// halva sanningar som får motorn att mäta mot port 0. Anropas med de
/// EFFEKTIVA värdena (efter patch-sammanslagning), samma mönster som
/// cykelkontrollen: felet ska stoppas när det skrivs.
fn validate_probe(
    probe_type: &str,
    probe_port: Option<u16>,
    lang: crate::i18n::Lang,
) -> anyhow::Result<()> {
    let t = crate::engine::types::ProbeType::parse(probe_type);
    // parse() är förlåtande (okänt → icmp); API:t är inte det. En
    // felstavning ska avvisas, inte tyst bli ping.
    if t.as_str() != probe_type {
        anyhow::bail!(crate::i18n::probe_invalid(lang, probe_type));
    }
    match (t.needs_port(), probe_port) {
        (true, None) => anyhow::bail!(crate::i18n::probe_port_required(lang)),
        (false, Some(_)) => anyhow::bail!(crate::i18n::probe_port_icmp(lang)),
        _ => Ok(()),
    }
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<HostRow> {
    Ok(HostRow {
        id: r.get(0)?,
        name: r.get(1)?,
        address: r.get(2)?,
        group_id: r.get(3)?,
        group: r.get(4)?,
        note: r.get(5)?,
        enabled: r.get::<_, i64>(6)? != 0,
        interval_sec: r.get::<_, Option<i64>>(7)?.map(|v| v as u32),
        packet_size: r.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        slow_threshold_ms: r.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        fail_period_sec: r.get::<_, Option<i64>>(10)?.map(|v| v as u32),
        success_period_sec: r.get::<_, Option<i64>>(11)?.map(|v| v as u32),
        depends_on_address: r.get(12)?,
        snooze_until: r.get(13)?,
        probe_type: r.get::<_, Option<String>>(14)?.unwrap_or_else(|| "icmp".into()),
        probe_port: r.get::<_, Option<i64>>(15)?.map(|v| v as u16),
        sms_enabled: r.get::<_, i64>(16)? != 0,
    })
}

const SELECT: &str = "SELECT h.id, h.name, h.address, h.group_id, g.name, h.note, h.enabled,
        h.interval_sec, h.packet_size, h.slow_threshold_ms,
        h.fail_period_sec, h.success_period_sec,
        h.depends_on_address, h.snooze_until, h.probe_type, h.probe_port,
        COALESCE(h.sms_enabled, 1)
        FROM hosts h LEFT JOIN groups g ON g.id = h.group_id";

pub async fn list(State(state): State<Arc<AppState>>) -> ApiResult<Json<Vec<HostRow>>> {
    let rows = state
        .db
        .call(|conn| {
            let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY h.name"))?;
            let rows = stmt.query_map([], read_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await?;
    Ok(Json(rows))
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<NewHost>,
) -> ApiResult<Json<HostRow>> {
    let lang = crate::i18n::load_db(&state.db).await;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request(crate::i18n::name_missing(lang)));
    }
    if body.address.trim().is_empty() {
        return Err(ApiError::bad_request(crate::i18n::address_missing(lang)));
    }

    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();
    let audit_addr = body.address.trim().to_string();
    let now = now_ms();
    let row = state
        .db
        .call(move |conn| {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT id FROM hosts WHERE address = ?1",
                    [body.address.trim()],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() {
                anyhow::bail!(crate::i18n::address_exists(lang));
            }

            // Tom sträng från formuläret = default, inte en mättyp.
            let probe_type = body
                .probe_type
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("icmp")
                .to_string();
            validate_probe(&probe_type, body.probe_port, lang)?;

            // Transaktion: cykeln kan bara prövas efter att raden finns,
            // och en avvisad cykel får inte lämna en halv enhet kvar.
            let tx = conn.unchecked_transaction()?;

            tx.execute(
                "INSERT INTO hosts
                    (name, address, group_id, note, enabled, interval_sec, packet_size,
                     slow_threshold_ms, fail_period_sec, success_period_sec,
                     depends_on_address, probe_type, probe_port, sms_enabled,
                     created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?15)",
                params![
                    body.name.trim(),
                    body.address.trim(),
                    body.group_id,
                    body.note,
                    body.enabled as i64,
                    body.interval_sec,
                    body.packet_size,
                    body.slow_threshold_ms,
                    body.fail_period_sec,
                    body.success_period_sec,
                    body.depends_on_address,
                    probe_type,
                    body.probe_port,
                    body.sms_enabled as i64,
                    now
                ],
            )?;

            let id = tx.last_insert_rowid();
            repo::validate_dependency(&tx, id, body.depends_on_address.as_deref(), lang)?;

            let row = tx.query_row(&format!("{SELECT} WHERE h.id = ?1"), [id], read_row)?;
            tx.commit()?;
            Ok(row)
        })
        .await
        .map_err(|e| ApiError::bad_request(format!("{e}")))?;

    super::audit::record(
        &state.db,
        &actor_name,
        "host_create",
        Some(&audit_addr),
        None,
        Some(&ip),
    )
    .await;

    Ok(Json(row))
}

pub async fn update(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
    Json(p): Json<HostPatch>,
) -> ApiResult<Json<HostRow>> {
    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();
    let lang = crate::i18n::load_db(&state.db).await;
    let now = now_ms();
    let row = state
        .db
        .call(move |conn| {
            let exists: Option<i64> = conn
                .query_row("SELECT id FROM hosts WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            if exists.is_none() {
                anyhow::bail!(crate::i18n::host_not_found(lang));
            }

            // Läses innan macron nedan flyttar fältet.
            let dep_changed = p.depends_on_address.is_some();
            let probe_changed = p.probe_type.is_some() || p.probe_port.is_some();

            let tx = conn.unchecked_transaction()?;

            macro_rules! set {
                ($field:expr, $col:literal) => {
                    if let Some(v) = $field {
                        tx.execute(
                            concat!("UPDATE hosts SET ", $col, " = ?2 WHERE id = ?1"),
                            params![id, v],
                        )?;
                    }
                };
            }

            set!(p.name.as_deref(), "name");
            set!(p.address.as_deref(), "address");
            set!(p.enabled.map(|b| b as i64), "enabled");
            set!(p.sms_enabled.map(|b| b as i64), "sms_enabled");
            set!(p.group_id, "group_id");
            set!(p.note, "note");
            set!(p.interval_sec, "interval_sec");
            set!(p.packet_size, "packet_size");
            set!(p.slow_threshold_ms, "slow_threshold_ms");
            set!(p.fail_period_sec, "fail_period_sec");
            set!(p.success_period_sec, "success_period_sec");
            set!(p.depends_on_address, "depends_on_address");
            set!(p.probe_type.as_deref().map(str::trim), "probe_type");
            set!(p.probe_port, "probe_port");

            // Mättyp och port valideras som ett PAR mot det som faktiskt
            // kommer att gälla — att byta till tcp utan att skicka port
            // är bara fel om enheten inte redan har en.
            if probe_changed {
                let (pt, pp): (Option<String>, Option<i64>) = tx.query_row(
                    "SELECT probe_type, probe_port FROM hosts WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let pt = pt.unwrap_or_else(|| "icmp".into());
                validate_probe(&pt, pp.map(|v| v as u16), lang)?;
            }

            // Kontrolleras EFTER skrivningen, så att kedjan som prövas är
            // den som faktiskt kommer gälla. Felet lämnar transaktionen
            // utan commit, och ändringen rullas tillbaka när tx faller.
            if dep_changed {
                let current: Option<String> = tx.query_row(
                    "SELECT depends_on_address FROM hosts WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )?;
                repo::validate_dependency(&tx, id, current.as_deref(), lang)?;
            }

            tx.execute("UPDATE hosts SET updated_at = ?2 WHERE id = ?1", params![id, now])?;
            let row = tx.query_row(&format!("{SELECT} WHERE h.id = ?1"), [id], read_row)?;
            tx.commit()?;
            Ok(row)
        })
        .await
        .map_err(|e| ApiError::bad_request(format!("{e}")))?;

    super::audit::record(
        &state.db,
        &actor_name,
        "host_update",
        Some(&row.address),
        None,
        Some(&ip),
    )
    .await;

    Ok(Json(row))
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
) -> ApiResult<Json<()>> {
    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();
    let (n, removed_addr) = state
        .db
        .call(move |conn| {
            // Enheter som berodde på den här blir självständiga igen.
            // Att lämna en död referens hade tystat dem för alltid.
            let addr: Option<String> = conn
                .query_row("SELECT address FROM hosts WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            if let Some(a) = &addr {
                conn.execute(
                    "UPDATE hosts SET depends_on_address = NULL WHERE depends_on_address = ?1",
                    [a],
                )?;
                conn.execute("DELETE FROM host_status WHERE address = ?1", [a])?;
            }
            let n = conn.execute("DELETE FROM hosts WHERE id = ?1", [id])?;
            Ok((n, addr))
        })
        .await?;

    if n == 0 {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::not_found(crate::i18n::host_not_found(lang)));
    }

    super::audit::record(
        &state.db,
        &actor_name,
        "host_delete",
        removed_addr.as_deref(),
        None,
        Some(&ip),
    )
    .await;

    Ok(Json(()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnoozeBody {
    /// Minuter framåt. Noll eller utelämnat tar bort tystnaden.
    #[serde(default)]
    pub minutes: i64,
}

pub async fn snooze(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
    Json(body): Json<SnoozeBody>,
) -> ApiResult<Json<()>> {
    let until = if body.minutes > 0 {
        Some(now_ms() + body.minutes * 60_000)
    } else {
        None
    };

    let ip = addr.ip().to_string();
    let actor_name = actor.username.clone();
    let n = state
        .db
        .call(move |conn| {
            let n = conn.execute(
                "UPDATE hosts SET snooze_until = ?2 WHERE id = ?1",
                params![id, until],
            )?;
            Ok(n)
        })
        .await?;

    if n == 0 {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::not_found(crate::i18n::host_not_found(lang)));
    }

    super::audit::record(
        &state.db,
        &actor_name,
        "host_snooze",
        None,
        Some(&format!("enhet {id}, {} minuter", body.minutes)),
        Some(&ip),
    )
    .await;

    Ok(Json(()))
}

/// Kvittera ett larm.
///
/// Kvitteringen stoppar inte bevakningen och ändrar inte bekräftad
/// status — den noterar att någon sett larmet, och vem det var.
pub async fn ack(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<i64>,
) -> ApiResult<Json<()>> {
    let now = now_ms();
    let ip = addr.ip().to_string();
    let username = actor.username.clone();
    let uname = username.clone();
    let n = state
        .db
        .call(move |conn| {
            let addr: Option<String> = conn
                .query_row("SELECT address FROM hosts WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            let Some(a) = addr else { return Ok(0) };
            let n = conn.execute(
                "UPDATE host_status SET acked_at = ?2, acked_by = ?3 WHERE address = ?1",
                params![a, now, uname],
            )?;
            Ok(n)
        })
        .await?;

    if n == 0 {
        let lang = crate::i18n::load_db(&state.db).await;
        return Err(ApiError::not_found(crate::i18n::host_no_status(lang)));
    }

    super::audit::record(
        &state.db,
        &username,
        "host_ack",
        None,
        Some(&format!("enhet {id}")),
        Some(&ip),
    )
    .await;

    Ok(Json(()))
}

/// Hjälpmodul för Option<Option<T>>. Serde behöver den för att skilja
/// "fältet saknas" från "fältet är null".
pub mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}
