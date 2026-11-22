// SPDX-License-Identifier: GPL-3.0-or-later
//! Encryption of secrets stored in the database (trunk passwords): AES-256-GCM,
//! key from PBX_SECRET_KEY_FILE (64 hex chars). Stored as base64(nonce | ciphertext).

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};

fn cipher(key: &[u8]) -> Aes256Gcm {
    Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key))
}

pub fn load_key() -> Result<Vec<u8>> {
    let path = std::env::var("PBX_SECRET_KEY_FILE").context("PBX_SECRET_KEY_FILE missing")?;
    let key = hex::decode(std::fs::read_to_string(&path)?.trim()).context("secret key is not hex")?;
    anyhow::ensure!(key.len() == 32, "secret key must be 32 bytes");
    Ok(key)
}

pub fn encrypt(key: &[u8], plain: &str) -> Result<String> {
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher(key).encrypt(&nonce, plain.as_bytes()).map_err(|e| anyhow!("encrypt: {e}"))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(STANDARD.encode(out))
}

pub fn decrypt(key: &[u8], stored: &str) -> Result<String> {
    let raw = STANDARD.decode(stored)?;
    anyhow::ensure!(raw.len() > 12, "ciphertext too short");
    let (nonce, ct) = raw.split_at(12);
    let plain = cipher(key).decrypt(Nonce::from_slice(nonce), ct).map_err(|e| anyhow!("decrypt: {e}"))?;
    Ok(String::from_utf8(plain)?)
}

/// Constant-time comparison (the length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Checks the `Authorization` header of a call to the internal webhooks: HTTP Basic with user `pbx`
/// and the shared key (the dialplan puts it into the CURL URL: http://pbx:<key>@127.0.0.1:8081/...).
pub fn internal_auth_ok(authorization: Option<&str>, key: &str) -> bool {
    let Some(b64) = authorization.and_then(|h| h.strip_prefix("Basic ")) else { return false };
    let Ok(raw) = STANDARD.decode(b64.trim()) else { return false };
    let Some(pw) = raw.strip_prefix(b"pbx:") else { return false };
    !key.is_empty() && ct_eq(pw, key.as_bytes())
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    #[test]
    fn internal_auth() {
        let h = format!("Basic {}", super::STANDARD.encode("pbx:abc123"));
        assert!(super::internal_auth_ok(Some(&h), "abc123"));
        assert!(!super::internal_auth_ok(Some(&h), "abc124"));
        assert!(!super::internal_auth_ok(Some(&h), "abc1234"));
        assert!(!super::internal_auth_ok(Some(&h), ""));
        assert!(!super::internal_auth_ok(None, "abc123"));
        let other = format!("Basic {}", super::STANDARD.encode("x:abc123"));
        assert!(!super::internal_auth_ok(Some(&other), "abc123"));
        assert!(!super::internal_auth_ok(Some("Bearer abc123"), "abc123"));
    }

    #[test]
    fn roundtrip() {
        let key = [7u8; 32];
        let c = super::encrypt(&key, "geheim").unwrap();
        assert_eq!(super::decrypt(&key, &c).unwrap(), "geheim");
        assert!(super::decrypt(&[8u8; 32], &c).is_err());
    }
}
