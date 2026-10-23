//! Message categorisation (port of services/categorizer.ts).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use inboxora_core::pg::{Db, Q, Val};
use inboxora_core::{Result, params};
use inboxora_mail::headers::{Headers, detect_category};
use parking_lot::Mutex;
use serde_json::Value;

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

static SOCIAL: LazyLock<Mutex<HashMap<String, (HashSet<String>, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static GLOBAL_ENABLED: LazyLock<Mutex<HashMap<String, (bool, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn invalidate_social_domain_cache(user_id: &str) {
    SOCIAL.lock().remove(user_id);
}

pub fn invalidate_global_categorization_cache(user_id: &str) {
    GLOBAL_ENABLED.lock().remove(user_id);
}

/// `preferences.categorizationEnabled` for the user (cached 5 minutes).
pub async fn global_categorization_enabled(db: &Db, user_id: &str) -> Result<bool> {
    if let Some((v, exp)) = GLOBAL_ENABLED.lock().get(user_id)
        && *exp > Instant::now()
    {
        return Ok(*v);
    }
    let row = db
        .opt(
            "SELECT (preferences->>'categorizationEnabled')::boolean AS val FROM users WHERE id = $1",
            params![user_id],
        )
        .await?;
    let value = row.and_then(|r| r.get::<_, Option<bool>>(0)) == Some(true);
    GLOBAL_ENABLED.lock().insert(user_id.to_owned(), (value, Instant::now() + CACHE_TTL));
    Ok(value)
}

const SHIPPING_DOMAINS: &[&str] = &[
    "ups.com",
    "pkginfo.ups.com",
    "email.ups.com",
    "fedex.com",
    "email.fedex.com",
    "fedexemail.com",
    "usps.com",
    "informeddelivery.usps.com",
    "dhl.com",
    "dhlparcel.com",
    "ontrac.com",
    "lasership.com",
    "lasership-email.com",
    "dpd.com",
    "dpd.de",
    "dpd.fr",
    "royalmail.com",
    "canadapost.ca",
    "canadapost-postescanada.ca",
    "auspost.com.au",
    "gls-group.eu",
    "gls-group.com",
];

/// Built-in domain sets (insertion order matters: the API lists their names).
pub const BUILTIN_SETS: [(&str, &[&str]); 2] = [
    (
        "social_networks",
        &[
            "facebookmail.com",
            "notification.facebook.com",
            "facebookappmail.com",
            "twitteremail.com",
            "mail.twitter.com",
            "x.com",
            "linkedin.com",
            "notifications.linkedin.com",
            "instagrammail.com",
            "notification.instagram.com",
            "tiktok.com",
            "emailmg.tiktok.com",
            "redditmail.com",
            "reddit.com",
            "pinterest.com",
            "email.pinterest.com",
            "snapchat.com",
            "discordapp.com",
            "discord.com",
        ],
    ),
    (
        "developer_platforms",
        &[
            "github.com",
            "noreply.github.com",
            "notifications.github.com",
            "gitlab.com",
            "noreply.gitlab.com",
            "npmjs.com",
            "stackoverflow.com",
            "hackerrank.com",
            "leetcode.com",
        ],
    ),
];

pub fn builtin_set(name: &str) -> Option<&'static [&'static str]> {
    BUILTIN_SETS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// Enabled social-domain sources for a user (cached 5 minutes).
pub async fn load_social_domains(db: &Db, user_id: &str) -> Result<HashSet<String>> {
    if let Some((d, exp)) = SOCIAL.lock().get(user_id)
        && *exp > Instant::now()
    {
        return Ok(d.clone());
    }
    let rows = db
        .json_rows("SELECT source_type, value, resolved_domains FROM category_list_sources WHERE user_id = $1 AND enabled = true", params![user_id])
        .await?;
    let mut domains = HashSet::new();
    for row in rows {
        let value = row.get("value").and_then(Value::as_str).unwrap_or("");
        match row.get("source_type").and_then(Value::as_str) {
            Some("manual") => {
                domains.insert(value.trim().to_lowercase());
            }
            Some("builtin") => {
                if let Some(set) = builtin_set(value) {
                    domains.extend(set.iter().map(|d| (*d).to_owned()));
                }
            }
            Some("url") => {
                if let Some(list) = row.get("resolved_domains").and_then(Value::as_array) {
                    domains.extend(list.iter().filter_map(Value::as_str).map(|d| d.trim().to_lowercase()));
                }
            }
            _ => {}
        }
    }
    SOCIAL.lock().insert(user_id.to_owned(), (domains.clone(), Instant::now() + CACHE_TTL));
    Ok(domains)
}

fn domain_of(addr: &str) -> Option<&str> {
    addr.find('@').map(|i| &addr[i + 1..])
}

/// `classifyMessage`.
pub fn classify_message(headers: &Headers, from_email: Option<&str>, social: &HashSet<String>) -> &'static str {
    if let Some(from) = from_email.filter(|f| !f.is_empty()) {
        let addr = from.trim().to_lowercase();
        if !social.is_empty() && (social.contains(&addr) || domain_of(&addr).is_some_and(|d| social.contains(d))) {
            return "social";
        }
        if domain_of(&addr).is_some_and(|d| !d.is_empty() && SHIPPING_DOMAINS.contains(&d)) {
            return "automated";
        }
    }
    detect_category(headers).unwrap_or("primary")
}

/// `backfillCategories`: social/newsletter categories for uncategorised messages.
pub async fn backfill_categories(engine: &crate::engine::EngineRef, account_id: &str, user_id: &str) -> Result<u64> {
    let db = &engine.db;
    let social = load_social_domains(db, user_id).await?;
    const BATCH: i64 = 500;
    // Keyset paging: rows that get a category leave the `category IS NULL` set, so an OFFSET
    // would skip the rows behind them.
    let mut after: Option<(String, String)> = None;
    let mut processed = 0u64;
    loop {
        let rows = db
            .json_rows(
                "SELECT id, account_id, folder, uid, is_bulk, date::text AS sort_date FROM messages \
                 WHERE account_id = $1 AND category IS NULL AND is_deleted = false \
                   AND ($3::timestamptz IS NULL OR (date, id) < ($3::timestamptz, $4::uuid)) \
                 ORDER BY date DESC, id DESC LIMIT $2",
                params![account_id, BATCH, after.as_ref().map(|a| a.0.clone()), after.as_ref().map(|a| a.1.clone())],
            )
            .await?;
        if rows.is_empty() {
            break;
        }
        after = rows
            .last()
            .and_then(|r| Some((r.get("sort_date")?.as_str()?.to_owned(), r.get("id")?.as_str()?.to_owned())));
        let locs: Vec<crate::engine::cache::Location> =
            rows.iter().filter_map(|r| r.as_object().and_then(crate::engine::cache::Location::from_row)).collect();
        // Only the social check needs the sender; skip the fetch when there are no social domains.
        let senders = if social.is_empty() { Default::default() } else { engine.message_senders(&locs).await };
        let (mut ids, mut cats): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        for (r, loc) in rows.iter().zip(&locs) {
            let from: Option<String> = senders.get(&loc.id).map(|(e, _)| e.clone());
            let bulk: Option<bool> = r.get("is_bulk").and_then(serde_json::Value::as_bool);
            let mut category = "primary";
            if let Some(f) = from.filter(|f| !f.is_empty())
                && !social.is_empty()
            {
                let addr = f.trim().to_lowercase();
                if social.contains(&addr) || domain_of(&addr).is_some_and(|d| social.contains(d)) {
                    category = "social";
                }
            }
            if category == "primary" && bulk == Some(true) {
                category = "newsletter";
            }
            if category != "primary" {
                ids.push(loc.id.to_string());
                cats.push(category.to_owned());
            }
        }
        if !ids.is_empty() {
            db.exec(
                "UPDATE messages SET category = v.category FROM (SELECT unnest($1::uuid[]) AS id, unnest($2::text[]) AS category) AS v WHERE messages.id = v.id",
                &[Val::from(ids), Val::from(cats)],
            )
            .await?;
        }
        processed += rows.len() as u64;
        if (rows.len() as i64) < BATCH || after.is_none() {
            break;
        }
    }
    Ok(processed)
}
