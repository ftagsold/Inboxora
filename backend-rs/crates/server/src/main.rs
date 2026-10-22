//! Inboxora backend server.

use std::net::SocketAddr;
use std::sync::Arc;

use inboxora_core::config::AppConfig;
use inboxora_core::events::Events;
use inboxora_core::pg::Db;
use inboxora_core::session::SessionStore;

mod app;
mod http;
mod routes;
mod services;
mod state;
mod ws;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn init_tracing() {
    let level = inboxora_core::config::env("LOG_LEVEL").unwrap_or_else(|| "info".into());
    let filter = tracing_subscriber::EnvFilter::try_new(format!("{level},tokio_postgres=warn,fred=warn"))
        .unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let redis = inboxora_core::redis::connect(&cfg.redis_url, 4).await?;
    let db = Db::connect(&cfg.db)?;

    // While the Node backend runs alongside, it owns migrations (MIGRATIONS=verify).
    let migrations_dir = inboxora_core::migrations::migrations_dir();
    match inboxora_core::config::env("MIGRATIONS").as_deref() {
        Some("verify") => inboxora_core::migrations::verify(&db, &migrations_dir).await?,
        _ => {
            inboxora_core::migrations::run(&db, &migrations_dir).await?;
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
    services::auth::start_auth_event_pruning(db.clone());
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
    let state = state::AppState {
        sessions: SessionStore::new(redis.clone(), &cfg.session_secret),
        cfg: cfg.clone(),
        db,
        redis,
        events,
        on_user_connect: None,
        on_user_disconnect: None,
    };
    let app = app::build(state);
    let addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Inboxora backend running on port {}", cfg.port);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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
