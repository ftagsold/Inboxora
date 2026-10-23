//! Pure mail-engine policy: provider profiles, connection budgets and backoff, CONDSTORE sync
//! planning and move reconciliation.

use std::collections::HashSet;

/// Per-provider capability flags and rate-limit tuning.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderProfile {
    pub name: &'static str,
    pub batch_size: usize,
    pub batch_delay_ms: u64,
    pub error_delay_ms: u64,
    pub batches_per_conn: usize,
    pub fetch_body: bool,
    pub pushes_flags: bool,
    pub speculative_fetch: bool,
    pub skip_folder_patterns: &'static [&'static str],
    pub skip_folder_names: &'static [&'static str],
    pub auto_backfill_existing_on_connect: bool,
    pub connect_stagger_ms: Option<u64>,
    pub flag_poll_every_ticks: Option<u32>,
    pub fresh_inbox_sync: bool,
    pub ignores_changed_since: bool,
    pub idle_keepalive_ms: Option<u64>,
    pub max_persistent_per_host: Option<u32>,
    pub max_sync_interval_ms: Option<u64>,
    pub prefer_fresh_body_fetch: bool,
    pub prefetch_new_bodies: bool,
    pub prefetch_new_bodies_limit: Option<usize>,
    pub uses_idle: bool,
}

const BASE: ProviderProfile = ProviderProfile {
    name: "generic",
    batch_size: 100,
    batch_delay_ms: 1500,
    error_delay_ms: 15000,
    batches_per_conn: 15,
    fetch_body: false,
    pushes_flags: true,
    speculative_fetch: true,
    skip_folder_patterns: &[],
    skip_folder_names: &[],
    auto_backfill_existing_on_connect: true,
    connect_stagger_ms: None,
    flag_poll_every_ticks: None,
    fresh_inbox_sync: false,
    ignores_changed_since: false,
    idle_keepalive_ms: None,
    max_persistent_per_host: None,
    max_sync_interval_ms: None,
    prefer_fresh_body_fetch: false,
    prefetch_new_bodies: true,
    prefetch_new_bodies_limit: None,
    uses_idle: true,
};

pub static GOOGLE: ProviderProfile = ProviderProfile {
    name: "google",
    batch_size: 500,
    batch_delay_ms: 2000,
    error_delay_ms: 30000,
    batches_per_conn: 10,
    pushes_flags: false,
    speculative_fetch: false,
    skip_folder_patterns: &["all mail", "[gmail]/starred", "[gmail]/important"],
    skip_folder_names: &["[gmail]"],
    ..BASE
};

pub static YAHOO: ProviderProfile = ProviderProfile {
    name: "yahoo",
    batch_size: 100,
    batch_delay_ms: 2000,
    error_delay_ms: 30000,
    batches_per_conn: 10,
    speculative_fetch: false,
    ..BASE
};

pub static APPLE: ProviderProfile = ProviderProfile {
    name: "apple",
    batch_size: 200,
    batch_delay_ms: 1000,
    error_delay_ms: 10000,
    batches_per_conn: 20,
    ignores_changed_since: true,
    ..BASE
};

pub static MICROSOFT: ProviderProfile = ProviderProfile { name: "microsoft", ..BASE };

pub static PURELYMAIL: ProviderProfile = ProviderProfile {
    name: "purelymail",
    connect_stagger_ms: Some(1200),
    idle_keepalive_ms: Some(4 * 60 * 1000),
    pushes_flags: false,
    speculative_fetch: false,
    prefer_fresh_body_fetch: true,
    fresh_inbox_sync: false,
    auto_backfill_existing_on_connect: false,
    max_sync_interval_ms: Some(120_000),
    flag_poll_every_ticks: Some(6),
    prefetch_new_bodies: true,
    prefetch_new_bodies_limit: Some(1),
    ..BASE
};

pub static GENERIC: ProviderProfile = ProviderProfile { connect_stagger_ms: Some(500), ..BASE };

/// `providerProfile(account)`: by IMAP host, with Microsoft also recognised by its OAuth flow.
pub fn provider_profile(imap_host: Option<&str>, oauth_provider: Option<&str>) -> &'static ProviderProfile {
    let host = imap_host.unwrap_or("").to_lowercase();
    if host.contains(".gmail.com") || host.contains(".googlemail.com") {
        &GOOGLE
    } else if host.contains(".yahoo.com") || host.contains(".ymail.com") {
        &YAHOO
    } else if host.contains(".icloud.com") || host.contains(".apple.com") || host.contains(".me.com") {
        &APPLE
    } else if host.contains(".outlook.com")
        || host.contains("office365.com")
        || host.contains(".hotmail.com")
        || host.contains(".live.com")
        || oauth_provider == Some("microsoft")
    {
        &MICROSOFT
    } else if host.contains("purelymail.com") {
        &PURELYMAIL
    } else {
        &GENERIC
    }
}

pub fn effective_sync_interval_ms(profile: &ProviderProfile, requested: u64) -> u64 {
    profile.max_sync_interval_ms.map_or(requested, |max| requested.min(max))
}

/// Base gap between successive account connects at startup, scaled by fleet size (≤ 2×).
pub fn connect_stagger_for(profile: &ProviderProfile, account_count: usize) -> u64 {
    let base = profile.connect_stagger_ms.unwrap_or(200) as f64;
    let factor = (1.0 + account_count as f64 / 25.0).min(2.0);
    (base * factor).round() as u64
}

pub const CONNECT_COOLDOWN_BASE_MS: u64 = 30_000;
pub const CONNECT_COOLDOWN_MAX_MS: u64 = 15 * 60_000;

/// Exponential refusal backoff: 30 s, 60 s, 120 s, … capped at 15 min.
pub fn connect_cooldown_ms(failures: u32) -> u64 {
    let n = failures.max(1);
    (CONNECT_COOLDOWN_BASE_MS * 2u64.pow((n - 1).min(5))).min(CONNECT_COOLDOWN_MAX_MS)
}

/// A connection-limit / throttle / temporary refusal (back off rather than retry hard).
pub fn is_connection_refusal(detail: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)connection not available|too many|maximum number|number of connections|rate.?limit|temporarily|try again|connection limit|over quota|throttl|connect timeout",
        )
        .expect("refusal regex")
    });
    RE.is_match(detail)
}

/// Retry IPv4-only only for a timeout on a dual-stack host when the provider did not refuse.
pub fn should_retry_ipv4(err: &str, addresses: &[std::net::IpAddr], saw_refusal: bool) -> bool {
    if saw_refusal {
        return false;
    }
    let v4 = addresses.iter().filter(|a| a.is_ipv4()).count();
    err.to_lowercase().contains("timeout") && v4 > 0 && v4 != addresses.len()
}

/// A positive integer caps; anything else is unlimited (`None`).
pub fn parse_persistent_cap(raw: Option<&str>) -> Option<u32> {
    let s = raw?.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse::<i64>().ok().filter(|n| *n > 0).map(|n| n as u32)
}

pub fn resolve_persistent_cap(env_cap: Option<u32>, profile_cap: Option<u32>) -> Option<u32> {
    match (env_cap.filter(|c| *c > 0), profile_cap.filter(|c| *c > 0)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The first `cap` accounts on a host (stable order) keep IDLE; absent accounts are eligible.
pub fn persistent_eligible(ordered: &[String], account_id: &str, cap: Option<u32>) -> bool {
    let Some(cap) = cap.filter(|c| *c > 0) else { return true };
    ordered.iter().position(|id| id == account_id).is_none_or(|rank| rank < cap as usize)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPlan {
    Unchanged,
    Delta,
    Full,
}

/// The CONDSTORE fetch strategy (`planModseqSync`); modseqs compare as u64.
pub fn plan_modseq_sync(
    stored: Option<u64>,
    server: Option<u64>,
    uid_validity_changed: bool,
    max_known_uid: u64,
    server_exists: u32,
) -> SyncPlan {
    if max_known_uid == 0 && server_exists > 0 {
        return SyncPlan::Full;
    }
    if uid_validity_changed {
        return SyncPlan::Full;
    }
    match (stored, server) {
        (Some(a), Some(b)) if a == b => SyncPlan::Unchanged,
        (Some(_), Some(_)) => SyncPlan::Delta,
        _ => SyncPlan::Full,
    }
}

/// Parse a stored `highest_modseq` (numeric text) the way `BigInt(value)` does.
pub fn parse_modseq(v: Option<&str>) -> Option<u64> {
    v.map(str::trim).filter(|s| !s.is_empty()).and_then(|s| s.split('.').next()?.parse().ok())
}

pub fn folder_sync_due(interval_ms: Option<u64>, last_at_ms: Option<u64>, now_ms: u64) -> bool {
    interval_ms.is_some_and(|i| i > 0 && now_ms.saturating_sub(last_at_ms.unwrap_or(0)) >= i)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveClassification {
    pub succeeded: Vec<u32>,
    pub failed: Vec<u32>,
    pub stale_count: Option<usize>,
    pub mappable: bool,
}

/// Outcome of a bulk move without a UIDPLUS map, from a follow-up UID SEARCH (#407).
pub fn classify_move_by_search(uids: &[u32], remaining: &[u32], dest_arrived: Option<usize>) -> MoveClassification {
    let remaining: HashSet<u32> = remaining.iter().copied().collect();
    let gone: Vec<u32> = uids.iter().copied().filter(|u| !remaining.contains(u)).collect();
    let still: Vec<u32> = uids.iter().copied().filter(|u| remaining.contains(u)).collect();
    if gone.is_empty() {
        return MoveClassification { succeeded: vec![], failed: uids.to_vec(), stale_count: Some(0), mappable: false };
    }
    match dest_arrived {
        None => MoveClassification { succeeded: gone, failed: still, stale_count: None, mappable: false },
        Some(n) if n < gone.len() => MoveClassification {
            succeeded: vec![],
            failed: uids.to_vec(),
            stale_count: Some(gone.len() - n),
            mappable: false,
        },
        Some(n) => {
            let mappable = n == gone.len();
            MoveClassification { succeeded: gone, failed: still, stale_count: Some(0), mappable }
        }
    }
}

/// Whether a folder is skipped by backfill/other-folder sync for this provider.
pub fn skips_folder(profile: &ProviderProfile, path: &str) -> bool {
    let lower = path.to_lowercase();
    profile.skip_folder_patterns.iter().any(|p| lower.contains(p))
        || profile.skip_folder_names.contains(&lower.as_str())
}

/// Sync tuning constants (Node's module-level values).
pub const BODY_PREFETCH_PARTS: [&str; 8] = ["1", "1.1", "1.2", "2", "2.1", "2.2", "1.1.1", "1.2.1"];
pub const FLAG_SCAN_TIMEOUT_MS: u64 = 20_000;
pub const DELTA_SCAN_UID_WINDOW: u64 = 5000;
pub const DEFAULT_SYNC_INTERVAL_MS: u64 = 15_000;
pub const DEFAULT_FOLDER_SYNC_INTERVAL_MS: u64 = 15 * 60_000;
pub const STALE_SYNC_WARN_MS: u64 = 5 * 60_000;
pub const STALENESS_CHECK_MS: u64 = 3 * 60_000;
pub const SYNC_HUNG_MS: u64 = 30_000;
pub const FLAG_PUSH_RECONCILE_MS: u64 = 15_000;
pub const FLAG_PUSH_MAX_ATTEMPTS: u32 = 40;
pub const FLAG_PUSH_PER_CYCLE: usize = 30;
pub const CONNECT_CONCURRENCY_PER_HOST: usize = 3;
pub const BACKGROUND_CONN_MAX_PER_HOST: usize = 2;
pub const POOL_SIZE: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_by_host() {
        assert_eq!(provider_profile(Some("imap.gmail.com"), None).name, "google");
        assert_eq!(provider_profile(Some("IMAP.GMAIL.COM"), None).name, "google");
        assert_eq!(provider_profile(Some("imap.mail.yahoo.com"), None).name, "yahoo");
        assert_eq!(provider_profile(Some("imap.mail.me.com"), None).batch_size, 200);
        assert_eq!(provider_profile(Some("outlook.office365.com"), None).name, "microsoft");
        assert_eq!(provider_profile(Some(""), Some("microsoft")).name, "microsoft");
        assert_eq!(provider_profile(Some(""), Some("google")).name, "generic");
        assert_eq!(provider_profile(None, None).name, "generic");
        let p = provider_profile(Some("imap.purelymail.com"), None);
        assert!(p.prefer_fresh_body_fetch && p.uses_idle && !p.pushes_flags && !p.auto_backfill_existing_on_connect);
        assert_eq!(p.idle_keepalive_ms, Some(240_000));
        assert_eq!(p.prefetch_new_bodies_limit, Some(1));
        assert!(skips_folder(&GOOGLE, "[Gmail]/All Mail"));
        assert!(skips_folder(&GOOGLE, "[Gmail]"));
        assert!(!skips_folder(&GOOGLE, "[Gmail]/Drafts"));
    }

    #[test]
    fn refusal_and_cooldown() {
        for s in [
            "Connection not available",
            "Too many simultaneous connections",
            "rate limit exceeded",
            "ratelimited",
            "IMAP connect timeout (30000ms)",
        ] {
            assert!(is_connection_refusal(s), "{s}");
        }
        assert!(!is_connection_refusal("Socket timeout"));
        assert!(!is_connection_refusal("Authentication failed"));
        assert_eq!(connect_cooldown_ms(0), 30_000);
        assert_eq!(connect_cooldown_ms(2), 60_000);
        assert_eq!(connect_cooldown_ms(6), 900_000);
        assert_eq!(connect_cooldown_ms(60), 900_000);
    }

    #[test]
    fn caps_and_eligibility() {
        assert_eq!(parse_persistent_cap(Some("3")), Some(3));
        assert_eq!(parse_persistent_cap(Some("0")), None);
        assert_eq!(parse_persistent_cap(Some("-2")), None);
        assert_eq!(parse_persistent_cap(Some("abc")), None);
        assert_eq!(parse_persistent_cap(None), None);
        assert_eq!(resolve_persistent_cap(Some(5), Some(3)), Some(3));
        assert_eq!(resolve_persistent_cap(None, None), None);
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|s| (*s).to_owned()).collect();
        assert!(persistent_eligible(&ids, "b", Some(2)));
        assert!(!persistent_eligible(&ids, "c", Some(2)));
        assert!(persistent_eligible(&ids, "z", Some(1)));
        assert!(persistent_eligible(&ids, "c", None));
    }

    #[test]
    fn ipv4_retry() {
        let dual: Vec<std::net::IpAddr> = vec!["::1".parse().unwrap(), "127.0.0.1".parse().unwrap()];
        let v4: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
        assert!(should_retry_ipv4("IMAP connect timeout (30000ms)", &dual, false));
        assert!(!should_retry_ipv4("IMAP connect timeout (30000ms)", &dual, true));
        assert!(!should_retry_ipv4("IMAP connect timeout (30000ms)", &v4, false));
        assert!(!should_retry_ipv4("Authentication failed", &dual, false));
    }

    #[test]
    fn modseq_plans() {
        assert_eq!(plan_modseq_sync(Some(5), Some(5), false, 0, 3), SyncPlan::Full);
        assert_eq!(plan_modseq_sync(Some(5), Some(5), false, 10, 3), SyncPlan::Unchanged);
        assert_eq!(plan_modseq_sync(Some(5), Some(6), false, 10, 3), SyncPlan::Delta);
        assert_eq!(plan_modseq_sync(Some(5), Some(6), true, 10, 3), SyncPlan::Full);
        assert_eq!(plan_modseq_sync(None, Some(6), false, 10, 3), SyncPlan::Full);
        assert_eq!(plan_modseq_sync(Some(6), None, false, 10, 3), SyncPlan::Full);
        assert_eq!(plan_modseq_sync(Some(u64::MAX), Some(u64::MAX), false, 1, 0), SyncPlan::Unchanged);
        assert_eq!(plan_modseq_sync(Some(5), Some(5), false, 0, 0), SyncPlan::Unchanged);
        assert_eq!(parse_modseq(Some("18446744073709551615")), Some(u64::MAX));
    }

    #[test]
    fn folder_sync_and_stagger() {
        assert!(folder_sync_due(Some(1000), None, 5000));
        assert!(!folder_sync_due(Some(0), None, 5000));
        assert!(!folder_sync_due(None, None, 5000));
        assert!(!folder_sync_due(Some(1000), Some(4500), 5000));
        assert_eq!(connect_stagger_for(&GENERIC, 0), 500);
        assert_eq!(connect_stagger_for(&PURELYMAIL, 100), 2400);
        assert_eq!(connect_stagger_for(&GOOGLE, 25), 400);
        assert_eq!(effective_sync_interval_ms(&PURELYMAIL, 300_000), 120_000);
        assert_eq!(effective_sync_interval_ms(&GENERIC, 300_000), 300_000);
    }

    #[test]
    fn move_classification() {
        let r = classify_move_by_search(&[1, 2, 3], &[1, 2, 3], Some(0));
        assert_eq!(r.failed, vec![1, 2, 3]);
        let r = classify_move_by_search(&[1, 2, 3], &[3], None);
        assert_eq!((r.succeeded, r.failed, r.stale_count), (vec![1, 2], vec![3], None));
        let r = classify_move_by_search(&[1, 2, 3], &[], Some(2));
        assert_eq!((r.succeeded.len(), r.failed.len(), r.stale_count), (0, 3, Some(1)));
        let r = classify_move_by_search(&[1, 2], &[], Some(2));
        assert!(r.mappable);
    }
}
