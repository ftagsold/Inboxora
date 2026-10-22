//! JavaScript-compatible JSON helpers.
//!
//! Responses and stored fingerprints were produced by `JSON.stringify`, so a few JS rules must
//! be reproduced exactly: numbers print in shortest round-trip form without a trailing `.0`,
//! `NaN`/`Infinity` become `null`, dates print as `toISOString()`, and object keys keep
//! insertion order (serde_json's `preserve_order` feature is enabled for this crate).

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Number, Value};

use crate::pg::Val;

/// `Date.prototype.toISOString()`.
pub fn js_iso(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A float as a JSON value the way `JSON.stringify` would print it.
pub fn js_float(f: f64) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        return Value::Number(Number::from(f as i64));
    }
    Number::from_f64(f).map_or(Value::Null, Value::Number)
}

/// `String(n)` for a JS number.
pub fn js_number_string(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    match js_float(f) {
        Value::Number(n) => n.to_string(),
        _ => "null".into(),
    }
}

/// `JSON.stringify(value)` for a `serde_json::Value` (keys in insertion order).
pub fn stringify(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

/// Convert a bound parameter back to JSON (used for JSON-typed params built from scalars).
pub fn val_to_json(v: &Val) -> Value {
    match v {
        Val::Null => Value::Null,
        Val::Bool(b) => Value::Bool(*b),
        Val::Int(i) => Value::from(*i),
        Val::Float(f) => js_float(*f),
        Val::Str(s) => Value::String(s.clone()),
        Val::Json(j) => j.clone(),
        Val::Time(t) => Value::String(js_iso(t)),
        Val::Bytes(b) => Value::Array(b.iter().map(|x| Value::from(*x)).collect()),
        Val::Array(items) => Value::Array(items.iter().map(val_to_json).collect()),
    }
}

/// JS truthiness of a JSON value (`if (req.body.x)`).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// A string field, or `None` if missing or not a string (`typeof x === 'string'`).
pub fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_js() {
        assert_eq!(stringify(&js_float(1.0)), "1");
        assert_eq!(stringify(&js_float(0.1 + 0.2)), "0.30000000000000004");
        assert_eq!(stringify(&js_float(f64::NAN)), "null");
        assert_eq!(js_number_string(2.5), "2.5");
    }

    #[test]
    fn iso_has_millis_and_z() {
        let t = DateTime::<Utc>::from_timestamp_millis(1_792_652_366_109).unwrap();
        assert_eq!(js_iso(&t), "2026-10-22T06:59:26.109Z");
    }

    #[test]
    fn stringify_preserves_insertion_order() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":[2,{"z":1,"y":2}]}"#).unwrap();
        assert_eq!(stringify(&v), r#"{"b":1,"a":[2,{"z":1,"y":2}]}"#);
    }
}
