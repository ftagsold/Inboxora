//! Route table. Groups are added here as they are ported.

pub mod system;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().merge(system::router())
}
