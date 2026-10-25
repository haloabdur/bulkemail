//! Checks that a sending domain's SPF, DKIM and DMARC records are published.

use crate::stalwart::DnsRecord;
use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DomainCheck {
    pub spf: bool,
    pub dkim: bool,
    pub dmarc: bool,
}

pub struct Verifier {
    resolver: TokioAsyncResolver,
}

impl Verifier {
    pub fn new() -> Self {
        let mut opts = ResolverOpts::default();
        // Users click "check again" right after editing DNS; do not serve stale answers.
        opts.cache_size = 0;
        let resolver = match hickory_resolver::system_conf::read_system_conf() {
            Ok((config, _)) => TokioAsyncResolver::tokio(config, opts),
            Err(_) => TokioAsyncResolver::tokio(ResolverConfig::cloudflare(), opts),
        };
        Verifier { resolver }
    }

    async fn txt(&self, name: &str) -> Vec<String> {
        match self.resolver.txt_lookup(name).await {
            Ok(lookup) => lookup
                .iter()
                .map(|txt| {
                    txt.txt_data()
                        .iter()
                        .map(|part| String::from_utf8_lossy(part))
                        .collect::<String>()
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn check(&self, domain: &str, expected: &[DnsRecord]) -> DomainCheck {
        let spf = self.txt(&format!("{domain}.")).await;
        let dmarc = self.txt(&format!("_dmarc.{domain}.")).await;
        let mut dkim = false;
        for rec in expected.iter().filter(|r| is_dkim_record(r)) {
            if dkim_matches(&rec.value, &self.txt(&rec.name).await) {
                dkim = true;
                break;
            }
        }
        DomainCheck {
            spf: has_prefix(&spf, "v=spf1"),
            dkim,
            dmarc: has_prefix(&dmarc, "v=dmarc1"),
        }
    }
}

pub fn is_dkim_record(r: &DnsRecord) -> bool {
    r.kind.eq_ignore_ascii_case("TXT") && r.name.contains("._domainkey.")
}

fn has_prefix(txts: &[String], prefix: &str) -> bool {
    txts.iter()
        .any(|t| t.trim().to_ascii_lowercase().starts_with(prefix))
}

fn public_key(record: &str) -> Option<String> {
    record.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("p=")
            .map(|k| k.chars().filter(|c| !c.is_whitespace()).collect())
    })
}

/// True when a published TXT carries the DKIM key Stalwart expects. When the
/// expected key is unknown (set up by hand in Stalwart), any DKIM key counts.
pub fn dkim_matches(expected: &str, published: &[String]) -> bool {
    let want = public_key(expected).filter(|k| !k.is_empty() && !k.contains('…'));
    published.iter().any(|txt| match (&want, public_key(txt)) {
        (Some(w), Some(got)) => *w == got,
        (None, Some(got)) => !got.is_empty(),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_dkim_key_ignoring_whitespace() {
        let expected = "v=DKIM1; k=rsa; p=MIIBIjANBg kqhkiG9w0";
        assert!(dkim_matches(
            expected,
            &["v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0".into()]
        ));
        assert!(!dkim_matches(expected, &["v=DKIM1; k=rsa; p=OTHER".into()]));
        assert!(!dkim_matches(expected, &[]));
        assert!(dkim_matches(
            "v=DKIM1; p=…",
            &["v=DKIM1; p=ANYTHING".into()]
        ));
    }

    #[test]
    fn detects_spf_and_dmarc() {
        assert!(has_prefix(&["v=spf1 mx ~all".into()], "v=spf1"));
        assert!(has_prefix(&["v=DMARC1; p=none".into()], "v=dmarc1"));
        assert!(!has_prefix(
            &["google-site-verification=x".into()],
            "v=spf1"
        ));
    }
}
