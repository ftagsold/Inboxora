//! `req.query` with the `qs` parser's common forms: repeated keys and `a[]` become arrays,
//! `a[b]` becomes an object.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct Qs(pub Map<String, Value>);

fn decode(s: &str) -> String {
    let replaced = s.replace('+', " ");
    percent_encoding::percent_decode_str(&replaced).decode_utf8_lossy().into_owned()
}

fn push(map: &mut Map<String, Value>, key: String, value: Value) {
    match map.get_mut(&key) {
        Some(Value::Array(items)) => items.push(value),
        Some(existing) => {
            let prev = existing.take();
            *existing = Value::Array(vec![prev, value]);
        }
        None => {
            map.insert(key, value);
        }
    }
}

/// Parse a query string.
pub fn parse(raw: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()).take(1000) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(k);
        let value = Value::String(decode(v));
        if let Some(open) = key.find('[').filter(|i| *i > 0 && key.ends_with(']')) {
            let base = key[..open].to_owned();
            let inner = key[open + 1..key.len() - 1].to_owned();
            if inner.is_empty() || inner.bytes().all(|b| b.is_ascii_digit()) {
                match out.get_mut(&base) {
                    Some(Value::Array(items)) => items.push(value),
                    Some(Value::Object(_)) => {}
                    Some(existing) => {
                        let prev = existing.take();
                        *existing = Value::Array(vec![prev, value]);
                    }
                    None => {
                        out.insert(base, Value::Array(vec![value]));
                    }
                }
            } else {
                let entry = out.entry(base).or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(obj) = entry {
                    push(obj, inner, value);
                }
            }
        } else {
            push(&mut out, key, value);
        }
    }
    out
}

impl Qs {
    /// A string parameter (`typeof req.query.x === 'string'`).
    pub fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// First value when repeated, as Express code often does with `String(req.query.x)`.
    pub fn first(&self, key: &str) -> Option<String> {
        match self.0.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Array(items) => items.first().and_then(Value::as_str).map(str::to_owned),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }
}

/// JS `parseInt(s, 10)`: optional whitespace and sign, then leading digits.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

impl<S: Send + Sync> FromRequestParts<S> for Qs {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Qs(parse(parts.uri.query().unwrap_or(""))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_qs() {
        let q = parse("a=1&a=2&b%5B%5D=x&c[d]=y&e=hello+world&f");
        assert_eq!(
            Value::Object(q),
            serde_json::json!({"a":["1","2"],"b":["x"],"c":{"d":"y"},"e":"hello world","f":""})
        );
    }

    #[test]
    fn parse_int_matches_js() {
        assert_eq!(parse_int("42abc"), Some(42));
        assert_eq!(parse_int(" -7"), Some(-7));
        assert_eq!(parse_int("abc"), None);
    }
}
