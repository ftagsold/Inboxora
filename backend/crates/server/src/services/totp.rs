//! TOTP compatible with otplib 12's `authenticator` defaults (SHA-1, 6 digits, 30 s step,
//! window 0, base32 secrets) and the QR codes shown during enrolment.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding (otplib strips `=`).
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// thirty-two's lenient decoder: case-insensitive, stops at `=`, skips unknown characters.
pub fn base32_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => continue,
        };
        buffer = (buffer << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}

/// `authenticator.generateSecret(20)`.
pub fn generate_secret() -> String {
    base32_encode(&inboxora_core::crypto::random_bytes::<20>())
}

fn hotp(key: &[u8], counter: u64) -> String {
    let Ok(mut mac) = <Hmac<Sha1> as KeyInit>::new_from_slice(key) else { return String::new() };
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let code = (u32::from(digest[offset] & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    format!("{:06}", code % 1_000_000)
}

/// The current 6-digit code for a base32 secret.
pub fn generate(secret: &str, unix_seconds: u64) -> String {
    hotp(&base32_decode(secret), unix_seconds / 30)
}

/// `authenticator.verify({ token, secret })`: digits only, current step only.
pub fn verify(token: &str, secret: &str) -> bool {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    inboxora_core::crypto::ct_eq(token.as_bytes(), generate(secret, now).as_bytes())
}

/// `authenticator.keyuri(accountName, issuer, secret)`.
pub fn keyuri(account: &str, issuer: &str, secret: &str) -> String {
    let enc = inboxora_mail::sanitize::encode_uri_component;
    format!(
        "otpauth://totp/{}:{}?secret={secret}&period=30&digits=6&algorithm=SHA1&issuer={}",
        enc(issuer),
        enc(account),
        enc(issuer)
    )
}

/// `QRCode.toDataURL(text)`: a PNG data URL (error correction M, 4 px modules, 4-module margin).
pub fn qr_data_url(text: &str) -> Result<String, String> {
    let code =
        qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).map_err(|e| e.to_string())?;
    let modules = code.width();
    let colors = code.to_colors();
    let (scale, margin) = (4usize, 4usize);
    let size = (modules + 2 * margin) * scale;
    let mut pixels = vec![255u8; size * size];
    for (i, c) in colors.iter().enumerate() {
        if *c == qrcode::Color::Dark {
            let (mx, my) = (i % modules + margin, i / modules + margin);
            for y in my * scale..(my + 1) * scale {
                pixels[y * size + mx * scale..y * size + (mx + 1) * scale].fill(0);
            }
        }
    }
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, size as u32, size as u32);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(&pixels).map_err(|e| e.to_string())?;
    }
    Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_sha1_vectors() {
        // RFC 6238 appendix B, secret "12345678901234567890", truncated to 6 digits.
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(secret, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(generate(&secret, 59), "287082");
        assert_eq!(generate(&secret, 1_111_111_109), "081804");
        assert_eq!(generate(&secret, 2_000_000_000), "279037");
        assert_eq!(base32_decode(&secret.to_lowercase()), b"12345678901234567890");
    }

    #[test]
    fn verification_rejects_non_digits_and_other_steps() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 32);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let current = generate(&secret, now);
        // A boundary crossing between generating and verifying would make this flaky.
        if now % 30 < 29 {
            assert!(verify(&current, &secret));
        }
        assert!(!verify(&generate(&secret, now - 60), &secret));
        assert!(!verify("12 345", &secret));
        assert!(!verify("", &secret));
    }

    #[test]
    fn keyuri_matches_otplib() {
        assert_eq!(
            keyuri("ann@x.test", "Inboxora", "ABC"),
            "otpauth://totp/Inboxora:ann%40x.test?secret=ABC&period=30&digits=6&algorithm=SHA1&issuer=Inboxora"
        );
        assert!(qr_data_url("otpauth://totp/x").unwrap().starts_with("data:image/png;base64,iVBORw0KGgo"));
    }
}
