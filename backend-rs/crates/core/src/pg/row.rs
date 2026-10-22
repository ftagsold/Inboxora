//! Row decoding with node-pg (`pg-types`) semantics, for handlers that return rows as JSON.
//!
//! Differences from the "natural" Rust mapping that clients depend on:
//! `int8`/`numeric` become strings, `date`/`timestamp[tz]` become JS `Date`s serialised by
//! `toISOString()` (millisecond precision, `Z`, the server process runs in UTC), `bytea` becomes
//! a serialised Node `Buffer`, and unknown types fall back to their text form.

use std::error::Error;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use fallible_iterator::FallibleIterator;
use postgres_protocol::types as pt;
use postgres_types::{FromSql, Kind, Type};
use serde_json::{Map, Number, Value};
use tokio_postgres::Row;

use crate::json::{js_float, js_iso};

type BoxError = Box<dyn Error + Sync + Send>;

/// A column value decoded the way node-pg would hand it to `res.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeJson(pub Value);

const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

fn timestamp(raw: &[u8]) -> Result<Value, BoxError> {
    let micros = pt::timestamp_from_sql(raw)?;
    // pg-types yields an invalid Date for ±infinity, which JSON.stringify prints as null.
    if micros == i64::MAX || micros == i64::MIN {
        return Ok(Value::Null);
    }
    // JS Date truncates sub-millisecond precision.
    let millis = (micros + PG_EPOCH_MICROS).div_euclid(1000);
    Ok(DateTime::<Utc>::from_timestamp_millis(millis).map_or(Value::Null, |t| Value::String(js_iso(&t))))
}

fn date(raw: &[u8]) -> Result<Value, BoxError> {
    let days = pt::date_from_sql(raw)?;
    if days == i32::MAX || days == i32::MIN {
        return Ok(Value::Null);
    }
    let base = NaiveDate::from_ymd_opt(2000, 1, 1).expect("valid epoch");
    let d = base + Duration::days(i64::from(days));
    Ok(Value::String(js_iso(&d.and_hms_opt(0, 0, 0).expect("midnight").and_utc())))
}

fn interval(raw: &[u8]) -> Result<Value, BoxError> {
    // postgres-interval's JSON: only the non-zero fields among years, months, days, hours,
    // minutes, seconds, milliseconds.
    if raw.len() != 16 {
        return Err("bad interval".into());
    }
    let micros = i64::from_be_bytes(raw[0..8].try_into()?);
    let days = i32::from_be_bytes(raw[8..12].try_into()?);
    let months = i32::from_be_bytes(raw[12..16].try_into()?);
    let mut m = Map::new();
    let mut put = |k: &str, v: i64| {
        if v != 0 {
            m.insert(k.into(), Value::from(v));
        }
    };
    put("years", i64::from(months / 12));
    put("months", i64::from(months % 12));
    put("days", i64::from(days));
    let total_ms = micros / 1000;
    put("hours", total_ms / 3_600_000);
    put("minutes", (total_ms / 60_000) % 60);
    put("seconds", (total_ms / 1000) % 60);
    put("milliseconds", total_ms % 1000);
    Ok(Value::Object(m))
}

fn inet(raw: &[u8], cidr: bool) -> Result<Value, BoxError> {
    let v = pt::inet_from_sql(raw)?;
    let addr = v.addr();
    let full = if addr.is_ipv4() { 32 } else { 128 };
    Ok(Value::String(if cidr || v.netmask() != full { format!("{addr}/{}", v.netmask()) } else { addr.to_string() }))
}

fn decode(ty: &Type, raw: &[u8]) -> Result<Value, BoxError> {
    if let Kind::Domain(inner) = ty.kind() {
        return decode(inner, raw);
    }
    if let Kind::Array(elem) = ty.kind() {
        let array = pt::array_from_sql(raw)?;
        let values: Vec<Value> = array
            .values()
            .map(|v| {
                Ok(match v {
                    Some(bytes) => decode(elem, bytes)?,
                    None => Value::Null,
                })
            })
            .collect()?;
        // Multi-dimensional arrays nest; node-pg does the same.
        let dims: Vec<usize> = array.dimensions().map(|d| Ok(d.len as usize)).collect()?;
        return Ok(nest(values, &dims));
    }
    Ok(match *ty {
        Type::BOOL => Value::Bool(pt::bool_from_sql(raw)?),
        Type::INT2 => Value::from(pt::int2_from_sql(raw)?),
        Type::INT4 => Value::from(pt::int4_from_sql(raw)?),
        Type::OID => Value::from(pt::oid_from_sql(raw)?),
        Type::INT8 => Value::String(pt::int8_from_sql(raw)?.to_string()),
        Type::FLOAT4 => js_float(f64::from(pt::float4_from_sql(raw)?)),
        Type::FLOAT8 => js_float(pt::float8_from_sql(raw)?),
        Type::NUMERIC => Value::String(super::numeric::decode(raw)?),
        Type::JSON => serde_json::from_slice(raw)?,
        Type::JSONB => serde_json::from_slice(raw.get(1..).ok_or("bad jsonb")?)?,
        Type::UUID => Value::String(uuid::Uuid::from_bytes(pt::uuid_from_sql(raw)?).to_string()),
        Type::TIMESTAMP | Type::TIMESTAMPTZ => timestamp(raw)?,
        Type::DATE => date(raw)?,
        Type::INTERVAL => interval(raw)?,
        Type::INET => inet(raw, false)?,
        Type::CIDR => inet(raw, true)?,
        Type::BYTEA => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("Buffer".into()));
            m.insert("data".into(), Value::Array(raw.iter().map(|b| Value::from(*b)).collect()));
            Value::Object(m)
        }
        Type::CHAR => Value::String(char::from(raw.first().copied().unwrap_or(0)).to_string()),
        _ => Value::String(String::from_utf8_lossy(raw).into_owned()),
    })
}

fn nest(values: Vec<Value>, dims: &[usize]) -> Value {
    if dims.len() <= 1 {
        return Value::Array(values);
    }
    let inner: usize = dims[1..].iter().product();
    if inner == 0 {
        return Value::Array(Vec::new());
    }
    let mut out = Vec::with_capacity(dims[0]);
    let mut it = values.into_iter();
    for _ in 0..dims[0] {
        let chunk: Vec<Value> = it.by_ref().take(inner).collect();
        out.push(nest(chunk, &dims[1..]));
    }
    Value::Array(out)
}

impl<'a> FromSql<'a> for NodeJson {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        decode(ty, raw).map(NodeJson)
    }

    fn from_sql_null(_: &Type) -> Result<Self, BoxError> {
        Ok(NodeJson(Value::Null))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

/// Convert a row into a JSON object keyed by column name, in column order (like node-pg).
/// A duplicate column name keeps the last value, as node-pg does.
pub fn row_to_json(row: &Row) -> Result<Map<String, Value>, tokio_postgres::Error> {
    let mut m = Map::with_capacity(row.len());
    for (i, col) in row.columns().iter().enumerate() {
        let NodeJson(v) = row.try_get::<_, NodeJson>(i)?;
        m.insert(col.name().to_owned(), v);
    }
    Ok(m)
}

/// Convert many rows.
pub fn rows_to_json(rows: &[Row]) -> Result<Vec<Value>, tokio_postgres::Error> {
    rows.iter().map(|r| row_to_json(r).map(Value::Object)).collect()
}

/// Number helper used by callers that build JSON by hand from typed columns.
pub fn num(n: impl Into<Number>) -> Value {
    Value::Number(n.into())
}
