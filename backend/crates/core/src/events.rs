//! Realtime event fan-out to WebSocket clients.
//!
//! `broadcast(data, user)` delivers to this process's sockets immediately and publishes on the
//! Redis channel [`CHANNEL`] so other backend processes (another replica, or the Node backend
//! during the migration) deliver it to theirs. Messages carry the publishing node's id, so a
//! process ignores its own echo.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fred::prelude::*;
use parking_lot::RwLock;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub const CHANNEL: &str = "inboxora:broadcast";

type Sender = mpsc::UnboundedSender<Arc<str>>;

struct Socket {
    id: u64,
    tx: Sender,
}

/// Process-wide WebSocket registry and publisher.
#[derive(Clone)]
pub struct Events {
    inner: Arc<Inner>,
}

struct Inner {
    node_id: String,
    sockets: RwLock<HashMap<String, Vec<Socket>>>,
    next_id: AtomicU64,
    redis: Option<Pool>,
    broadcast_counts: RwLock<HashMap<String, u64>>,
}

/// Handle for one registered socket; dropping it unregisters.
pub struct Registration {
    events: Events,
    user_id: String,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut map = self.events.inner.sockets.write();
        if let Some(list) = map.get_mut(&self.user_id) {
            list.retain(|s| s.id != self.id);
            if list.is_empty() {
                map.remove(&self.user_id);
            }
        }
    }
}

impl Events {
    pub fn new(redis: Option<Pool>) -> Self {
        Self {
            inner: Arc::new(Inner {
                node_id: crate::crypto::random_hex(8),
                sockets: RwLock::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                redis,
                broadcast_counts: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Register an authenticated socket for `user_id`.
    pub fn register(&self, user_id: &str, tx: Sender) -> Registration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.sockets.write().entry(user_id.to_owned()).or_default().push(Socket { id, tx });
        Registration { events: self.clone(), user_id: user_id.to_owned(), id }
    }

    /// Number of open sockets in this process.
    pub fn socket_count(&self) -> usize {
        self.inner.sockets.read().values().map(Vec::len).sum()
    }

    /// Whether this process has a socket for the user.
    pub fn has_user(&self, user_id: &str) -> bool {
        self.inner.sockets.read().contains_key(user_id)
    }

    fn deliver_local(&self, msg: &Arc<str>, user_id: Option<&str>) {
        let map = self.inner.sockets.read();
        let send = |list: &Vec<Socket>| {
            for s in list {
                let _ = s.tx.send(msg.clone());
            }
        };
        match user_id {
            Some(u) => {
                if let Some(list) = map.get(u) {
                    send(list);
                }
            }
            None => map.values().for_each(send),
        }
    }

    /// Send `data` to one user's sockets (or everyone with `None`), across processes.
    pub fn broadcast(&self, data: &Value, user_id: Option<&str>) {
        if let Some(t) = data.get("type").and_then(Value::as_str) {
            *self.inner.broadcast_counts.write().entry(t.to_owned()).or_default() += 1;
        }
        let msg: Arc<str> = serde_json::to_string(data).unwrap_or_default().into();
        self.deliver_local(&msg, user_id);
        if let Some(redis) = &self.inner.redis {
            let envelope = json!({ "origin": self.inner.node_id, "userId": user_id, "data": data }).to_string();
            let redis = redis.clone();
            tokio::spawn(async move {
                if let Err(e) = redis.next().publish::<i64, _, _>(CHANNEL, envelope).await {
                    tracing::warn!("event publish failed: {e}");
                }
            });
        }
    }

    /// Counters for the diagnostics report.
    pub fn broadcast_counts(&self) -> HashMap<String, u64> {
        self.inner.broadcast_counts.read().clone()
    }

    /// Deliver events published by other processes. Runs until the subscriber closes.
    pub async fn run_subscriber(self, subscriber: fred::clients::SubscriberClient) -> Result<(), Error> {
        let mut rx = subscriber.message_rx();
        subscriber.subscribe(CHANNEL).await?;
        while let Ok(message) = rx.recv().await {
            let Some(text) = message.value.as_string() else {
                continue;
            };
            let Ok(envelope) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if envelope.get("origin").and_then(Value::as_str) == Some(self.inner.node_id.as_str()) {
                continue;
            }
            let Some(data) = envelope.get("data") else {
                continue;
            };
            let msg: Arc<str> = serde_json::to_string(data).unwrap_or_default().into();
            self.deliver_local(&msg, envelope.get("userId").and_then(Value::as_str));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_to_the_user_only_and_unregisters_on_drop() {
        let ev = Events::new(None);
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let reg_a = ev.register("a", tx_a);
        let _reg_b = ev.register("b", tx_b);
        ev.broadcast(&json!({"type": "x"}), Some("a"));
        assert_eq!(&*rx_a.recv().await.unwrap(), r#"{"type":"x"}"#);
        assert!(rx_b.try_recv().is_err());
        ev.broadcast(&json!({"type": "all"}), None);
        assert!(rx_a.recv().await.is_some() && rx_b.recv().await.is_some());
        drop(reg_a);
        assert!(!ev.has_user("a"));
        assert_eq!(ev.broadcast_counts().get("x"), Some(&1));
    }
}
