//! Inbox rule validation helpers shared by the rules routes and engine (ports from
//! `routes/rules.ts` and `services/inboxRules.ts`).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

/// `isDangerousRegex`: too long, catastrophic-backtracking shapes, or not a valid pattern.
pub fn is_dangerous_regex(src: &str) -> bool {
    static QUANT_ALT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(.*[+*]\).*[+*]|\(.*\|.*\).*[+*]").expect("regex"));
    if src.is_empty() || crate::text::js_len(src) > 200 {
        return true;
    }
    if QUANT_ALT.is_match(src) {
        return true;
    }
    let chars: Vec<char> = src.chars().collect();
    let mut group_has_quant: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            i += 2;
            continue;
        }
        if c == '[' {
            i += 1;
            while i < chars.len() && chars[i] != ']' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '(' {
            group_has_quant.push(false);
        } else if matches!(c, '*' | '+' | '{' | '?') {
            if let Some(last) = group_has_quant.last_mut() {
                *last = true;
            }
        } else if c == ')' {
            let inner = group_has_quant.pop().unwrap_or(false);
            if inner && chars.get(i + 1).is_some_and(|n| matches!(n, '*' | '+' | '{' | '?')) {
                return true;
            }
        }
        i += 1;
    }
    compile_js_regex(src, false).is_none()
}

/// Compile a user pattern with JS-like semantics (`i` flag optional). Plain patterns use the
/// linear-time engine; lookaround/backreferences fall back to fancy-regex.
pub fn compile_js_regex(src: &str, case_insensitive: bool) -> Option<fancy_regex::Regex> {
    let pattern = if case_insensitive { format!("(?i){src}") } else { src.to_owned() };
    fancy_regex::RegexBuilder::new(&pattern).backtrack_limit(100_000).build().ok()
}

const FIELDS_REQUIRING_VALUE: [&str; 5] = ["from", "to", "subject", "body", "header"];

fn js_str(v: Option<&Value>) -> String {
    match v {
        Some(x) if crate::text_truthy(x) => crate::text::js_string(x),
        _ => String::new(),
    }
}

/// `validateConditions`.
pub fn validate_conditions(conditions: &[Value]) -> Option<&'static str> {
    for c in conditions {
        let Some(obj) = c.as_object() else { return Some("Each condition must have a valid field") };
        let Some(field) = obj.get("field").and_then(Value::as_str) else {
            return Some("Each condition must have a valid field");
        };
        if FIELDS_REQUIRING_VALUE.contains(&field) && crate::text::js_trim(&js_str(obj.get("value"))).is_empty() {
            return Some("Condition value cannot be empty");
        }
        if field == "header" && crate::text::js_trim(&js_str(obj.get("headerName"))).is_empty() {
            return Some("Header name is required for header conditions");
        }
        if field == "read_status" {
            let v = obj.get("value").map_or_else(|| "undefined".to_owned(), crate::text::js_string);
            if v != "read" && v != "unread" {
                return Some("Read status condition must be \"read\" or \"unread\"");
            }
        }
        if obj.get("operator").and_then(Value::as_str) == Some("regex") && is_dangerous_regex(&js_str(obj.get("value")))
        {
            return Some("Regex pattern is invalid or too complex (possible catastrophic backtracking)");
        }
    }
    None
}

fn valid_forward_email(v: &str) -> bool {
    // ^[^\s@<>(),;:]+@[^\s@<>(),;:]+\.[^\s@<>(),;:]+$ and no CR/LF/NUL
    let bad = |c: char| c.is_whitespace() || "@<>(),;:".contains(c);
    let Some((local, domain)) = v.split_once('@') else { return false };
    let part_ok = |s: &str| !s.is_empty() && !s.chars().any(bad);
    if !part_ok(local) || v.contains(['\r', '\n', '\0']) {
        return false;
    }
    // The domain needs at least one dot with non-empty parts around some dot.
    let pieces: Vec<&str> = domain.split('.').collect();
    pieces.len() >= 2
        && !domain.chars().any(bad)
        && (1..pieces.len()).any(|i| !pieces[..i].join(".").is_empty() && !pieces[i..].join(".").is_empty())
}

/// `validateActions`.
pub fn validate_actions(actions: &[Value]) -> Option<&'static str> {
    for a in actions {
        if a.get("type").and_then(Value::as_str) != Some("forward") {
            continue;
        }
        let v = a.get("value").and_then(Value::as_str).map(|s| crate::text::js_trim(s).to_owned()).unwrap_or_default();
        if !valid_forward_email(&v) {
            return Some("Forward action requires one valid email address");
        }
    }
    None
}

/// `normalizeActions`: drop malformed entries and duplicate destination/forward actions
/// (first wins); trim move/forward values.
pub fn normalize_actions(actions: &[Value]) -> Vec<Value> {
    let mut dest_seen = false;
    let mut forward_seen = false;
    let mut out = Vec::new();
    for a in actions {
        let Some(obj) = a.as_object() else { continue };
        let Some(t) = obj.get("type").and_then(Value::as_str) else { continue };
        if matches!(t, "move" | "archive" | "delete") {
            if dest_seen {
                continue;
            }
            dest_seen = true;
        }
        if t == "forward" {
            if forward_seen {
                continue;
            }
            forward_seen = true;
        }
        let mut a = a.clone();
        if matches!(t, "move" | "forward")
            && let Some(v) = obj.get("value").and_then(Value::as_str)
        {
            a["value"] = json!(crate::text::js_trim(v));
        }
        out.push(a);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_safety() {
        assert!(is_dangerous_regex("(a+)+"));
        assert!(is_dangerous_regex("(a|a)*"));
        assert!(is_dangerous_regex("(a{1,9}){1,9}"));
        assert!(is_dangerous_regex("["));
        assert!(!is_dangerous_regex("^invoice\\s+\\d+$"));
        assert!(!is_dangerous_regex("(?=x)abc"));
    }

    #[test]
    fn action_rules() {
        let acts = vec![
            json!({"type":"move","value":" A "}),
            json!({"type":"delete"}),
            json!(null),
            json!({"type":"forward","value":" x@y.z "}),
            json!({"type":"forward","value":"q@w.e"}),
        ];
        assert_eq!(
            normalize_actions(&acts),
            vec![json!({"type":"move","value":"A"}), json!({"type":"forward","value":"x@y.z"})]
        );
        assert_eq!(
            validate_actions(&[json!({"type":"forward","value":"a@b"})]),
            Some("Forward action requires one valid email address")
        );
        assert_eq!(validate_actions(&[json!({"type":"forward","value":"a@b.c"})]), None);
    }
}
