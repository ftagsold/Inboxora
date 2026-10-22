//! Redis connection (sessions, rate limits, event bus).

use fred::clients::SubscriberClient;
use fred::prelude::*;

/// Parse REDIS_URL with the same socket forms the Node backend accepted: `redis+unix://`,
/// `unix://`, `redis+socket://` and bare absolute paths select a Unix socket.
pub fn config_from_url(raw: &str) -> Result<Config, Error> {
    const SOCKET_SCHEMES: [&str; 3] = ["redis+unix://", "unix://", "redis+socket://"];
    if raw.starts_with('/') || SOCKET_SCHEMES.iter().any(|s| raw.starts_with(s)) {
        let path = raw.split_once("://").map_or(raw, |(_, p)| p);
        let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
        return Ok(Config { server: ServerConfig::Unix { path: path.into() }, ..Config::default() });
    }
    Config::from_url(raw)
}

/// Connect a small pool of multiplexed clients.
pub async fn connect(url: &str, size: usize) -> Result<Pool, Error> {
    let config = config_from_url(url)?;
    let pool = Builder::from_config(config)
        .with_connection_config(|c| {
            c.connection_timeout = std::time::Duration::from_secs(5);
        })
        .set_policy(ReconnectPolicy::new_exponential(0, 100, 5_000, 2))
        .build_pool(size)?;
    pool.init().await?;
    Ok(pool)
}

/// A dedicated subscriber client for pub/sub.
pub async fn subscriber(url: &str) -> Result<SubscriberClient, Error> {
    let config = config_from_url(url)?;
    let client = Builder::from_config(config)
        .set_policy(ReconnectPolicy::new_exponential(0, 100, 5_000, 2))
        .build_subscriber_client()?;
    client.init().await?;
    client.manage_subscriptions();
    Ok(client)
}
