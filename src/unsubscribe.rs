//! Signed unsubscribe links: `/u/{token}`, where the token carries the recipient
//! email and sender domain plus an HMAC, so no lookup table is needed.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;
const TAG_LEN: usize = 16;

pub struct Target {
    pub email: String,
    pub domain: String,
}

fn mac(secret: &str, payload: &[u8]) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    m.update(b"unsubscribe:");
    m.update(payload);
    m
}

pub fn token(secret: &str, email: &str, domain: &str) -> String {
    let payload = format!(
        "{}\n{}",
        email.to_ascii_lowercase(),
        domain.to_ascii_lowercase()
    );
    let tag = mac(secret, payload.as_bytes()).finalize().into_bytes();
    format!("{}.{}", B64.encode(payload), B64.encode(&tag[..TAG_LEN]))
}

pub fn url(base: &str, secret: &str, email: &str, domain: &str) -> String {
    format!("{base}/u/{}", token(secret, email, domain))
}

pub fn verify(secret: &str, token: &str) -> Option<Target> {
    let (p, t) = token.split_once('.')?;
    let payload = B64.decode(p).ok()?;
    let tag = B64.decode(t).ok()?;
    if tag.len() != TAG_LEN {
        return None;
    }
    mac(secret, &payload).verify_truncated_left(&tag).ok()?;
    let payload = String::from_utf8(payload).ok()?;
    let (email, domain) = payload.split_once('\n')?;
    Some(Target {
        email: email.to_string(),
        domain: domain.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn roundtrip() {
        let t = token(S, "Andi@Contoh.co.id", "domainanda.id");
        let target = verify(S, &t).unwrap();
        assert_eq!(target.email, "andi@contoh.co.id");
        assert_eq!(target.domain, "domainanda.id");
    }

    #[test]
    fn rejects_tampering() {
        let t = token(S, "andi@contoh.co.id", "domainanda.id");
        let forged = token(
            "another-secret-another-secret-xx",
            "andi@contoh.co.id",
            "domainanda.id",
        );
        assert!(verify(S, &forged).is_none());
        let (p, tag) = t.split_once('.').unwrap();
        let other = B64.encode("siti@contoh.com\ndomainanda.id");
        assert!(verify(S, &format!("{other}.{tag}")).is_none());
        assert!(verify(S, p).is_none());
    }
}
