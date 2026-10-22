//! Shared application state.

use std::sync::Arc;

use inboxora_core::config::AppConfig;
use inboxora_core::events::Events;
use inboxora_core::pg::Db;
use inboxora_core::session::SessionStore;

/// Called when an authenticated WebSocket connects (the mail engine reconnects the user's
/// accounts if needed).
pub type OnUserConnect = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<AppConfig>,
    pub db: Db,
    pub redis: fred::clients::Pool,
    pub sessions: SessionStore,
    pub events: Events,
    pub on_user_connect: Option<OnUserConnect>,
}

impl axum::extract::FromRef<AppState> for Db {
    fn from_ref(s: &AppState) -> Self {
        s.db.clone()
    }
}
