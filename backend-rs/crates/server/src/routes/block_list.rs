//! `/api/block-list` (port of routes/blockList.ts).

use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{delete, get};
use inboxora_core::pg::Q;
use inboxora_core::{Result, params};
use serde_json::{Value, json};

use crate::http::{Auth, Body};
use crate::routes::util::{Js, fail, json};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list).post(add)).route("/{id}", delete(remove))
}

async fn list(State(s): State<AppState>, auth: Auth) -> Result<Js> {
    let rows =
        s.db.json_rows("SELECT * FROM block_list WHERE user_id = $1 ORDER BY created_at DESC", params![&auth.user_id])
            .await
            .map_err(fail("GET /block-list error", "Failed to load block list"))?;
    Ok(Js(Value::Array(rows)))
}

async fn add(State(s): State<AppState>, auth: Auth, body: Body) -> Result<axum::response::Response> {
    let email = match body.str("emailAddress") {
        Some(e) if !e.trim().is_empty() => e.trim().to_lowercase(),
        _ => return Ok(json(400, json!({ "error": "emailAddress is required" }))),
    };
    let run = async {
        let inserted =
            s.db.json_opt(
                "INSERT INTO block_list (user_id, email_address) VALUES ($1, $2) \
                 ON CONFLICT (user_id, email_address) DO NOTHING RETURNING *",
                params![&auth.user_id, &email],
            )
            .await?;
        let row = match inserted {
            Some(r) => Some(r),
            None => {
                s.db.json_opt(
                    "SELECT * FROM block_list WHERE user_id = $1 AND email_address = $2",
                    params![&auth.user_id, &email],
                )
                .await?
            }
        };
        Ok::<_, inboxora_core::Error>(row.map_or(Value::Null, Value::Object))
    };
    let row = run.await.map_err(fail("POST /block-list error", "Failed to add to block list"))?;
    Ok(json(201, row))
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<axum::response::Response> {
    let deleted =
        s.db.opt("DELETE FROM block_list WHERE id = $1 AND user_id = $2 RETURNING id", params![&id, &auth.user_id])
            .await
            .map_err(fail("DELETE /block-list/:id error", "Failed to remove from block list"))?;
    if deleted.is_none() {
        return Ok(json(404, json!({ "error": "Entry not found" })));
    }
    Ok(json(200, json!({ "ok": true })))
}
