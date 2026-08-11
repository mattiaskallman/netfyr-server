// =====================================================================
// routes.rs
// HTTP-router.
//
// Endpointernas implementation ligger i api/. Här bestäms vilka
// sökvägar som finns och vem som får använda dem.
//
// BEHÖRIGHETSMATRISEN (etapp 5)
//
//   Öppet:        /health, /auth/login
//   Inloggad:     läs läget, kvittera och tysta larm, eget konto,
//                 SMS-gatewayens status (läsläge för sidopanelen)
//   Admin:        allt skrivande, kanaler, hemligheter, användare, audit
//
// Kontrollen ligger i middleware på servern. Gränssnittet döljer det
// en icke-admin inte får göra, men det är kosmetika — skyddet är att
// servern säger nej.
// =====================================================================

use axum::{extract::State, http::StatusCode, middleware, routing::{get, post, put}, Json, Router};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::api;
use crate::auth;
use crate::db::Db;
use crate::secrets::Secrets;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub secrets: Secrets,
    pub started_at: Instant,
    /// Sätt Secure-flaggan på sessionskakan. Ska vara true så fort
    /// tjänsten nås över HTTPS (bakom en TLS-terminerande proxy).
    pub secure_cookies: bool,
    /// Sessionens livslängd i timmar.
    pub session_hours: i64,
}

pub fn build(state: AppState, static_dir: Option<std::path::PathBuf>) -> Router {
    let state = Arc::new(state);

    // Öppet: hälsokontrollen (vakthunden har ingen inloggning) och
    // inloggningen själv.
    let public = Router::new()
        .route("/health", get(health))
        .route("/auth/login", post(api::auth::login));

    // Inloggad, båda rollerna: läsa läget och svara på larm.
    let authed = Router::new()
        .route("/auth/logout", post(api::auth::logout))
        .route("/auth/me", get(api::auth::me))
        .route("/auth/password", post(api::auth::change_password))
        .route("/overview", get(api::overview::get))
        .route("/hosts", get(api::hosts::list))
        .route("/hosts/{id}/ack", post(api::hosts::ack))
        .route("/hosts/{id}/snooze", post(api::hosts::snooze))
        .route("/groups", get(api::groups::list))
        .route("/settings", get(api::settings::get))
        .route("/maintenance", get(api::maintenance::list))
        .route("/events", get(api::logs::events))
        .route("/deliveries", get(api::logs::deliveries))
        // Gateway-statusen är läsläge, inte konfiguration — sidopanelens
        // statusruta ska fungera även för rollen user, samma som i
        // desktopvarianten. Den avslöjar operatör/signal men inga
        // hemligheter, och kan inte ändra något.
        .route("/channels/sms/status", get(api::channels::sms_status))
        // Statistik/KPI är läsläge över mätdata — samma information som
        // översikten, bara aggregerad över tid.
        .route("/stats", get(api::stats::get))
        // Push: status är läsläge, och prenumerationen är ett per-enhetsval
        // som varje inloggad användare gör för sin egen klient.
        .route("/push/status", get(api::push::status))
        .route(
            "/push/subscriptions",
            post(api::push::subscribe).delete(api::push::unsubscribe),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_auth,
        ));

    // Administratör: allt som förändrar systemet eller avslöjar
    // konfiguration.
    let admin = Router::new()
        .route("/hosts", post(api::hosts::create))
        .route(
            "/hosts/{id}",
            axum::routing::patch(api::hosts::update).delete(api::hosts::delete),
        )
        .route("/groups", post(api::groups::create))
        .route(
            "/groups/{id}",
            axum::routing::patch(api::groups::rename).delete(api::groups::delete),
        )
        .route("/settings", put(api::settings::update))
        .route(
            "/channels/{name}/config",
            get(api::settings::get_channel).put(api::settings::set_channel),
        )
        .route("/channels/{name}/test", post(api::channels::test))
        // Kanalens på/av påverkar larmflödet för alla — admin.
        .route("/push/enable", post(api::push::enable))
        .route("/push/disable", post(api::push::disable))
        .route("/channels/sms/verify", post(api::channels::sms_verify))
        .route("/sms/sessions", get(api::channels::sms_sessions))
        .route("/maintenance", post(api::maintenance::create))
        .route(
            "/maintenance/{id}",
            axum::routing::patch(api::maintenance::update).delete(api::maintenance::delete),
        )
        .route("/export", get(api::transfer::export))
        .route("/import", post(api::transfer::import))
        .route("/secrets", get(api::secrets::list))
        .route("/secrets/{name}", put(api::secrets::set).delete(api::secrets::remove))
        .route("/users", get(api::users::list).post(api::users::create))
        .route(
            "/users/{id}",
            axum::routing::patch(api::users::update).delete(api::users::delete),
        )
        .route("/audit", get(api::audit::list))
        // Att radera all mätdata är destruktivt och globalt — admin.
        .route("/stats/history", axum::routing::delete(api::stats::clear))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_admin,
        ));

    let api_routes = public.merge(authed).merge(admin).with_state(state);

    let mut app = Router::new().nest("/api", api_routes);

    // Frontend serveras från samma binär. Saknas katalogen körs bara
    // API:t, vilket är det normala läget under utveckling.
    if let Some(dir) = static_dir {
        if dir.is_dir() {
            tracing::info!("serverar frontend från {}", dir.display());
            app = app.fallback_service(ServeDir::new(dir));
        } else {
            tracing::warn!("static_dir {} finns inte, hoppar över", dir.display());
        }
    }

    app.layer(middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        // Ytterst: skriv om klientadressen bakom TLS-proxyn innan någon
        // handler eller auth-middleware läser den.
        .layer(middleware::from_fn(real_ip))
}

/// Klientens riktiga adress bakom TLS-proxyn.
///
/// I drift når all trafik appen via Caddy på samma maskin — den direkta
/// TCP-peern är då alltid loopback, och utan den här omskrivningen skulle
/// auditloggens "varifrån" bara visa 127.0.0.1. Caddy lägger klientens
/// adress SIST i X-Forwarded-For (ett förfalskat värde från klienten
/// behålls inte — proxyn skriver sin egen observation efter det), så det
/// är sista ledet som är sanningen. Sker anropet direkt (utveckling utan
/// proxy) är peern redan den riktiga klienten och inget ändras.
///
/// Omskrivningen sker genom att ConnectInfo-extensionen byts ut, så alla
/// handlers får rätt adress utan att behöva känna till proxyn.
async fn real_ip(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    mut req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    if addr.ip().is_loopback() {
        // OBS: X-Forwarded-For ar ingen IANA-standardheader, sa http-
        //cratet har ingen konstant for den — strängliteralen galler.
        let real = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|xff| xff.rsplit(',').map(str::trim).find(|s| !s.is_empty()))
            .and_then(|last| last.parse::<std::net::IpAddr>().ok());
        if let Some(ip) = real {
            req.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::new(ip, addr.port()),
            ));
        }
    }
    next.run(req).await
}

/// Säkerhetsheaders på varje svar.
///
/// CSP:n är snäv: allt kommer från samma ursprung, inga externa
/// teckensnitt eller skript (air-gap-principen). 'unsafe-inline' för
/// stil behövs eftersom gränssnittet sätter style-attribut dynamiskt.
async fn security_headers(req: axum::extract::Request, next: middleware::Next) -> axum::response::Response {
    let is_api = req.uri().path().starts_with("/api");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert(
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self'; font-src 'self'; \
         frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
            .parse()
            .unwrap(),
    );
    if is_api {
        // API-svar ska aldrig fastna i en cache — de innehåller både
        // läge och konfiguration.
        h.insert("cache-control", "no-store".parse().unwrap());
    }
    res
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Health {
    status: &'static str,
    version: &'static str,
    uptime_sec: u64,
    database: &'static str,
    hosts: Option<i64>,
}

/// Hälsokontroll.
///
/// Returnerar 503 när databasen inte svarar. Det gör endpointen
/// användbar för en extern vakthund: en tjänst som svarar 200 men inte
/// kan läsa sin databas övervakar ingenting.
async fn health(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Health>) {
    let uptime_sec = state.started_at.elapsed().as_secs();

    match state.db.ping().await {
        Ok(()) => {
            let hosts = state.db.host_count().await.ok();
            (
                StatusCode::OK,
                Json(Health {
                    status: "ok",
                    version: env!("CARGO_PKG_VERSION"),
                    uptime_sec,
                    database: "ok",
                    hosts,
                }),
            )
        }
        Err(e) => {
            tracing::error!("hälsokontroll: databasen svarar inte: {e:#}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(Health {
                    status: "degraded",
                    version: env!("CARGO_PKG_VERSION"),
                    uptime_sec,
                    database: "error",
                    hosts: None,
                }),
            )
        }
    }
}
