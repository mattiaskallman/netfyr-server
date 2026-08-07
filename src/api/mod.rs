// =====================================================================
// api/mod.rs
// REST-API.
//
// Delat i moduler efter resurs. Alla svar är camelCase, så att
// frontend kan använda dem oförändrade.
//
// FELHANTERING: ett fel blir en JSON-kropp med fältet "error" och en
// vettig statuskod. Meddelandena är på svenska och riktade till en
// operatör, inte till en utvecklare — de hamnar i gränssnittet.
// =====================================================================

pub mod audit;
pub mod auth;
pub mod channels;
pub mod groups;
pub mod hosts;
pub mod logs;
pub mod maintenance;
pub mod overview;
pub mod secrets;
pub mod settings;
pub mod stats;
pub mod transfer;
pub mod users;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Fel som kan lämna API:t.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
        }
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: msg.into(),
        }
    }

    pub fn too_many(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: msg.into(),
        }
    }
}

/// Oväntade fel blir 500. Det underliggande felet loggas men skickas
/// inte ut — en databassökväg eller ett SQL-fel hör inte hemma i ett
/// svar till en webbläsare.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("API-fel: {e:#}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            // 500-fallbacken nås bara vid oväntade fel och har ingen
            // request-kontext att läsa språket ur — svenska är standard.
            message: crate::i18n::internal_error(crate::i18n::Lang::Sv).to_string(),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
