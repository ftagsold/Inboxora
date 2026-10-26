//! Long-lived background loops: a panic in one tick is logged and the loop carries on, so a
//! single bad row or bug never silently stops a timer (health check, retries, schedulers).

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;

/// Run one tick of a background loop, containing a panic to that tick.
pub async fn tick(label: &str, fut: impl Future<Output = ()>) {
    if let Err(p) = AssertUnwindSafe(fut).catch_unwind().await {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        tracing::error!("{label}: tick panicked, continuing: {msg}");
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn panic_is_contained() {
        super::tick("test", async { panic!("boom") }).await;
        let mut ran = false;
        super::tick("test", async { ran = true }).await;
        assert!(ran);
    }
}
