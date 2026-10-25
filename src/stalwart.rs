//! Client for Stalwart's management API (the same API its web admin uses).
//!
//! Used to make adding a sending domain a one-click step: the domain is created
//! in Stalwart, a DKIM key is generated there, and the DNS records Stalwart wants
//! published are fetched and shown to the user.
//!
//! Endpoints used (check them against your Stalwart version if a call fails):
//!   POST /api/principal          {"type":"domain","name":...}
//!   POST /api/dkim               {"id":null,"algorithm":...,"domain":...,"selector":null}
//!   GET  /api/dns/records/{name}
//!   GET  /api/queue/messages?limit=1

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(alias = "content")]
    pub value: String,
}

pub struct Stalwart {
    http: reqwest::Client,
    base: String,
    auth: String,
}

impl Stalwart {
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let auth = if token.contains(':') {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(token)
            )
        } else {
            format!("Bearer {token}")
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Stalwart {
            http,
            base: base.trim_end_matches('/').to_string(),
            auth,
        })
    }

    async fn call(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req
            .header("Authorization", &self.auth)
            .send()
            .await
            .context("Stalwart tidak bisa dihubungi")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if let Some(err) = body.get("error") {
            bail!("Stalwart: {}", describe_error(err, &body));
        }
        if !status.is_success() {
            bail!("Stalwart membalas HTTP {status}");
        }
        Ok(body.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Creates the domain in Stalwart; a domain that already exists is fine.
    pub async fn ensure_domain(&self, domain: &str) -> Result<()> {
        let req = self
            .http
            .post(format!("{}/principal", self.base))
            .json(&json!({ "type": "domain", "name": domain, "description": "Ditambahkan oleh BatchMail" }));
        match self.call(req).await {
            Ok(_) => Ok(()),
            Err(e) if is_already_exists(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Generates a DKIM key for the domain in Stalwart (which then signs outgoing mail).
    pub async fn ensure_dkim(&self, domain: &str) -> Result<()> {
        for algorithm in ["Rsa", "Ed25519"] {
            let req = self.http.post(format!("{}/dkim", self.base)).json(&json!({
                "id": null, "algorithm": algorithm, "domain": domain, "selector": null
            }));
            match self.call(req).await {
                Ok(_) => {}
                Err(e) if is_already_exists(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The DNS records Stalwart recommends for the domain (MX, SPF, DKIM, DMARC, …).
    pub async fn dns_records(&self, domain: &str) -> Result<Vec<DnsRecord>> {
        let data = self
            .call(self.http.get(format!("{}/dns/records/{domain}", self.base)))
            .await?;
        serde_json::from_value(data).context("format record DNS dari Stalwart tidak dikenali")
    }

    pub async fn queue_size(&self) -> Result<u64> {
        let data = self
            .call(
                self.http
                    .get(format!("{}/queue/messages?limit=1", self.base)),
            )
            .await?;
        data.get("total")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("format antrean tidak dikenali"))
    }

    /// Full setup for a new sending domain.
    pub async fn provision_domain(&self, domain: &str) -> Result<Vec<DnsRecord>> {
        self.ensure_domain(domain).await?;
        self.ensure_dkim(domain).await?;
        self.dns_records(domain).await
    }
}

fn describe_error(err: &Value, body: &Value) -> String {
    let code = err
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string());
    match body.get("details").or_else(|| body.get("reason")) {
        Some(d) => format!(
            "{code} ({})",
            d.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| d.to_string())
        ),
        None => code,
    }
}

fn is_already_exists(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("AlreadyExists") || s.contains("alreadyExists") || s.contains("already exists")
}

/// Records to publish when the Stalwart management API is not configured.
/// DKIM then has to be set up in Stalwart by hand (Settings › Signatures).
pub fn fallback_records(domain: &str, smtp_host: &str) -> Vec<DnsRecord> {
    vec![
        DnsRecord {
            kind: "TXT".into(),
            name: format!("{domain}."),
            value: format!("v=spf1 a:{smtp_host} mx ~all"),
        },
        DnsRecord {
            kind: "TXT".into(),
            name: format!("_dmarc.{domain}."),
            value: format!("v=DMARC1; p=none; rua=mailto:postmaster@{domain}"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stalwart_dns_records() {
        let data = json!([
            {"type": "MX", "name": "domainanda.id.", "content": "10 mail.domainanda.id."},
            {"type": "TXT", "name": "202410r._domainkey.domainanda.id.", "content": "v=DKIM1; k=rsa; p=MIIB"}
        ]);
        let recs: Vec<DnsRecord> = serde_json::from_value(data).unwrap();
        assert_eq!(recs[1].kind, "TXT");
        assert!(recs[1].value.starts_with("v=DKIM1"));
    }
}
