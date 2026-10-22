//! AI provider access. The provider adapters (OpenAI-compatible, Codex OAuth, …) are ported
//! in Phase 6; until then completions are reported as unavailable, which every caller already
//! handles (the Node code returns the same result when no provider is configured).

use serde_json::{Map, Value};

use crate::state::AppState;

/// `aiClassifyMessage`: one of the five category names, or `None` when unavailable.
pub async fn classify_message(_state: &AppState, _message: &Map<String, Value>) -> Option<String> {
    None
}
