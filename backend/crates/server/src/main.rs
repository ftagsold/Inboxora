//! Inboxora backend server.

use std::net::SocketAddr;
use std::sync::Arc;

use inboxora_core::config::AppConfig;
use inboxora_core::events::Events;
use inboxora_core::pg::Db;
use inboxora_core::session::SessionStore;

mod app;
mod engine;
mod frontend;
mod http;
mod routes;
mod services;
mod state;
mod tls;
#[cfg(test)]
mod upgrade_tests;
mod ws;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn init_tracing() {
    let level = inboxora_core::config::env("LOG_LEVEL").unwrap_or_else(|| "info".into());
    let filter = tracing_subscriber::EnvFilter::try_new(format!("{level},tokio_postgres=warn,fred=warn"))
        .unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

/// `inboxora-server healthcheck`: probe `/api/health` on the local port (for distroless images,
/// which have no shell or wget). Exits 0 on HTTP 200.
fn healthcheck() -> ! {
    use std::io::{Read, Write};
    let port = inboxora_core::config::env("PORT").and_then(|p| p.parse::<u16>().ok()).unwrap_or(3000);
    let probe = || -> std::io::Result<bool> {
        let timeout = std::time::Duration::from_secs(4);
        let mut stream = std::net::TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut head = [0u8; 12];
        stream.read_exact(&mut head)?;
        Ok(head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200"))
    };
    std::process::exit(if probe().unwrap_or(false) { 0 } else { 1 });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        healthcheck();
    }
    init_tracing();
    let cfg = match AppConfig::from_env(env!("CARGO_PKG_VERSION")) {
        Ok(c) => Arc::new(c),
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };
    if cfg.app_origin().is_none() {
        tracing::warn!(
            "WARNING: APP_URL is not set — WebSocket origin validation is disabled. Set APP_URL in .env for production."
        );
    }
    inboxora_core::crypto::init_key(&cfg.encryption_key_hex).map_err(anyhow::Error::msg)?;
    let frontend = frontend::Frontend::from_env()?;
    let tls = tls::Tls::from_env()?;

    let redis = inboxora_core::redis::connect(&cfg.redis_url, 4).await?;
    let db = Db::connect(&cfg.db)?;

    // This backend owns the schema; MIGRATIONS=verify only checks it (e.g. a read-only replica).
    let migrations_dir = inboxora_core::migrations::migrations_dir();
    match inboxora_core::config::env("MIGRATIONS").as_deref() {
        Some("verify") => inboxora_core::migrations::verify(&db, &migrations_dir).await?,
        _ => {
            inboxora_core::migrations::run(&db, &migrations_dir).await?;
            services::compaction::run_once(&db).await;
            // The Node migration runner finishes with this backfill.
            let n = services::contact_backfill::backfill_rich_fields(&db).await?;
            if n > 0 {
                tracing::info!("Backfilled rich fields for {n} contact(s)");
            }
        }
    }
    // One-time bootstrap: create any OIDC_<N>_* env-configured providers that don't exist yet.
    services::oidc::seed_env_providers(&db).await;
    match services::contact_backfill::backfill_photos(&db).await {
        Ok(n) if n > 0 => tracing::info!("Backfilled contact photos for {n} contact(s)"),
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }

    let events = Events::new(Some(redis.clone()));
    match inboxora_core::redis::subscriber(&cfg.redis_url).await {
        Ok(sub) => {
            let ev = events.clone();
            tokio::spawn(async move {
                if let Err(e) = ev.run_subscriber(sub).await {
                    tracing::error!("event subscriber stopped: {e}");
                }
            });
        }
        Err(e) => tracing::warn!("event subscriber unavailable: {e}"),
    }

    services::calendar::start_occurrence_scheduler(db.clone());
    services::auth::reload_auth_settings(&db).await;
    // Encrypt any plaintext credentials left from before at-rest encryption existed.
    services::credentials::encrypt_existing(&db).await?;
    // OAuth app credentials configured in the admin UI override the environment.
    routes::integrations::load_integration_configs(&db).await;
    services::retention::start(db.clone());
    // Remote DAV/iCalendar sync. During a side-by-side rollout only one backend should run
    // these schedulers: `BACKGROUND_SYNC=off` leaves them to the other process.
    if inboxora_core::config::env("BACKGROUND_SYNC").as_deref() != Some("off") {
        if let Err(e) = services::dav_sync::start_carddav_schedulers(&db).await {
            tracing::warn!("CardDAV scheduler start failed: {e}");
        }
        if let Err(e) = services::dav_sync::start_calendar_scheduler(&db).await {
            tracing::warn!("External calendar scheduler start failed: {e}");
        }
    }
    let mail = engine::Engine::new(db.clone(), events.clone(), Some(redis.clone()));
    services::remote_dav::init(redis.clone());
    let state = state::AppState {
        sessions: SessionStore::new(redis.clone(), &cfg.session_secret),
        cfg: cfg.clone(),
        db,
        redis,
        events,
        on_user_connect: Some({
            let m = mail.clone();
            Arc::new(move |user: String| {
                let m = m.clone();
                tokio::spawn(async move { m.connect_all_for_user(&user).await });
            })
        }),
        on_user_disconnect: Some({
            let m = mail.clone();
            Arc::new(move |user: String| {
                let m = m.clone();
                tokio::spawn(async move { m.disconnect_user(&user).await });
            })
        }),
        mail: mail.clone(),
    };
    std::sync::LazyLock::force(&crate::routes::diagnostics::STARTED);
    crate::services::conversation_ops::start_ingest_retry(state.mail.clone());
    crate::services::calendar_invite::start_outbox_worker(state.db.clone());
    if inboxora_core::config::env("BACKGROUND_SYNC").as_deref() != Some("off") {
        mail.start();
        let test_mode = inboxora_core::config::env("NODE_ENV").as_deref() == Some("test")
            || inboxora_core::config::env("E2E_DISABLE_IMAP_CONNECT").as_deref() == Some("true");
        if !test_mode {
            let m = mail.clone();
            tokio::spawn(async move { m.connect_all_at_startup().await });
        }
    }
    let app = app::build(state, frontend);
    let addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Inboxora backend running on port {}", cfg.port);
    let http = axum::serve(listener, app.clone().into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal());
    match tls {
        // Open HTTPS connections are dropped once the HTTP server has drained.
        Some(tls) => tokio::select! {
            r = http => r?,
            r = tls.serve(app) => r?,
        },
        None => http.await?,
    }
    Ok(())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("SIGTERM received — shutting down gracefully");
}
