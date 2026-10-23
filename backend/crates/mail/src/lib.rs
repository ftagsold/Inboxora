//! Mail content processing: HTML parsing and sanitisation, snippets, headers, MIME parts.

pub mod body;
pub mod conversation;
pub mod css;
pub mod headers;
pub mod html;
pub mod rules;
pub mod sanitize;
pub mod snippet;
pub mod srcset;
pub mod text;
pub mod util;

/// JS truthiness of a JSON value.
pub fn text_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
