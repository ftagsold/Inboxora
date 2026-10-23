//! `/api/mail`: message reading, flags, folders, bulk actions, snooze, spam and unsubscribe
//! (port of `routes/mail.ts`).

pub mod bulk;
pub mod cache;
pub mod compose;
pub mod conversations;
pub mod folders;
pub mod read;
pub mod single;

use axum::Router;
use axum::routing::{get, patch, post};
use inboxora_core::Result;
use inboxora_core::pg::Q;
use serde_json::Value;

use crate::engine::connect::{Account, load_account};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/messages", get(read::messages))
        .route("/messages/changes", get(cache::changes))
        .route("/meta", post(cache::meta))
        .route("/preload/status", get(cache::preload_status))
        .route("/messages/{id}", get(read::message).delete(single::delete_message))
        .route("/resolve-message", get(read::resolve_message))
        .route("/thread/{thread_id}", get(read::thread))
        .route("/unread-counts", get(read::unread_counts))
        .route("/messages/{id}/body", get(read::body))
        .route("/messages/{id}/headers", get(read::headers))
        .route("/messages/{id}/attachments.zip", get(read::attachments_zip))
        .route("/messages/{id}/attachments/{part}", get(read::attachment))
        .route("/messages/{id}/read", patch(read::mark_read))
        .route("/messages/{id}/star", patch(read::mark_star))
        .route("/sync", post(folders::sync))
        .route("/sync-folders", post(folders::sync_folders))
        .route("/sync-folder", post(folders::sync_folder))
        .route("/mark-all-read", post(folders::mark_all_read))
        .route("/folders", post(folders::create_folder))
        .route("/folders/delete", post(folders::delete_folder))
        .route("/folders/rename", post(folders::rename_folder))
        .route("/folders/empty", post(folders::empty_folder))
        .route("/messages/bulk-read", post(folders::bulk_read))
        .route("/messages/bulk-delete", post(bulk::bulk_delete))
        .route("/mailbox-usage", get(bulk::mailbox_usage))
        .route("/cleanup-preview", get(bulk::cleanup_preview))
        .route("/messages/bulk-move", post(bulk::bulk_move))
        .route("/messages/bulk-archive", post(single::bulk_archive))
        .route("/messages/{id}/snooze", post(single::snooze))
        .route("/messages/{id}/spam", post(single::spam))
        .route("/messages/{id}/ham", post(single::ham))
        .route("/category-counts", get(single::category_counts))
        .route("/messages/{id}/category", patch(single::set_category))
        .route("/messages/{id}/unsubscribe", post(single::unsubscribe))
        .route("/send", post(compose::send))
        .route("/scheduled", get(crate::services::scheduled_send::list))
        .route(
            "/scheduled/{id}",
            get(crate::services::scheduled_send::detail)
                .patch(crate::services::scheduled_send::update)
                .delete(crate::services::scheduled_send::cancel),
        )
        .route("/draft", post(compose::save_draft))
        .route("/draft/{uid}", axum::routing::delete(compose::delete_draft))
        .route("/conversations", get(conversations::list))
        .route("/conversations/{id}", get(conversations::detail))
        .route("/conversations/{id}/diagnostics", get(conversations::diagnostics))
        .route("/conversations/{id}/overrides", get(conversations::get_overrides).post(conversations::create_override))
        .route("/conversations/{id}/archive", post(conversations::archive))
        .route("/conversations/{id}/delete", post(conversations::delete))
        .route("/conversations/{id}/move", post(conversations::move_to))
        .route("/conversations/{id}/read", post(conversations::read))
        .route("/conversations/{id}/star", post(conversations::star))
        .route("/conversations/{id}/merge", post(conversations::merge))
        .route("/conversations/{id}/lock", post(conversations::lock))
        .route("/conversations/{id}/unlock", post(conversations::unlock))
        .route("/conversations/{id}/logical-messages/{lm}/body", get(conversations::logical_body))
        .route("/conversations/{id}/logical-messages/{lm}/split", post(conversations::split))
        .route("/conversations/{id}/logical-messages/{lm}/move", post(conversations::move_logical))
        .route("/conversations/{id}/logical-messages/{lm}/force-include", post(conversations::force_include))
        .route("/conversations/{id}/logical-messages/{lm}/force-exclude", post(conversations::force_exclude))
        .route("/conversations/bulk-archive", post(conversations::bulk_archive).get(conversations::invalid_id))
        .route("/conversations/bulk-delete", post(conversations::bulk_delete).get(conversations::invalid_id))
        .route("/conversations/bulk-read", post(conversations::bulk_read).get(conversations::invalid_id))
        .route("/conversations/bulk-move", post(conversations::bulk_move).get(conversations::invalid_id))
        .route("/conversations/rebuild", post(conversations::rebuild).get(conversations::invalid_id))
        .route("/conversations/rebuild/{job}", get(conversations::rebuild_job))
        .route("/messages/{id}/conversation", get(conversations::message_conversation))
}

pub fn is_uuid(v: &str) -> bool {
    inboxora_mail::util::is_uuid(v)
}

/// `isValidFolderName`: 1–255 characters, no control characters.
pub fn valid_folder_name(name: &str) -> bool {
    let n = inboxora_mail::text::js_len(name);
    n > 0 && n <= 255 && name.chars().all(|c| (c as u32) > 31 && c as u32 != 127)
}

/// An attachment list as an array (JSON text or a JSON array).
pub fn attachments_of(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => {
            serde_json::from_str(if s.is_empty() { "[]" } else { s }).unwrap_or(Value::Array(vec![]))
        }
        Some(v @ Value::Array(_)) => v.clone(),
        _ => Value::Array(vec![]),
    }
}

pub async fn account_of(s: &AppState, id: &str) -> Result<Option<Account>> {
    load_account(&s.db, id).await
}

/// The account when it belongs to `user`.
pub async fn account_for_user(s: &AppState, id: &str, user: &str) -> Result<Option<Account>> {
    Ok(s.db
        .json_opt("SELECT * FROM email_accounts WHERE id = $1 AND user_id = $2", inboxora_core::params![id, user])
        .await?
        .map(Account::from_map))
}
