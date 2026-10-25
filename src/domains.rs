//! Sending domains: add (provisioned in Stalwart), show DNS records, verify.

use crate::AppState;
use crate::stalwart::{self, DnsRecord};
use crate::util::now;
use anyhow::{Result, bail};
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
}

impl Domain {
    /// Ready to send from: SPF and DKIM must pass. DMARC is recommended.
    pub fn verified(&self) -> bool {
        self.spf_ok && self.dkim_ok
    }

    pub fn records(&self) -> Vec<DnsRecord> {
        serde_json::from_str(&self.dns_records).unwrap_or_default()
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
/// and its DKIM key in Stalwart and stores the DNS records to publish.
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

/// (Re)fetches the records from Stalwart, or builds the fallback set.
pub async fn sync_with_stalwart(
    state: &AppState,
    id: i64,
    dkim_selector: Option<&str>,
) -> Result<()> {
    let Some(domain) = get(state, id).await? else {
        bail!("Domain tidak ditemukan")
    };
    let (records, synced, error) = match &state.stalwart {
        Some(api) => match api.provision_domain(&domain.name).await {
            Ok(records) => (records, true, None),
            Err(e) => {
                tracing::warn!("Stalwart provisioning for {} failed: {e:#}", domain.name);
                (
                    fallback(state, &domain.name, dkim_selector),
                    false,
                    Some(format!("{e:#}")),
                )
            }
        },
        None => (fallback(state, &domain.name, dkim_selector), false, None),
    };
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

fn fallback(state: &AppState, domain: &str, dkim_selector: Option<&str>) -> Vec<DnsRecord> {
    let mut records = stalwart::fallback_records(domain, &state.cfg.smtp_host);
    let selector = dkim_selector
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("stalwart");
    records.push(DnsRecord {
        kind: "TXT".into(),
        name: format!("{selector}._domainkey.{domain}."),
        value: "v=DKIM1; k=rsa; p=… (salin kunci publik dari Stalwart)".into(),
    });
    records
}

pub async fn verify(state: &AppState, domain: &Domain) -> Result<Domain> {
    let check = state.dns.check(&domain.name, &domain.records()).await;
    sqlx::query(
        "UPDATE domains SET spf_ok = ?, dkim_ok = ?, dmarc_ok = ?, checked_at = ? WHERE id = ?",
    )
    .bind(check.spf)
    .bind(check.dkim)
    .bind(check.dmarc)
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

#[cfg(test)]
mod tests {
    use super::normalize;

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
}
