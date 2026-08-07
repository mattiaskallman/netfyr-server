// =====================================================================
// api/secrets.rs
// Hemligheter.
//
// LÄSNING GER BARA NAMN, ALDRIG VÄRDEN. En hemlighet som en gång
// skrivits kan bytas eller tas bort, men inte läsas tillbaka — varken
// av gränssnittet eller av någon som kommer åt API:t.
//
// Det gör gränssnittet något klumpigare (man kan inte se vad som står
// i fältet), men är det enda försvarbara. Ett lösenord som kan hämtas
// via HTTP är inte skyddat av att ligga krypterat på disk.
// =====================================================================

use axum::extract::{ConnectInfo, Path, State};
use axum::Extension;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use super::{ApiError, ApiResult};
use crate::auth::AuthUser;
use crate::routes::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsView {
    /// Namn på lagrade hemligheter. Aldrig värdena.
    pub names: Vec<String>,
}

pub async fn list(State(state): State<Arc<AppState>>) -> ApiResult<Json<SecretsView>> {
    Ok(Json(SecretsView {
        names: state.secrets.names(),
    }))
}

#[derive(Deserialize)]
pub struct SecretBody {
    pub value: String,
}

pub async fn set(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    Json(body): Json<SecretBody>,
) -> ApiResult<Json<()>> {
    let lang = crate::i18n::load_db(&state.db).await;
    if name.trim().is_empty() {
        return Err(ApiError::bad_request(crate::i18n::name_missing(lang)));
    }
    if body.value.is_empty() {
        return Err(ApiError::bad_request(crate::i18n::secret_empty(lang)));
    }
    state.secrets.set(name.trim(), &body.value)?;
    tracing::info!("hemlighet uppdaterad: {}", name.trim());
    // Värdet nämns aldrig, bara namnet.
    super::audit::record(&state.db, &actor.username, "secret_set", Some(name.trim()), None, Some(&addr.ip().to_string())).await;
    Ok(Json(()))
}

pub async fn remove(
    State(state): State<Arc<AppState>>,
    Extension(actor): Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
) -> ApiResult<Json<()>> {
    state.secrets.remove(name.trim())?;
    tracing::info!("hemlighet borttagen: {}", name.trim());
    super::audit::record(&state.db, &actor.username, "secret_remove", Some(name.trim()), None, Some(&addr.ip().to_string())).await;
    Ok(Json(()))
}
