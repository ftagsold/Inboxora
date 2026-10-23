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
    js_float_text(f)
}

/// serde_json formatter that prints numbers like JavaScript (`1.0` → `1`, `1e21` → `1e+21`).
#[derive(Clone, Copy, Default)]
pub struct JsFormatter;

impl serde_json::ser::Formatter for JsFormatter {
    fn write_f64<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, value: f64) -> std::io::Result<()> {
        writer.write_all(js_float_text(value).as_bytes())
    }

    fn write_f32<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, value: f32) -> std::io::Result<()> {
        writer.write_all(js_float_text(f64::from(value)).as_bytes())
    }
}

/// `String(n)` for a finite float, `null` otherwise (JSON.stringify).
fn js_float_text(f: f64) -> String {
    if !f.is_finite() {
        return "null".into();
    }
    if f.fract() == 0.0 && f.abs() < 1e21 {
        // Shortest round-trip digits, zero-padded: 2^64 prints as 18446744073709552000.
        return if f == 0.0 { "0".into() } else { format!("{f}") };
    }
    let s = format!("{f:e}");
    // Rust: "1.5e21" / "1e-7"; JS: "1.5e+21" / "1e-7". JS uses plain notation for 1e-6 ≤ |f| < 1e21.
    let abs = f.abs();
    if (1e-6..1e21).contains(&abs) {
        return format!("{f}");
    }
    match s.split_once('e') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
        _ => s,
    }
}

/// `JSON.stringify(value)` (keys in insertion order, JS number formatting).
pub fn stringify(v: &Value) -> String {
    String::from_utf8(to_vec(v)).unwrap_or_else(|_| "null".into())
}

/// `JSON.stringify(value)` as bytes.
pub fn to_vec(v: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, JsFormatter);
    if serde::Serialize::serialize(v, &mut ser).is_err() {
        return b"null".to_vec();
    }
    out
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
/// An integer column from a JSON row: BIGINT arrives as a string (node-postgres parity), INT as
/// a number.
pub fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

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
        // Integral values beyond 2^53 print their shortest round-trip digits, like `String(n)`.
        assert_eq!(js_number_string(12_345_678_901_234_567_890.0), "12345678901234567000");
        assert_eq!(js_number_string(18_446_744_073_709_551_616.0), "18446744073709552000");
        assert_eq!(js_number_string(-0.0), "0");
        assert_eq!(js_number_string(1e21), "1e+21");
        assert_eq!(stringify(&serde_json::json!(1e20)), "100000000000000000000");
        let v: Value = serde_json::from_str(r#"{"a":1.0,"b":2.5,"c":1e21,"d":1e-7,"e":0.000001,"f":-3.0}"#).unwrap();
        assert_eq!(stringify(&v), r#"{"a":1,"b":2.5,"c":1e+21,"d":1e-7,"e":0.000001,"f":-3}"#);
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
