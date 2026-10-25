//! Encrypts small secrets (third-party API tokens) at rest with a key derived
//! from APP_SECRET, so a copy of the database alone does not leak them.

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::RngCore;
use sha2::{Digest, Sha256};

fn cipher(secret: &str) -> ChaCha20Poly1305 {
    let key = Sha256::digest(format!("batchmail-secretbox:{secret}").as_bytes());
    ChaCha20Poly1305::new(&key)
}

pub fn seal(secret: &str, plaintext: &str) -> Result<String> {
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ct = cipher(secret)
        .encrypt(&Nonce::from(nonce), plaintext.as_bytes())
        .map_err(|_| anyhow!("enkripsi gagal"))?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(B64.encode(out))
}

pub fn open(secret: &str, sealed: &str) -> Result<String> {
    let raw = B64.decode(sealed)?;
    if raw.len() < 12 {
        return Err(anyhow!("data terenkripsi rusak"));
    }
    let (nonce, ct) = raw.split_at(12);
    let nonce: [u8; 12] = nonce.try_into()?;
    let pt = cipher(secret)
        .decrypt(&Nonce::from(nonce), ct)
        .map_err(|_| anyhow!("tidak bisa membuka token (APP_SECRET berubah?)"))?;
    Ok(String::from_utf8(pt)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_key() {
        let s = seal("secret-one-secret-one-secret-one!", "cf-token-123").unwrap();
        assert_ne!(s, "cf-token-123");
        assert_eq!(
            open("secret-one-secret-one-secret-one!", &s).unwrap(),
            "cf-token-123"
        );
        assert!(open("secret-two-secret-two-secret-two!", &s).is_err());
    }
}
