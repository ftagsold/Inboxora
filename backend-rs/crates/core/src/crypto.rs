//! Credential encryption (`enc:v1:<iv_hex>:<tag_hex>:<ciphertext_hex>`, AES-256-GCM, 12-byte IV)
//! and token helpers, byte-compatible with `encryption.ts`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngExt;
use std::sync::OnceLock;

const PREFIX: &str = "enc:v1:";

static KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Install the key (64 hex chars). Called once at startup after validation.
pub fn init_key(hex_key: &str) -> Result<(), String> {
    let bytes = hex::decode(hex_key).map_err(|e| e.to_string())?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| "ENCRYPTION_KEY must be 32 bytes".to_owned())?;
    let _ = KEY.set(arr);
    Ok(())
}

fn key() -> Option<&'static [u8; 32]> {
    KEY.get()
}

/// Encrypt a credential. Empty input is returned unchanged. Fails when no key is configured:
/// callers must never silently store plaintext.
pub fn encrypt(plaintext: &str) -> Result<String, String> {
    if plaintext.is_empty() {
        return Ok(String::new());
    }
    let key = key().ok_or("ENCRYPTION_KEY is not set or invalid — cannot encrypt credential")?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let iv: [u8; 12] = random_bytes();
    let sealed = cipher.encrypt(&Nonce::from(iv), plaintext.as_bytes()).map_err(|e| e.to_string())?;
    // aes-gcm appends the 16-byte tag; Node stores it separately.
    let (ct, tag) = sealed.split_at(sealed.len() - 16);
    Ok(format!("{PREFIX}{}:{}:{}", hex::encode(iv), hex::encode(tag), hex::encode(ct)))
}

/// Decrypt a value produced by [`encrypt`]. Unprefixed values (legacy plaintext) are returned
/// unchanged; a wrong key, tampering or malformed input yields `None` (the "corrupted
/// credential" path), never a panic.
pub fn decrypt(value: &str) -> Option<String> {
    let Some(rest) = value.strip_prefix(PREFIX) else {
        return Some(value.to_owned());
    };
    let Some(key) = key() else {
        tracing::error!("ENCRYPTION_KEY not set — cannot decrypt stored credential");
        return None;
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let [iv_hex, tag_hex, ct_hex] = parts.as_slice() else {
        tracing::error!("Malformed encrypted credential");
        return None;
    };
    let open = || -> Option<String> {
        let iv = hex::decode(iv_hex).ok()?;
        let tag = hex::decode(tag_hex).ok()?;
        let mut ct = hex::decode(ct_hex).ok()?;
        if tag.len() != 16 || iv.is_empty() {
            return None;
        }
        ct.extend_from_slice(&tag);
        let cipher = Aes256Gcm::new_from_slice(key).ok()?;
        let iv: [u8; 12] = iv.try_into().ok()?;
        let plain = cipher.decrypt(&Nonce::from(iv), ct.as_ref()).ok()?;
        String::from_utf8(plain).ok()
    };
    let out = open();
    if out.is_none() {
        tracing::error!("Failed to decrypt stored credential — wrong ENCRYPTION_KEY or tampered data");
    }
    out
}

/// True if a value is already encrypted with our scheme.
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(PREFIX)
}

/// Cryptographically random bytes.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rng().fill(&mut b[..]);
    b
}

/// `crypto.randomBytes(n).toString('hex')`.
pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::rng().fill(&mut b[..]);
    hex::encode(b)
}

/// `crypto.randomBytes(n).toString('base64url')`.
pub fn random_base64url(n: usize) -> String {
    use base64::Engine;
    let mut b = vec![0u8; n];
    rand::rng().fill(&mut b[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// `crypto.randomUUID()`.
pub fn random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data.as_ref()))
}

/// Lowercase hex MD5 (DAV ETags).
pub fn md5_hex(data: impl AsRef<[u8]>) -> String {
    use md5::Digest;
    hex::encode(md5::Md5::digest(data.as_ref()))
}

/// Constant-time equality for secrets.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_node_ciphertext_and_roundtrips() {
        init_key(&"0".repeat(64)).unwrap();
        // Produced by encryption.ts with a zero key and a fixed IV: encrypt('hunter2').
        let node = "enc:v1:000102030405060708090a0b:ed92f9b64a70afe9afe946bea4fd948e:e0be5d26658fbd";
        assert_eq!(decrypt(node).as_deref(), Some("hunter2"));
        let enc = encrypt("hunter2").unwrap();
        assert!(is_encrypted(&enc));
        assert_eq!(decrypt(&enc).as_deref(), Some("hunter2"));
        assert_eq!(decrypt("plain").as_deref(), Some("plain"));
        assert_eq!(decrypt("enc:v1:zz:zz:zz"), None);
        let mut tampered = enc.clone();
        tampered.pop();
        tampered.push(if enc.ends_with('0') { '1' } else { '0' });
        assert_eq!(decrypt(&tampered), None);
    }
}
