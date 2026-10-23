//! PostgreSQL `numeric` binary wire format.
//!
//! node-pg returns `numeric` as the exact text PostgreSQL prints, so decoding must reproduce
//! that text (including trailing zeros up to the display scale).

use bytes::{BufMut, BytesMut};

type BoxError = Box<dyn std::error::Error + Sync + Send>;

const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;

fn read_u16(raw: &[u8], at: usize) -> Result<u16, BoxError> {
    raw.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or_else(|| "truncated numeric".into())
}

/// Decode to PostgreSQL's text output.
pub fn decode(raw: &[u8]) -> Result<String, BoxError> {
    let ndigits = read_u16(raw, 0)? as usize;
    let weight = read_u16(raw, 2)? as i16;
    let sign = read_u16(raw, 4)?;
    let dscale = read_u16(raw, 6)? as usize;
    match sign {
        NUMERIC_NAN => return Ok("NaN".into()),
        NUMERIC_PINF => return Ok("Infinity".into()),
        NUMERIC_NINF => return Ok("-Infinity".into()),
        _ => {}
    }
    let mut digits = Vec::with_capacity(ndigits);
    for i in 0..ndigits {
        digits.push(read_u16(raw, 8 + i * 2)?);
    }
    let mut out = String::new();
    if sign == NUMERIC_NEG {
        out.push('-');
    }
    // Integer part: groups 0..=weight.
    if weight < 0 {
        out.push('0');
    } else {
        for i in 0..=(weight as usize) {
            let d = digits.get(i).copied().unwrap_or(0);
            if i == 0 {
                out.push_str(&d.to_string());
            } else {
                out.push_str(&format!("{d:04}"));
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut frac = String::new();
        let mut group = i32::from(weight) + 1;
        while frac.len() < dscale {
            let d = if group < 0 { 0 } else { digits.get(group as usize).copied().unwrap_or(0) };
            frac.push_str(&format!("{d:04}"));
            group += 1;
        }
        frac.truncate(dscale);
        out.push_str(&frac);
    }
    Ok(out)
}

/// Encode a decimal string (as node-pg would send it as text).
pub fn encode(text: &str, out: &mut BytesMut) -> Result<(), BoxError> {
    let t = text.trim();
    let special = match t.to_ascii_lowercase().as_str() {
        "nan" => Some(NUMERIC_NAN),
        "infinity" | "+infinity" | "inf" => Some(NUMERIC_PINF),
        "-infinity" | "-inf" => Some(NUMERIC_NINF),
        _ => None,
    };
    if let Some(sign) = special {
        out.put_u16(0);
        out.put_u16(0);
        out.put_u16(sign);
        out.put_u16(0);
        return Ok(());
    }
    let invalid = || -> BoxError { format!("invalid input syntax for type numeric: \"{text}\"").into() };
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    // Expand exponent notation into plain digits.
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().map_err(|_| invalid())?),
        None => (body, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    let mut all: String = format!("{int_part}{frac_part}");
    let mut point = int_part.len() as i32 + exp; // decimal point position within `all`
    let dscale = (frac_part.len() as i32 - exp).max(0) as u16;
    if point < 0 {
        all = format!("{}{all}", "0".repeat((-point) as usize));
        point = 0;
    }
    if (point as usize) > all.len() {
        all.push_str(&"0".repeat(point as usize - all.len()));
    }
    let (ip, fp) = all.split_at(point as usize);
    let ip = ip.trim_start_matches('0');
    // Group into base-10000 digits aligned on the decimal point.
    let ip_pad = (4 - ip.len() % 4) % 4;
    let ip_full = format!("{}{ip}", "0".repeat(ip_pad));
    let fp_pad = (4 - fp.len() % 4) % 4;
    let fp_full = format!("{fp}{}", "0".repeat(fp_pad));
    let mut groups: Vec<u16> = Vec::new();
    for chunk in ip_full.as_bytes().chunks(4).chain(fp_full.as_bytes().chunks(4)) {
        groups.push(std::str::from_utf8(chunk)?.parse::<u16>()?);
    }
    let mut weight = (ip_full.len() / 4) as i16 - 1;
    // Strip leading and trailing zero groups.
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    out.put_u16(u16::try_from(groups.len())?);
    out.put_i16(weight);
    out.put_u16(if neg && !groups.is_empty() { NUMERIC_NEG } else { NUMERIC_POS });
    out.put_u16(dscale);
    for g in groups {
        out.put_u16(g);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(s: &str) -> String {
        let mut buf = BytesMut::new();
        encode(s, &mut buf).unwrap();
        decode(&buf).unwrap()
    }

    #[test]
    fn roundtrips_postgres_text() {
        for (input, expected) in [
            ("0", "0"),
            ("12.50", "12.50"),
            ("-0.0001", "-0.0001"),
            ("100000000", "100000000"),
            ("1234.5678", "1234.5678"),
            ("0.000012345", "0.000012345"),
            ("1e3", "1000"),
            ("1.5e-2", "0.015"),
            ("NaN", "NaN"),
            ("98765432109876543210.0123", "98765432109876543210.0123"),
        ] {
            assert_eq!(roundtrip(input), expected, "{input}");
        }
    }
}
