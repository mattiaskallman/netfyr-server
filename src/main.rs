// =====================================================================
// main.rs
// NetFyr Server — startpunkt.
//
// Etapp 1: skelett. HTTP-server, databas med schema, hälsokontroll.
// Ingen motor, inga kanaler, ingen autentisering.
// =====================================================================

mod api;
mod auth;
mod channels;
mod config;
mod db;
mod engine;
mod i18n;
mod queue;
mod retention;
mod routes;
mod secrets;
mod sms_engine;
mod watchdog;

use anyhow::{Context, Result};
use std::time::Instant;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let path = config::config_path_from_args();

    // Läs konfigurationen innan loggningen sätts upp, så loggnivån kan
    // komma därifrån. Varningar under inläsningen går till stderr.
    let cfg = config::Config::load(&path)?;

    // RUST_LOG vinner över konfigurationsfilen — bekvämt vid felsökning.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("netfyr_server={}", cfg.log_level)));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("NetFyr Server {}", env!("CARGO_PKG_VERSION"));
    tracing::info!("konfiguration: {}", path.display());

    let db = db::Db::open(&cfg.database)
        .with_context(|| format!("databasen {}", cfg.database.display()))?;
    tracing::info!("databas: {}", cfg.database.display());

    // Första körningen: skapa administratörskontot. Engångslösenordet
    // skrivs bara till loggen — det finns ingen annan väg in, och ett
    // lösenord som aldrig visas kan inte bytas.
    match auth::bootstrap_admin(&db).await {
        Ok(Some(password)) => {
            tracing::warn!("=====================================================");
            tracing::warn!("FIRST RUN — administrator account created");
            tracing::warn!("  username: admin");
            tracing::warn!("  one-time password: {password}");
            tracing::warn!("Sign in and change the password immediately.");
            tracing::warn!("=====================================================");
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!("kunde inte skapa administratörskonto: {e:#}");
        }
    }

    // Städloopen: utgångna sessioner och gammal auditdata.
    tokio::spawn(auth::run_janitor(db.clone()));

    // Retention-städningen: gamla mätningar, leveranser och händelser.
    // Samma regler som desktop (180/30/7 dagar), körs vid start + varje dygn.
    tokio::spawn(retention::run(db.clone()));

    // Hemligheter ligger bredvid databasen. Katalogen skapas om den
    // saknas, och nyckeln genereras vid första start.
    let secrets_dir = cfg
        .database
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let secrets = secrets::Secrets::open(&secrets_dir)?;

    // Underhållskommandon. Kör och avsluta — tjänsten startar inte.
    //
    // Finns tills API:t i etapp 4. Utan en väg att sätta hemligheter går
    // kanalerna inte att ta i drift alls.
    if let Some(action) = config::secret_action_from_args() {
        match action {
            config::SecretAction::Set { name, value } => {
                secrets.set(&name, &value)?;
                println!("hemlighet satt: {name}");
            }
            config::SecretAction::Remove { name } => {
                secrets.remove(&name)?;
                println!("hemlighet borttagen: {name}");
            }
            config::SecretAction::List => {
                let names = secrets.names();
                if names.is_empty() {
                    println!("inga hemligheter lagrade");
                } else {
                    for n in names {
                        println!("{n}");
                    }
                }
            }
        }
        return Ok(());
    }
    tracing::info!(
        "hemligheter: {} ({} lagrade)",
        secrets_dir.display(),
        secrets.names().len()
    );

    // Leveranskön körs som en egen task, skild från motorn. Ett
    // långsamt utskick får inte fördröja nästa svep.
    tokio::spawn(queue::Queue::new(db.clone(), secrets.clone()).run());
    tracing::info!("leveranskön startad");

    // SMS-eskaleringen är en egen task, skild från både motorn och kön.
    // En långsam gateway får varken fördröja svepen eller andra larm.
    tokio::spawn(sms_engine::run(db.clone(), secrets.clone()));
    tracing::info!("sms-motorn startad");

    // Vakthunden är en egen task, skild från motorn. Den läser
    // inställningarna ur databasen och håller sin TCP-port uppe bara
    // när bevakningen faktiskt pågår.
    tokio::spawn(watchdog::run(db.clone()));
    tracing::info!("vakthunden startad");

    // Motorn körs som en egen task. Den lever lika länge som processen
    // och avslutas när tokio-runtime rivs vid nedstängning. Pollräknarna
    // är processlokala med flit och börjar därför på noll vid omstart.
    let polls = engine::polls::PollCounters::default();
    match engine::monitor::Monitor::new(db.clone(), polls.clone()) {
        Ok(monitor) => {
            tokio::spawn(monitor.run());
            tracing::info!("övervakningsmotorn startad");
        }
        Err(e) => {
            // Vanligaste orsaken är att ICMP-socketen nekas. Tjänsten
            // startar ändå — gränssnittet och hälsokontrollen ska vara
            // nåbara så att felet går att se utifrån.
            tracing::error!("kunde inte starta motorn: {e:#}");
        }
    }

    let state = routes::AppState {
        db,
        polls,
        secrets,
        started_at: Instant::now(),
        secure_cookies: cfg.secure_cookies,
        session_hours: auth::effective_admin_session_hours(cfg.session_hours),
        admin_idle_minutes: cfg.admin_idle_minutes,
        operator_session_days: cfg.operator_session_days,
    };
    let app = routes::build(state, cfg.static_dir.clone());

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("kunde inte lyssna på {}", cfg.listen))?;
    tracing::info!("lyssnar på http://{}", cfg.listen);
    if !cfg.secure_cookies && cfg.listen.ip().is_unspecified() {
        tracing::warn!(
            "tjänsten lyssnar på alla gränssnitt utan Secure-kakor — \
             avsett för utveckling. Sätt secure_cookies = true bakom HTTPS i drift."
        );
    }

    axum::serve(
        listener,
        // ConnectInfo gör att auditloggen får klientens adress.
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("serverfel")?;

    tracing::info!("avslutad");
    Ok(())
}

/// Vänta på SIGINT eller SIGTERM.
///
/// SIGTERM är det systemd skickar vid `systemctl stop`. Utan hantering
/// dödas processen hårt, och en pågående databasskrivning kan avbrytas
/// mitt i. Med WAL är det inte förödande, men ordnad avstängning är
/// ändå rätt.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("kunde inte lyssna på SIGINT");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("kunde inte lyssna på SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT mottagen, avslutar"),
        _ = terminate => tracing::info!("SIGTERM mottagen, avslutar"),
    }
}
