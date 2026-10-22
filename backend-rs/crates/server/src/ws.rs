//! Realtime WebSocket endpoint (port of `websocket.ts`).
//!
//! Like the Node `ws` server, the upgrade is accepted first and policy failures close the
//! socket with a code: 1008 for a foreign origin, a missing session or a locked session
//! (browsers do not auto-reconnect), 1011 when the session store is unavailable (retryable).

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;

use crate::state::AppState;

enum Gate {
    Close(u16, &'static str),
    Open(String),
}

async fn gate(state: &AppState, headers: &HeaderMap) -> Gate {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    match (state.cfg.app_origin(), origin) {
        (Some(allowed), Some(o)) if o != allowed => return Gate::Close(1008, "Forbidden"),
        // Production without APP_URL: reject browser connections (non-browsers omit Origin).
        (None, Some(_)) if inboxora_core::config::is_production() => {
            return Gate::Close(1008, "Forbidden");
        }
        _ => {}
    }
    let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    let secure = crate::http::middleware::request_is_secure(headers);
    let session = match state.sessions.load(cookie, secure).await {
        Ok(s) => s,
        Err(_) => return Gate::Close(1011, "Session unavailable"),
    };
    let Some(user_id) = session.user_id() else {
        return Gate::Close(1008, "Unauthorized");
    };
    if session.is_locked() {
        return Gate::Close(1008, "Locked");
    }
    Gate::Open(user_id)
}

pub async fn upgrade(State(state): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        let decision = gate(&state, &headers).await;
        run(state, socket, decision).await;
    })
}

async fn run(state: AppState, socket: WebSocket, decision: Gate) {
    let (mut sink, mut stream) = socket.split();
    let user_id = match decision {
        Gate::Close(code, reason) => {
            let _ = sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
            return;
        }
        Gate::Open(u) => u,
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<std::sync::Arc<str>>();
    let _registration = state.events.register(&user_id, tx.clone());
    crate::services::diagnostics_ring::record_ws_connect();
    tracing::info!("WebSocket connected for user {user_id}");
    let _ = tx.send(json!({ "type": "connected" }).to_string().into());
    // Re-establish mail connections if the server restarted (already-connected accounts are skipped).
    if let Some(hook) = &state.on_user_connect {
        hook(user_id.clone());
    }
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(Message::Text(msg.as_ref().into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                if serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("type").cloned())
                    == Some(json!("ping"))
                {
                    let _ = tx.send(json!({ "type": "pong" }).to_string().into());
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    writer.abort();
    crate::services::diagnostics_ring::record_ws_disconnect();
    tracing::info!("WebSocket disconnected");
}
