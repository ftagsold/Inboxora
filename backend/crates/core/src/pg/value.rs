//! Loosely typed query parameters with node-pg semantics.
//!
//! node-pg sends every parameter as text and lets the server cast it, so the Node code freely
//! binds a JS string to a `uuid`, `int` or `timestamptz` placeholder. tokio-postgres binds in
//! binary with strict Rust types. [`Val`] bridges the two: it accepts any parameter type the
//! server inferred and encodes the value for that type, failing only when the value cannot be
//! represented (the same cases where PostgreSQL would reject node-pg's text).

use std::error::Error;

use bytes::{BufMut, BytesMut};
use chrono::{DateTime, NaiveDate, Utc};
use postgres_protocol::types as pt;
use postgres_types::{IsNull, Kind, ToSql, Type, to_sql_checked};

type BoxError = Box<dyn Error + Sync + Send>;

/// A query parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Json(serde_json::Value),
    Time(DateTime<Utc>),
    Bytes(Vec<u8>),
    Array(Vec<Val>),
}

impl Val {
    fn as_text(&self) -> Option<String> {
        match self {
            Val::Str(s) => Some(s.clone()),
            Val::Int(i) => Some(i.to_string()),
            Val::Float(f) => Some(crate::json::js_number_string(*f)),
            Val::Bool(b) => Some(b.to_string()),
            Val::Json(serde_json::Value::String(s)) => Some(s.clone()),
            Val::Json(v) => Some(v.to_string()),
            Val::Time(t) => Some(crate::json::js_iso(t)),
            Val::Null | Val::Bytes(_) | Val::Array(_) => None,
        }
    }

    fn as_i64(&self) -> Result<i64, BoxError> {
        match self {
            Val::Int(i) => Ok(*i),
            Val::Float(f) if f.fract() == 0.0 && f.is_finite() => Ok(*f as i64),
            Val::Bool(b) => Ok(i64::from(*b)),
            Val::Str(s) => Ok(s.trim().parse::<i64>()?),
            Val::Json(serde_json::Value::Number(n)) => n.as_i64().ok_or_else(|| "invalid integer".into()),
            Val::Json(serde_json::Value::String(s)) => Ok(s.trim().parse::<i64>()?),
            other => Err(format!("invalid input syntax for type integer: {other:?}").into()),
        }
    }

    fn as_f64(&self) -> Result<f64, BoxError> {
        match self {
            Val::Int(i) => Ok(*i as f64),
            Val::Float(f) => Ok(*f),
            Val::Str(s) => Ok(s.trim().parse::<f64>()?),
            Val::Json(serde_json::Value::Number(n)) => n.as_f64().ok_or_else(|| "invalid number".into()),
            Val::Json(serde_json::Value::String(s)) => Ok(s.trim().parse::<f64>()?),
            other => Err(format!("invalid input syntax for type double precision: {other:?}").into()),
        }
    }

    fn as_bool(&self) -> Result<bool, BoxError> {
        match self {
            Val::Bool(b) => Ok(*b),
            Val::Json(serde_json::Value::Bool(b)) => Ok(*b),
            Val::Int(i) => Ok(*i != 0),
            Val::Str(s) | Val::Json(serde_json::Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "t" | "true" | "y" | "yes" | "on" | "1" => Ok(true),
                "f" | "false" | "n" | "no" | "off" | "0" => Ok(false),
                _ => Err(format!("invalid input syntax for type boolean: \"{s}\"").into()),
            },
            other => Err(format!("invalid input syntax for type boolean: {other:?}").into()),
        }
    }

    fn as_time(&self) -> Result<DateTime<Utc>, BoxError> {
        match self {
            Val::Time(t) => Ok(*t),
            // A JS number bound to a timestamp is rejected by PostgreSQL too, but node code
            // passes epoch milliseconds through `new Date(ms)`, which arrives as Val::Time.
            Val::Str(s) | Val::Json(serde_json::Value::String(s)) => parse_timestamp(s),
            other => Err(format!("invalid input syntax for type timestamp: {other:?}").into()),
        }
    }
}

/// Parse the timestamp spellings the frontend and Node code produce (RFC 3339, PostgreSQL text
/// output, and bare dates, which PostgreSQL reads as midnight).
pub fn parse_timestamp(s: &str) -> Result<DateTime<Utc>, BoxError> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z", "%Y-%m-%d %H:%M:%S%#z"] {
        if let Ok(t) = DateTime::parse_from_str(s, fmt) {
            return Ok(t.with_timezone(&Utc));
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(t.and_utc());
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc());
    }
    Err(format!("invalid input syntax for type timestamp with time zone: \"{s}\"").into())
}

const PG_EPOCH_DAYS_FROM_CE: i32 = 730_120; // 2000-01-01
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

fn base_type(ty: &Type) -> &Type {
    match ty.kind() {
        Kind::Domain(inner) => base_type(inner),
        _ => ty,
    }
}

impl ToSql for Val {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        if matches!(self, Val::Null) {
            return Ok(IsNull::Yes);
        }
        let ty = base_type(ty);
        if let Kind::Array(elem) = ty.kind() {
            let items: Vec<Val> = match self {
                Val::Array(items) => items.clone(),
                Val::Json(serde_json::Value::Array(items)) => items.iter().cloned().map(Val::from).collect(),
                other => {
                    return Err(format!("expected an array for {}, got {other:?}", ty.name()).into());
                }
            };
            let elem = elem.clone();
            let dims = [pt::ArrayDimension { len: i32::try_from(items.len())?, lower_bound: 1 }];
            pt::array_to_sql(
                dims,
                elem.oid(),
                items.iter(),
                |item, buf| match item.to_sql(&elem, buf)? {
                    IsNull::Yes => Ok(postgres_protocol::IsNull::Yes),
                    IsNull::No => Ok(postgres_protocol::IsNull::No),
                },
                out,
            )?;
            return Ok(IsNull::No);
        }
        if let Kind::Enum(_) = ty.kind() {
            out.put_slice(self.as_text().ok_or("invalid enum value")?.as_bytes());
            return Ok(IsNull::No);
        }
        match *ty {
            Type::BOOL => pt::bool_to_sql(self.as_bool()?, out),
            Type::INT2 => pt::int2_to_sql(i16::try_from(self.as_i64()?)?, out),
            Type::INT4 => pt::int4_to_sql(i32::try_from(self.as_i64()?)?, out),
            Type::INT8 => pt::int8_to_sql(self.as_i64()?, out),
            Type::OID => pt::oid_to_sql(u32::try_from(self.as_i64()?)?, out),
            Type::FLOAT4 => pt::float4_to_sql(self.as_f64()? as f32, out),
            Type::FLOAT8 => pt::float8_to_sql(self.as_f64()?, out),
            Type::NUMERIC => super::numeric::encode(&self.as_text().ok_or("invalid numeric")?, out)?,
            Type::UUID => {
                let text = self.as_text().ok_or("invalid uuid")?;
                let id = uuid::Uuid::parse_str(text.trim())
                    .map_err(|_| format!("invalid input syntax for type uuid: \"{text}\""))?;
                pt::uuid_to_sql(*id.as_bytes(), out);
            }
            Type::JSON | Type::JSONB => {
                if *ty == Type::JSONB {
                    out.put_u8(1);
                }
                match self {
                    Val::Json(v) => serde_json::to_writer((&mut *out).writer(), v)?,
                    Val::Str(s) => out.put_slice(s.as_bytes()),
                    other => out.put_slice(serde_json::to_string(&crate::json::val_to_json(other))?.as_bytes()),
                }
            }
            Type::TIMESTAMPTZ | Type::TIMESTAMP => {
                let t = self.as_time()?;
                pt::timestamp_to_sql(t.timestamp_micros() - PG_EPOCH_MICROS, out);
            }
            Type::DATE => {
                let d = match self {
                    Val::Str(s) | Val::Json(serde_json::Value::String(s)) => {
                        match NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
                            Ok(d) => d,
                            Err(_) => parse_timestamp(s)?.date_naive(),
                        }
                    }
                    other => other.as_time()?.date_naive(),
                };
                pt::date_to_sql(chrono::Datelike::num_days_from_ce(&d) - PG_EPOCH_DAYS_FROM_CE, out);
            }
            Type::BYTEA => match self {
                Val::Bytes(b) => pt::bytea_to_sql(b, out),
                Val::Str(s) => pt::bytea_to_sql(s.as_bytes(), out),
                other => return Err(format!("invalid bytea value {other:?}").into()),
            },
            Type::INET | Type::CIDR => {
                let text = self.as_text().ok_or("invalid inet")?;
                let (addr, netmask) = match text.split_once('/') {
                    Some((a, m)) => (a.parse::<std::net::IpAddr>()?, Some(m.parse::<u8>()?)),
                    None => (text.parse::<std::net::IpAddr>()?, None),
                };
                let netmask = netmask.unwrap_or(if addr.is_ipv4() { 32 } else { 128 });
                pt::inet_to_sql(addr, netmask, out);
            }
            _ => {
                // text, varchar, bpchar, name, citext, unknown, xml, tsquery input, …: the
                // binary form of every text-like type is its UTF-8 text.
                out.put_slice(
                    self.as_text().ok_or_else(|| format!("cannot bind {self:?} as {}", ty.name()))?.as_bytes(),
                );
            }
        }
        Ok(IsNull::No)
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Val::Str(v.to_owned())
    }
}
impl From<String> for Val {
    fn from(v: String) -> Self {
        Val::Str(v)
    }
}
impl From<&String> for Val {
    fn from(v: &String) -> Self {
        Val::Str(v.clone())
    }
}
impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Val::Bool(v)
    }
}
impl From<i32> for Val {
    fn from(v: i32) -> Self {
        Val::Int(v.into())
    }
}
impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Val::Int(v)
    }
}
impl From<u32> for Val {
    fn from(v: u32) -> Self {
        Val::Int(v.into())
    }
}
impl From<usize> for Val {
    fn from(v: usize) -> Self {
        Val::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}
impl From<f64> for Val {
    fn from(v: f64) -> Self {
        Val::Float(v)
    }
}
impl From<uuid::Uuid> for Val {
    fn from(v: uuid::Uuid) -> Self {
        Val::Str(v.to_string())
    }
}
impl From<DateTime<Utc>> for Val {
    fn from(v: DateTime<Utc>) -> Self {
        Val::Time(v)
    }
}
impl From<Vec<u8>> for Val {
    fn from(v: Vec<u8>) -> Self {
        Val::Bytes(v)
    }
}
impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(v: Option<T>) -> Self {
        v.map_or(Val::Null, Into::into)
    }
}
impl<T: Into<Val> + Clone> From<&Option<T>> for Val {
    fn from(v: &Option<T>) -> Self {
        v.clone().map_or(Val::Null, Into::into)
    }
}

impl<T: Into<Val>> From<Vec<T>> for Val {
    fn from(v: Vec<T>) -> Self {
        Val::Array(v.into_iter().map(Into::into).collect())
    }
}
impl From<serde_json::Value> for Val {
    /// A JSON value from a request body. Scalars bind like node-pg would bind the JS value;
    /// objects bind as JSON; arrays bind as PostgreSQL arrays (node-pg's array rule).
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Val::Null,
            serde_json::Value::Bool(b) => Val::Bool(b),
            serde_json::Value::Number(n) => {
                n.as_i64().map_or_else(|| Val::Float(n.as_f64().unwrap_or(f64::NAN)), Val::Int)
            }
            serde_json::Value::String(s) => Val::Str(s),
            serde_json::Value::Array(items) => Val::Array(items.into_iter().map(Val::from).collect()),
            obj @ serde_json::Value::Object(_) => Val::Json(obj),
        }
    }
}
impl From<&serde_json::Value> for Val {
    fn from(v: &serde_json::Value) -> Self {
        Val::from(v.clone())
    }
}

/// Bind a value as JSON regardless of shape (node code that calls `JSON.stringify` itself).
pub fn json(v: impl serde::Serialize) -> Val {
    Val::Json(serde_json::to_value(v).unwrap_or(serde_json::Value::Null))
}
