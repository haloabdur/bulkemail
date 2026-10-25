//! Publishes a domain's DNS records through the Cloudflare API.
//!
//! Needs an API token with the "Zone › DNS › Edit" permission for the zone.
//! Changes are planned first and are deliberately conservative: an existing SPF
//! record is extended (never replaced), an existing DMARC record is kept, and
//! records that only matter for receiving mail (MX, …) are touched only when
//! asked for and never overwrite what is there.

use crate::stalwart::DnsRecord;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const API: &str = "https://api.cloudflare.com/client/v4";

#[derive(Clone, Debug, Deserialize)]
pub struct CfRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub priority: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Create,
    Update,
    Keep,
    Skip,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    pub action: Action,
    pub kind: String,
    pub name: String,
    pub content: String,
    pub priority: Option<u16>,
    #[serde(skip)]
    pub record_id: Option<String>,
    pub note: String,
    /// Set after applying: whether the API call succeeded.
    #[serde(default)]
    pub ok: Option<bool>,
}

pub struct Cloudflare {
    http: reqwest::Client,
    token: String,
}

fn fqdn(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Cloudflare may return TXT content quoted and split (`"a" "b"`); compare it as one string.
pub fn normalize_txt(content: &str) -> String {
    let c = content.trim();
    if c.starts_with('"') && c.ends_with('"') && c.len() >= 2 {
        c[1..c.len() - 1]
            .split("\" \"")
            .collect::<Vec<_>>()
            .join("")
    } else {
        c.to_string()
    }
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Adds `mechanism` to an existing SPF record, before its final `all`/`redirect`.
pub fn merge_spf(existing: &str, mechanism: &str) -> String {
    let mut parts: Vec<String> = normalize_txt(existing)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if parts.iter().any(|p| p.eq_ignore_ascii_case(mechanism)) {
        return parts.join(" ");
    }
    let is_tail = |p: &str| {
        let p = p.to_ascii_lowercase();
        p.trim_start_matches(['+', '-', '~', '?']) == "all" || p.starts_with("redirect=")
    };
    let at = parts.iter().position(|p| is_tail(p)).unwrap_or(parts.len());
    parts.insert(at.max(1), mechanism.to_string());
    parts.join(" ")
}

fn split_mx(value: &str) -> (Option<u16>, String) {
    let mut it = value.split_whitespace();
    match (it.next(), it.next()) {
        (Some(p), Some(host)) => (p.parse().ok(), fqdn(host)),
        _ => (None, fqdn(value)),
    }
}

/// Works out what to change. `existing` holds the zone's records at the names involved.
pub fn plan(
    desired: &[DnsRecord],
    existing: &[CfRecord],
    include_receiving: bool,
    spf_mechanism: &str,
) -> Vec<Change> {
    let mut out = Vec::new();
    for rec in desired {
        let name = fqdn(&rec.name);
        let kind = rec.kind.to_ascii_uppercase();
        let at_name: Vec<&CfRecord> = existing.iter().filter(|e| fqdn(&e.name) == name).collect();
        let txts: Vec<(&CfRecord, String)> = at_name
            .iter()
            .filter(|e| e.kind == "TXT")
            .map(|e| (*e, normalize_txt(&e.content)))
            .collect();
        let change =
            |action, content: String, priority, record_id: Option<String>, note: &str| Change {
                action,
                kind: kind.clone(),
                name: name.clone(),
                content,
                priority,
                record_id,
                note: note.to_string(),
                ok: None,
            };
        match rec.purpose.as_str() {
            "spf" => match txts
                .iter()
                .find(|(_, c)| c.to_ascii_lowercase().starts_with("v=spf1"))
            {
                Some((e, current)) => {
                    let merged = merge_spf(current, spf_mechanism);
                    if squash(&merged) == squash(current) {
                        out.push(change(
                            Action::Keep,
                            current.clone(),
                            None,
                            None,
                            "SPF sudah mengizinkan Stalwart",
                        ));
                    } else {
                        out.push(change(
                            Action::Update,
                            merged,
                            None,
                            Some(e.id.clone()),
                            "Menambahkan Stalwart ke SPF yang sudah ada",
                        ));
                    }
                }
                None => out.push(change(
                    Action::Create,
                    rec.value.clone(),
                    None,
                    None,
                    "SPF baru",
                )),
            },
            "dmarc" => match txts
                .iter()
                .find(|(_, c)| c.to_ascii_lowercase().starts_with("v=dmarc1"))
            {
                Some((_, current)) => out.push(change(
                    Action::Keep,
                    current.clone(),
                    None,
                    None,
                    "DMARC sudah ada, tidak diubah",
                )),
                None => out.push(change(
                    Action::Create,
                    rec.value.clone(),
                    None,
                    None,
                    "DMARC baru (mode pantau p=none)",
                )),
            },
            "dkim" => {
                if rec.value.contains('…') {
                    out.push(change(
                        Action::Skip,
                        rec.value.clone(),
                        None,
                        None,
                        "Kunci DKIM belum diketahui — salin dari Stalwart",
                    ));
                    continue;
                }
                match txts.first() {
                    Some((_, current)) if squash(current) == squash(&rec.value) => {
                        out.push(change(
                            Action::Keep,
                            rec.value.clone(),
                            None,
                            None,
                            "Kunci DKIM sudah sama",
                        ))
                    }
                    Some((e, _)) => out.push(change(
                        Action::Update,
                        rec.value.clone(),
                        None,
                        Some(e.id.clone()),
                        "Memperbarui kunci DKIM",
                    )),
                    None => out.push(change(
                        Action::Create,
                        rec.value.clone(),
                        None,
                        None,
                        "Kunci DKIM dari Stalwart",
                    )),
                }
            }
            _ => {
                let (priority, content) = if kind == "MX" {
                    split_mx(&rec.value)
                } else {
                    (None, rec.value.clone())
                };
                if !include_receiving {
                    out.push(change(
                        Action::Skip,
                        rec.value.clone(),
                        priority,
                        None,
                        "Hanya untuk menerima email lewat Stalwart",
                    ));
                    continue;
                }
                if !matches!(kind.as_str(), "MX" | "TXT" | "CNAME") {
                    out.push(change(
                        Action::Skip,
                        rec.value.clone(),
                        priority,
                        None,
                        "Tipe ini dipasang manual",
                    ));
                    continue;
                }
                let same_kind: Vec<&&CfRecord> =
                    at_name.iter().filter(|e| e.kind == kind).collect();
                let wanted = if kind == "TXT" {
                    squash(&content)
                } else {
                    fqdn(&content)
                };
                let matches = same_kind.iter().any(|e| {
                    let got = if kind == "TXT" {
                        squash(&normalize_txt(&e.content))
                    } else {
                        fqdn(&e.content)
                    };
                    got == wanted
                });
                if matches {
                    out.push(change(
                        Action::Keep,
                        content,
                        priority,
                        None,
                        "Sudah terpasang",
                    ));
                } else if kind != "TXT" && !same_kind.is_empty() {
                    out.push(change(
                        Action::Skip,
                        content,
                        priority,
                        None,
                        "Sudah ada record lain — tidak ditimpa",
                    ));
                } else if kind == "CNAME" && !at_name.is_empty() {
                    out.push(change(
                        Action::Skip,
                        content,
                        priority,
                        None,
                        "Nama ini sudah dipakai record lain",
                    ));
                } else {
                    out.push(change(
                        Action::Create,
                        content,
                        priority,
                        None,
                        "Record penerimaan email",
                    ));
                }
            }
        }
    }
    out
}

impl Cloudflare {
    pub fn new(token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Cloudflare {
            http,
            token: token.trim().to_string(),
        })
    }

    async fn call(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Cloudflare tidak bisa dihubungi")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if body.get("success").and_then(Value::as_bool) != Some(true) {
            let msg = body
                .get("errors")
                .and_then(Value::as_array)
                .map(|errs| {
                    errs.iter()
                        .filter_map(|e| e.get("message").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| format!("HTTP {status}"));
            bail!("Cloudflare: {msg}");
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Finds the zone holding `domain` (which may be a subdomain of the zone).
    pub async fn find_zone(&self, domain: &str) -> Result<(String, String)> {
        let labels: Vec<&str> = domain.split('.').collect();
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels[i..].join(".");
            let result = self
                .call(
                    self.http
                        .get(format!("{API}/zones"))
                        .query(&[("name", candidate.as_str())]),
                )
                .await?;
            if let Some(zone) = result.as_array().and_then(|z| z.first()) {
                let id = zone
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = zone
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !id.is_empty() {
                    return Ok((id, name));
                }
            }
        }
        bail!(
            "Zona untuk {domain} tidak ditemukan di akun Cloudflare ini (atau token tidak punya akses)"
        )
    }

    pub async fn records_at(&self, zone: &str, names: &[String]) -> Result<Vec<CfRecord>> {
        let mut out = Vec::new();
        for name in names {
            let result = self
                .call(
                    self.http
                        .get(format!("{API}/zones/{zone}/dns_records"))
                        .query(&[("name", name.as_str()), ("per_page", "100")]),
                )
                .await?;
            out.extend(serde_json::from_value::<Vec<CfRecord>>(result).unwrap_or_default());
        }
        Ok(out)
    }

    pub async fn plan(
        &self,
        zone: &str,
        desired: &[DnsRecord],
        include_receiving: bool,
        spf_mechanism: &str,
    ) -> Result<Vec<Change>> {
        let mut names: Vec<String> = desired.iter().map(|r| fqdn(&r.name)).collect();
        names.sort();
        names.dedup();
        let existing = self.records_at(zone, &names).await?;
        Ok(plan(desired, &existing, include_receiving, spf_mechanism))
    }

    pub async fn apply(&self, zone: &str, changes: &mut [Change]) {
        for c in changes.iter_mut() {
            let mut body =
                json!({ "type": c.kind, "name": c.name, "content": c.content, "ttl": 1 });
            if let Some(p) = c.priority {
                body["priority"] = json!(p);
            }
            if c.kind == "CNAME" {
                body["proxied"] = json!(false);
            }
            let result = match (c.action, &c.record_id) {
                (Action::Create, _) => {
                    self.call(
                        self.http
                            .post(format!("{API}/zones/{zone}/dns_records"))
                            .json(&body),
                    )
                    .await
                }
                (Action::Update, Some(id)) => {
                    self.call(
                        self.http
                            .patch(format!("{API}/zones/{zone}/dns_records/{id}"))
                            .json(&json!({ "content": c.content })),
                    )
                    .await
                }
                _ => continue,
            };
            match result {
                Ok(_) => c.ok = Some(true),
                Err(e) => {
                    c.ok = Some(false);
                    c.note = format!("Gagal: {e:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, name: &str, value: &str, purpose: &str) -> DnsRecord {
        DnsRecord {
            kind: kind.into(),
            name: name.into(),
            value: value.into(),
            purpose: purpose.into(),
            published: None,
        }
    }
    fn cf(id: &str, kind: &str, name: &str, content: &str) -> CfRecord {
        CfRecord {
            id: id.into(),
            kind: kind.into(),
            name: name.into(),
            content: content.into(),
            priority: None,
        }
    }

    #[test]
    fn merges_spf_without_breaking_existing_senders() {
        assert_eq!(
            merge_spf("v=spf1 include:_spf.google.com ~all", "a:mail.x.id"),
            "v=spf1 include:_spf.google.com a:mail.x.id ~all"
        );
        assert_eq!(
            merge_spf("\"v=spf1 a:mail.x.id -all\"", "a:mail.x.id"),
            "v=spf1 a:mail.x.id -all"
        );
        assert_eq!(
            merge_spf("v=spf1 mx", "a:mail.x.id"),
            "v=spf1 mx a:mail.x.id"
        );
        assert_eq!(
            merge_spf("v=spf1 redirect=_spf.x.id", "a:m"),
            "v=spf1 a:m redirect=_spf.x.id"
        );
    }

    #[test]
    fn plans_conservative_changes() {
        let desired = vec![
            rec("TXT", "x.id.", "v=spf1 a:mail.x.id ~all", "spf"),
            rec("TXT", "_dmarc.x.id.", "v=DMARC1; p=none", "dmarc"),
            rec(
                "TXT",
                "s1._domainkey.x.id.",
                "v=DKIM1; k=rsa; p=ABC",
                "dkim",
            ),
            rec("MX", "x.id.", "10 mail.x.id.", "receive"),
        ];
        let existing = vec![
            cf(
                "1",
                "TXT",
                "x.id",
                "\"v=spf1 include:_spf.google.com ~all\"",
            ),
            cf("2", "TXT", "_dmarc.x.id", "v=DMARC1; p=reject"),
            cf("3", "MX", "x.id", "aspmx.l.google.com"),
        ];
        let p = plan(&desired, &existing, false, "a:mail.x.id");
        assert_eq!(p[0].action, Action::Update);
        assert_eq!(p[0].record_id.as_deref(), Some("1"));
        assert_eq!(
            p[0].content,
            "v=spf1 include:_spf.google.com a:mail.x.id ~all"
        );
        assert_eq!(p[1].action, Action::Keep);
        assert_eq!(p[2].action, Action::Create);
        assert_eq!(p[2].name, "s1._domainkey.x.id");
        assert_eq!(p[3].action, Action::Skip);

        // Even when asked, an existing MX is never replaced.
        let p = plan(&desired, &existing, true, "a:mail.x.id");
        assert_eq!(p[3].action, Action::Skip);
        let p = plan(&desired, &[], true, "a:mail.x.id");
        assert_eq!(p[3].action, Action::Create);
        assert_eq!(p[3].priority, Some(10));
        assert_eq!(p[3].content, "mail.x.id");
    }
}
