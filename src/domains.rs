//! Sending domains: add (provisioned in Stalwart), work out the DNS records,
//! publish them (by hand or through Cloudflare) and verify them.

use crate::AppState;
use crate::cloudflare::{self, Change, Cloudflare};
use crate::config::Config;
use crate::dns::{dkim_matches, has_prefix};
use crate::secretbox;
use crate::stalwart::DnsRecord;
use crate::util::now;
use anyhow::{Context, Result, bail};
use sqlx::FromRow;

#[derive(Clone, Debug, FromRow)]
pub struct Domain {
    pub id: i64,
    pub name: String,
    pub spf_ok: bool,
    pub dkim_ok: bool,
    pub dmarc_ok: bool,
    pub dns_records: String,
    pub stalwart_synced: bool,
    pub last_error: Option<String>,
    pub checked_at: Option<i64>,
    pub created_at: i64,
    pub dns_provider: String,
    pub cf_api_token: Option<String>,
    pub cf_zone_id: Option<String>,
    pub cf_zone_name: Option<String>,
    pub cf_synced_at: Option<i64>,
    pub cf_last_changes: String,
}

impl Domain {
    /// Ready to send from: SPF and DKIM must pass. DMARC is recommended.
    pub fn verified(&self) -> bool {
        self.spf_ok && self.dkim_ok
    }

    pub fn records(&self) -> Vec<DnsRecord> {
        serde_json::from_str(&self.dns_records).unwrap_or_default()
    }

    pub fn last_changes(&self) -> Vec<Change> {
        serde_json::from_str(&self.cf_last_changes).unwrap_or_default()
    }

    pub fn uses_cloudflare(&self) -> bool {
        self.dns_provider == "cloudflare" && self.cf_zone_id.is_some()
    }
}

pub fn normalize(name: &str) -> Result<String> {
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    let valid_label = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if name.len() > 253 || !name.contains('.') || !name.split('.').all(valid_label) {
        bail!("Nama domain tidak valid");
    }
    Ok(name)
}

/// The SPF mechanism that authorises the Stalwart server to send for a domain.
pub fn spf_mechanism(cfg: &Config) -> String {
    match cfg.mail_hostname.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => format!("ip4:{ip}"),
        Ok(std::net::IpAddr::V6(ip)) => format!("ip6:{ip}"),
        Err(_) => format!("a:{}", cfg.mail_hostname),
    }
}

fn bare(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// The records a domain needs. SPF and DMARC are BatchMail's own (Stalwart's
/// suggested SPF uses `mx`, which is wrong for a domain whose inbound mail is
/// elsewhere); DKIM comes from Stalwart; the rest of Stalwart's records are only
/// needed to *receive* mail through Stalwart.
pub fn desired_records(
    cfg: &Config,
    domain: &str,
    from_stalwart: Option<&[DnsRecord]>,
    dkim_selector: Option<&str>,
) -> Vec<DnsRecord> {
    let mut out = vec![DnsRecord {
        kind: "TXT".into(),
        name: format!("{domain}."),
        value: format!("v=spf1 {} ~all", spf_mechanism(cfg)),
        purpose: "spf".into(),
        published: None,
    }];
    let mut dkim = Vec::new();
    let mut receive = Vec::new();
    for r in from_stalwart.unwrap_or_default() {
        let name = bare(&r.name);
        let is_txt = r.kind.eq_ignore_ascii_case("TXT");
        let value = r.value.to_ascii_lowercase();
        if is_txt && name.ends_with(&format!("._domainkey.{domain}")) {
            dkim.push(DnsRecord {
                purpose: "dkim".into(),
                published: None,
                ..r.clone()
            });
        } else if is_txt
            && (name == domain && value.starts_with("v=spf1") || name == format!("_dmarc.{domain}"))
        {
            // Replaced by our own SPF / DMARC.
        } else {
            receive.push(DnsRecord {
                purpose: "receive".into(),
                published: None,
                ..r.clone()
            });
        }
    }
    if dkim.is_empty() {
        let selector = dkim_selector
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("stalwart");
        dkim.push(DnsRecord {
            kind: "TXT".into(),
            name: format!("{selector}._domainkey.{domain}."),
            value: "v=DKIM1; k=rsa; p=… (salin kunci publik dari Stalwart)".into(),
            purpose: "dkim".into(),
            published: None,
        });
    }
    out.extend(dkim);
    out.push(DnsRecord {
        kind: "TXT".into(),
        name: format!("_dmarc.{domain}."),
        value: format!("v=DMARC1; p=none; rua=mailto:postmaster@{domain}"),
        purpose: "dmarc".into(),
        published: None,
    });
    out.extend(receive);
    out
}

pub async fn list(state: &AppState) -> Result<Vec<Domain>> {
    Ok(sqlx::query_as("SELECT * FROM domains ORDER BY name")
        .fetch_all(&state.db)
        .await?)
}

pub async fn get(state: &AppState, id: i64) -> Result<Option<Domain>> {
    Ok(sqlx::query_as("SELECT * FROM domains WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await?)
}

pub async fn find_by_name(state: &AppState, name: &str) -> Result<Option<Domain>> {
    Ok(sqlx::query_as("SELECT * FROM domains WHERE name = ?")
        .bind(name)
        .fetch_optional(&state.db)
        .await?)
}

/// Adds a domain. With the Stalwart API configured this also creates the domain
/// and its DKIM key in Stalwart.
pub async fn add(state: &AppState, raw_name: &str, dkim_selector: Option<&str>) -> Result<Domain> {
    let name = normalize(raw_name)?;
    if find_by_name(state, &name).await?.is_some() {
        bail!("Domain {name} sudah terdaftar");
    }
    let id: i64 =
        sqlx::query_scalar("INSERT INTO domains (name, created_at) VALUES (?, ?) RETURNING id")
            .bind(&name)
            .bind(now())
            .fetch_one(&state.db)
            .await?;
    sync_with_stalwart(state, id, dkim_selector).await?;
    let domain = get(state, id).await?.expect("just inserted");
    verify(state, &domain).await
}

/// (Re)builds the record list, asking Stalwart for the DKIM key when possible.
pub async fn sync_with_stalwart(
    state: &AppState,
    id: i64,
    dkim_selector: Option<&str>,
) -> Result<()> {
    let Some(domain) = get(state, id).await? else {
        bail!("Domain tidak ditemukan")
    };
    let (from_stalwart, synced, error) = match &state.stalwart {
        Some(api) => match api.provision_domain(&domain.name).await {
            Ok(records) => (Some(records), true, None),
            Err(e) => {
                tracing::warn!("Stalwart provisioning for {} failed: {e:#}", domain.name);
                (None, false, Some(format!("{e:#}")))
            }
        },
        None => (None, false, None),
    };
    let records = desired_records(
        &state.cfg,
        &domain.name,
        from_stalwart.as_deref(),
        dkim_selector,
    );
    sqlx::query(
        "UPDATE domains SET dns_records = ?, stalwart_synced = ?, last_error = ? WHERE id = ?",
    )
    .bind(serde_json::to_string(&records)?)
    .bind(synced)
    .bind(error)
    .bind(id)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Looks every sending record up in DNS and stores the result per record.
pub async fn verify(state: &AppState, domain: &Domain) -> Result<Domain> {
    let mechanism = spf_mechanism(&state.cfg).to_ascii_lowercase();
    let mut records = domain.records();
    for r in records.iter_mut() {
        let txts = || state.dns.txt(&r.name);
        r.published = match r.purpose.as_str() {
            "spf" => Some(txts().await.iter().any(|t| {
                let t = t.to_ascii_lowercase();
                t.trim().starts_with("v=spf1") && t.split_whitespace().any(|p| p == mechanism)
            })),
            "dkim" => Some(dkim_matches(&r.value, &txts().await)),
            "dmarc" => Some(has_prefix(&txts().await, "v=dmarc1")),
            _ => None,
        };
    }
    let ok = |purpose: &str| {
        records
            .iter()
            .any(|r| r.purpose == purpose && r.published == Some(true))
    };
    sqlx::query(
        "UPDATE domains SET spf_ok = ?, dkim_ok = ?, dmarc_ok = ?, dns_records = ?, checked_at = ? WHERE id = ?",
    )
    .bind(ok("spf"))
    .bind(ok("dkim"))
    .bind(ok("dmarc"))
    .bind(serde_json::to_string(&records)?)
    .bind(now())
    .bind(domain.id)
    .execute(&state.db)
    .await?;
    Ok(get(state, domain.id).await?.expect("exists"))
}

pub async fn remove(state: &AppState, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM domains WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(())
}

// ---------- Cloudflare ----------

fn cf_token(state: &AppState, domain: &Domain) -> Result<String> {
    match &domain.cf_api_token {
        Some(sealed) => secretbox::open(&state.cfg.secret, sealed),
        None => state
            .cfg
            .cloudflare_api_token
            .clone()
            .context("Token Cloudflare belum diisi"),
    }
}

/// Connects a domain to Cloudflare: checks the token can see the zone and stores it encrypted.
/// An empty `token` uses CLOUDFLARE_API_TOKEN from the environment.
pub async fn connect_cloudflare(
    state: &AppState,
    domain: &Domain,
    token: Option<&str>,
) -> Result<()> {
    let token = token.map(str::trim).filter(|t| !t.is_empty());
    let plain = match token {
        Some(t) => t.to_string(),
        None => state
            .cfg
            .cloudflare_api_token
            .clone()
            .context("Isi API token Cloudflare (atau set CLOUDFLARE_API_TOKEN di server)")?,
    };
    let (zone_id, zone_name) = Cloudflare::new(&plain)?.find_zone(&domain.name).await?;
    let sealed = match token {
        Some(t) => Some(secretbox::seal(&state.cfg.secret, t)?),
        None => None,
    };
    sqlx::query(
        "UPDATE domains SET dns_provider = 'cloudflare', cf_api_token = ?, cf_zone_id = ?, cf_zone_name = ? WHERE id = ?",
    )
    .bind(sealed)
    .bind(zone_id)
    .bind(zone_name)
    .bind(domain.id)
    .execute(&state.db)
    .await?;
    Ok(())
}

pub async fn disconnect_cloudflare(state: &AppState, id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE domains SET dns_provider = 'manual', cf_api_token = NULL, cf_zone_id = NULL, cf_zone_name = NULL WHERE id = ?",
    )
    .bind(id)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// What a Cloudflare sync would change, without changing anything.
pub async fn cloudflare_plan(
    state: &AppState,
    domain: &Domain,
    include_receiving: bool,
) -> Result<Vec<Change>> {
    let zone = domain
        .cf_zone_id
        .as_deref()
        .context("Domain belum terhubung ke Cloudflare")?;
    let client = Cloudflare::new(&cf_token(state, domain)?)?;
    client
        .plan(
            zone,
            &domain.records(),
            include_receiving,
            &spf_mechanism(&state.cfg),
        )
        .await
}

/// Applies the plan in Cloudflare, stores what happened, then re-checks DNS.
pub async fn cloudflare_apply(
    state: &AppState,
    domain: &Domain,
    include_receiving: bool,
) -> Result<Vec<Change>> {
    let zone = domain
        .cf_zone_id
        .as_deref()
        .context("Domain belum terhubung ke Cloudflare")?;
    let client = Cloudflare::new(&cf_token(state, domain)?)?;
    let mut changes = client
        .plan(
            zone,
            &domain.records(),
            include_receiving,
            &spf_mechanism(&state.cfg),
        )
        .await?;
    client.apply(zone, &mut changes).await;
    sqlx::query("UPDATE domains SET cf_last_changes = ?, cf_synced_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&changes)?)
        .bind(now())
        .bind(domain.id)
        .execute(&state.db)
        .await?;
    let domain = get(state, domain.id).await?.expect("exists");
    verify(state, &domain).await?;
    Ok(changes)
}

pub fn change_label(c: &Change) -> (&'static str, &'static str) {
    match (c.action, c.ok) {
        (_, Some(false)) => ("Gagal", "orange"),
        (cloudflare::Action::Create, Some(true)) => ("Dibuat", "blue"),
        (cloudflare::Action::Update, Some(true)) => ("Diperbarui", "blue"),
        (cloudflare::Action::Create, None) => ("Akan dibuat", "blue"),
        (cloudflare::Action::Update, None) => ("Akan diperbarui", "purple"),
        (cloudflare::Action::Keep, _) => ("Tetap", "gray"),
        (cloudflare::Action::Skip, _) => ("Dilewati", "white"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_domain_names() {
        assert_eq!(
            normalize(" Klinik.DomainAnda.id. ").unwrap(),
            "klinik.domainanda.id"
        );
        assert!(normalize("localhost").is_err());
        assert!(normalize("bad_domain.id").is_err());
        assert!(normalize("-x.id").is_err());
    }

    #[test]
    fn builds_records_from_stalwart_output() {
        let cfg = Config::for_tests();
        let from_stalwart = vec![
            DnsRecord {
                kind: "MX".into(),
                name: "x.id.".into(),
                value: "10 mail.x.id.".into(),
                ..Default::default()
            },
            DnsRecord {
                kind: "TXT".into(),
                name: "x.id.".into(),
                value: "v=spf1 mx ra=postmaster -all".into(),
                ..Default::default()
            },
            DnsRecord {
                kind: "TXT".into(),
                name: "202410r._domainkey.x.id.".into(),
                value: "v=DKIM1; k=rsa; p=AAA".into(),
                ..Default::default()
            },
            DnsRecord {
                kind: "TXT".into(),
                name: "_dmarc.x.id.".into(),
                value: "v=DMARC1; p=reject".into(),
                ..Default::default()
            },
        ];
        let recs = desired_records(&cfg, "x.id", Some(&from_stalwart), None);
        let purposes: Vec<&str> = recs.iter().map(|r| r.purpose.as_str()).collect();
        assert_eq!(purposes, ["spf", "dkim", "dmarc", "receive"]);
        assert_eq!(recs[0].value, "v=spf1 a:mail.domainanda.id ~all");
        assert_eq!(recs[1].value, "v=DKIM1; k=rsa; p=AAA");
        assert!(recs[2].value.contains("p=none"));

        let mut ip_cfg = Config::for_tests();
        ip_cfg.mail_hostname = "203.0.113.7".into();
        assert_eq!(spf_mechanism(&ip_cfg), "ip4:203.0.113.7");

        let manual = desired_records(&cfg, "x.id", None, Some("sel1"));
        assert_eq!(manual[1].name, "sel1._domainkey.x.id.");
    }
}
