//! Pure conversation-threading helpers (port of `conversationEngine.ts`).

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::headers::decode_mime_words;
use crate::util::normalize_message_id;

static REPLY_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:(?:re|odp|aw|sv|vs|antw|ant|ref|rif|ynt|tr)\s*:\s*)+").expect("regex"));
static FORWARD_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:fwd|fw|przek)\s*:\s*").expect("regex"));

/// Subject used to group a conversation: MIME-decoded, NFKC, whitespace collapsed, reply
/// prefixes removed (forward prefixes kept), lowercased.
pub fn canonical_subject(subject: &str) -> String {
    let decoded: String = decode_mime_words(subject).nfkc().collect();
    let decoded = crate::text::collapse_ws(&decoded);
    if FORWARD_PREFIX.is_match(&decoded) {
        return decoded.to_lowercase();
    }
    REPLY_PREFIX.replace(&decoded, "").trim().to_lowercase()
}

/// `addressOf`: an address from a string (`Name <a@b>` or bare) or `{email|address}` object.
pub fn address_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => {
            let inner =
                s.find('<').and_then(|i| s[i + 1..].find('>').map(|j| &s[i + 1..i + 1 + j])).filter(|x| !x.is_empty());
            Some(inner.map_or_else(|| s.trim().to_lowercase(), str::to_lowercase))
        }
        Value::Object(o) => {
            let pick = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_lowercase);
            pick("email").or_else(|| pick("address"))
        }
        _ => None,
    }
}

/// `incoming` | `outgoing` | `self` | `unknown` relative to the user's identities.
pub fn classify_direction(message: &Value, identities: &[Value]) -> &'static str {
    let mine: Vec<String> = identities.iter().filter_map(address_of).collect();
    let field = |k: &str| message.get(k).filter(|v| crate::text_truthy(v));
    let from = field("from_email").or_else(|| field("from")).or_else(|| field("sender")).and_then(address_of);
    let recipients: Vec<String> = ["to_addresses", "cc_addresses", "delivery_addresses"]
        .iter()
        .filter_map(|k| message.get(*k).and_then(Value::as_array))
        .flatten()
        .filter_map(address_of)
        .collect();
    let from_mine = from.as_ref().is_some_and(|f| mine.contains(f));
    let external = recipients.iter().any(|a| !mine.contains(a));
    if from.is_none() && recipients.is_empty() {
        return "unknown";
    }
    if from_mine && !external {
        return "self";
    }
    if from_mine {
        return "outgoing";
    }
    if recipients.iter().any(|a| mine.contains(a)) {
        return "incoming";
    }
    "unknown"
}

/// SHA-256 hex of `String(value || '')`.
pub fn fingerprint(value: &str) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(value.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// How a message's date reached `logicalMessageIdentity` in the Node code.
#[derive(Debug, Clone)]
pub enum MessageDate {
    /// A JS `Date`: stringified with `Date.prototype.toString()` in the UTC process zone.
    JsDate(DateTime<Utc>),
    /// A string value used verbatim.
    Text(String),
    None,
}

/// `Date.prototype.toString()` for a process running in UTC.
pub fn js_date_to_string(t: &DateTime<Utc>) -> String {
    t.format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)").to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogicalIdentity {
    pub user_id: Option<String>,
    pub canonical_message_id: Option<String>,
    pub raw_message_id: Option<String>,
    pub collision_key: String,
}

/// `logicalMessageIdentity(message, { userId })`.
pub fn logical_message_identity(
    raw_message_id: Option<&str>,
    date: &MessageDate,
    subject: Option<&str>,
    from: Option<&Value>,
    user_id: Option<&str>,
) -> LogicalIdentity {
    let raw = raw_message_id.filter(|s| !s.is_empty());
    let canonical = raw.and_then(normalize_message_id);
    let date_text = match date {
        MessageDate::JsDate(t) => js_date_to_string(t),
        MessageDate::Text(s) => s.clone(),
        MessageDate::None => String::new(),
    };
    let stable = [
        user_id.unwrap_or("").to_owned(),
        canonical.clone().unwrap_or_default(),
        date_text,
        canonical_subject(subject.unwrap_or("")),
        from.and_then(address_of).unwrap_or_default(),
    ]
    .join("\u{001f}");
    LogicalIdentity {
        user_id: user_id.filter(|s| !s.is_empty()).map(str::to_owned),
        canonical_message_id: canonical,
        raw_message_id: raw.map(str::to_owned),
        collision_key: fingerprint(&stable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_and_directions() {
        assert_eq!(canonical_subject("RE: Aw:  Hello\tWorld "), "hello world");
        assert_eq!(canonical_subject("Fwd: Re: x"), "fwd: re: x");
        assert_eq!(canonical_subject("=?utf-8?Q?R=C3=A9union?="), "réunion");
        let msg = serde_json::json!({ "from_email": "Me@x.test", "to_addresses": [{ "email": "you@y.test" }] });
        assert_eq!(classify_direction(&msg, &[serde_json::json!("me@x.test")]), "outgoing");
        let t = DateTime::<Utc>::from_timestamp(1_792_652_366, 0).unwrap();
        assert_eq!(js_date_to_string(&t), "Thu Oct 22 2026 06:59:26 GMT+0000 (Coordinated Universal Time)");
    }
}
