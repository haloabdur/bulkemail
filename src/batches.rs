//! Creating batches (shared by the dashboard and the API) and reading them back.

use crate::AppState;
use crate::auth::ApiKey;
use crate::domains;
use crate::error::ApiError;
use crate::util::{domain_of, now, random_alnum};
use lettre::message::Mailbox;
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::{FromRow, QueryBuilder, Sqlite};
use std::collections::HashSet;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Newsletters, promos: unsubscribe link + header, suppression list applies.
    Marketing,
    /// Receipts, password resets, payslips: always delivered, no unsubscribe link.
    Transactional,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "marketing" => Some(Kind::Marketing),
            "transactional" => Some(Kind::Transactional),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Marketing => "marketing",
            Kind::Transactional => "transactional",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecipientInput {
    pub email: String,
    pub vars: Map<String, Value>,
}

pub struct NewBatch<'a> {
    pub name: String,
    pub source: String,
    pub api_key: Option<&'a ApiKey>,
    pub kind: Kind,
    pub from: String,
    pub reply_to: Option<String>,
    pub subject: String,
    pub html: Option<String>,
    pub text: Option<String>,
    pub send_at: Option<i64>,
    pub rate_per_minute: Option<i64>,
    pub recipients: Vec<RecipientInput>,
    pub max_recipients: usize,
}

#[derive(Debug, Serialize)]
pub struct Rejected {
    pub email: String,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Created {
    pub id: String,
    pub status: String,
    pub accepted: usize,
    pub rejected: Vec<Rejected>,
}

pub fn valid_email(email: &str) -> bool {
    lettre::Address::from_str(email)
        .is_ok_and(|a| a.domain().contains('.') && !a.domain().ends_with('.'))
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub async fn create(state: &AppState, nb: NewBatch<'_>) -> Result<Created, ApiError> {
    let from = Mailbox::from_str(nb.from.trim())
        .map_err(|_| ApiError::bad_request("invalid_from", "Alamat pengirim (from) tidak valid"))?;
    let from_domain = from.email.domain().to_ascii_lowercase();
    let domain = domains::find_by_name(state, &from_domain)
        .await?
        .ok_or_else(|| {
            ApiError::bad_request(
                "unknown_domain",
                format!("Domain {from_domain} belum ditambahkan di BatchMail"),
            )
        })?;
    if !domain.verified() && !state.cfg.allow_unverified_domains {
        return Err(ApiError::bad_request(
            "domain_not_verified",
            format!("SPF/DKIM domain {from_domain} belum terverifikasi"),
        ));
    }
    if let Some(key) = nb.api_key {
        if key.domain_id.is_some_and(|d| d != domain.id) {
            return Err(ApiError::new(
                axum::http::StatusCode::FORBIDDEN,
                "domain_not_allowed",
                format!("API key ini tidak boleh mengirim dari {from_domain}"),
            ));
        }
    }
    let reply_to = match non_empty(nb.reply_to) {
        Some(r) => Some(
            Mailbox::from_str(&r)
                .map_err(|_| ApiError::bad_request("invalid_reply_to", "Reply-To tidak valid"))?,
        ),
        None => None,
    };
    let subject = nb.subject.trim().to_string();
    if subject.is_empty() {
        return Err(ApiError::bad_request(
            "missing_subject",
            "Subjek wajib diisi",
        ));
    }
    let (html, text) = (non_empty(nb.html), non_empty(nb.text));
    if html.is_none() && text.is_none() {
        return Err(ApiError::bad_request(
            "missing_body",
            "Isi html atau text wajib diisi",
        ));
    }
    if nb.recipients.len() > nb.max_recipients {
        return Err(ApiError::bad_request(
            "too_many_recipients",
            format!("Maksimal {} penerima per batch", nb.max_recipients),
        ));
    }
    let rate = nb
        .rate_per_minute
        .unwrap_or(state.cfg.default_rate_per_minute)
        .clamp(1, 10_000);

    // Clean the list: invalid, duplicates, unsubscribed.
    let suppressed: HashSet<String> = if nb.kind == Kind::Marketing {
        sqlx::query_scalar("SELECT email FROM suppressions")
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect()
    } else {
        HashSet::new()
    };
    let mut seen = HashSet::new();
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for r in nb.recipients {
        let email = r.email.trim().to_ascii_lowercase();
        if !valid_email(&email) {
            rejected.push(Rejected {
                email,
                reason: "invalid_email",
            });
        } else if !seen.insert(email.clone()) {
            rejected.push(Rejected {
                email,
                reason: "duplicate",
            });
        } else if suppressed.contains(&email) {
            rejected.push(Rejected {
                email,
                reason: "unsubscribed",
            });
        } else {
            accepted.push(RecipientInput {
                email,
                vars: r.vars,
            });
        }
    }
    if accepted.is_empty() {
        return Err(ApiError::bad_request(
            "no_valid_recipients",
            "Tidak ada penerima yang valid",
        ));
    }

    if let Some(key) = nb.api_key {
        if let Some(limit) = key.daily_limit {
            let used: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(total), 0) FROM batches WHERE api_key_id = ? AND created_at > ?",
            )
            .bind(key.id)
            .bind(now() - 86_400)
            .fetch_one(&state.db)
            .await?;
            if used + accepted.len() as i64 > limit {
                return Err(ApiError::new(
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    "daily_limit",
                    format!(
                        "Batas harian {limit} email untuk key ini terlampaui (terpakai {used})"
                    ),
                ));
            }
        }
    }

    let id = format!("b_{}", random_alnum(10));
    let created = now();
    let send_at = nb.send_at.unwrap_or(created);
    let status = if send_at > created + 30 {
        "scheduled"
    } else {
        "queued"
    };
    let name = match nb.name.trim() {
        "" => subject.chars().take(80).collect(),
        n => n.to_string(),
    };

    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO batches (id, name, source, api_key_id, kind, test_mode, from_addr, reply_to, subject, html,
           text, rate_per_minute, status, total, send_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&nb.source)
    .bind(nb.api_key.map(|k| k.id))
    .bind(nb.kind.as_str())
    .bind(nb.api_key.is_some_and(|k| k.test_mode))
    .bind(from.to_string())
    .bind(reply_to.map(|r| r.to_string()))
    .bind(&subject)
    .bind(&html)
    .bind(&text)
    .bind(rate)
    .bind(status)
    .bind(accepted.len() as i64)
    .bind(send_at)
    .bind(created)
    .execute(&mut *tx)
    .await?;
    for chunk in accepted.chunks(500) {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("INSERT INTO recipients (batch_id, email, vars) ");
        qb.push_values(chunk, |mut b, r| {
            b.push_bind(&id)
                .push_bind(&r.email)
                .push_bind(Value::Object(r.vars.clone()).to_string());
        });
        qb.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;

    Ok(Created {
        id,
        status: status.to_string(),
        accepted: accepted.len(),
        rejected,
    })
}

// ---------- parsing recipient lists ----------

/// CSV with a header row; the `email` column is required, other columns become variables.
pub fn parse_csv(data: &[u8]) -> Result<Vec<RecipientInput>, String> {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    let first_line = data.split(|b| *b == b'\n').next().unwrap_or_default();
    let delimiter = if first_line.iter().filter(|b| **b == b';').count()
        > first_line.iter().filter(|b| **b == b',').count()
    {
        b';'
    } else {
        b','
    };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(data);
    let headers: Vec<String> = reader
        .headers()
        .map_err(|e| format!("Header CSV tidak terbaca: {e}"))?
        .iter()
        .map(|h| h.trim().to_ascii_lowercase().replace(' ', "_"))
        .collect();
    let email_col = headers
        .iter()
        .position(|h| h == "email" || h == "e-mail" || h == "e_mail")
        .ok_or("CSV harus punya kolom \"email\"")?;
    let mut out = Vec::new();
    for row in reader.records() {
        let row = row.map_err(|e| format!("Baris CSV tidak valid: {e}"))?;
        let mut vars = Map::new();
        for (i, h) in headers.iter().enumerate() {
            if i != email_col && !h.is_empty() {
                vars.insert(
                    h.clone(),
                    Value::String(row.get(i).unwrap_or("").to_string()),
                );
            }
        }
        out.push(RecipientInput {
            email: row.get(email_col).unwrap_or("").to_string(),
            vars,
        });
    }
    Ok(out)
}

/// One recipient per line: `email` or `email,nama` (comma, semicolon or tab).
pub fn parse_pasted(text: &str) -> Vec<RecipientInput> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.splitn(2, [',', ';', '\t']);
            let email = parts.next().unwrap_or("").trim().to_string();
            let mut vars = Map::new();
            if let Some(nama) = parts.next().map(str::trim).filter(|s| !s.is_empty()) {
                vars.insert("nama".into(), Value::String(nama.to_string()));
            }
            RecipientInput { email, vars }
        })
        .collect()
}

// ---------- reading ----------

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Batch {
    pub id: String,
    pub name: String,
    pub source: String,
    pub api_key_id: Option<i64>,
    pub kind: String,
    pub test_mode: bool,
    pub from_addr: String,
    pub reply_to: Option<String>,
    pub subject: String,
    pub html: Option<String>,
    pub text: Option<String>,
    pub rate_per_minute: i64,
    pub status: String,
    pub total: i64,
    pub send_at: i64,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

impl Batch {
    pub fn from_domain(&self) -> String {
        Mailbox::from_str(&self.from_addr)
            .map(|m| m.email.domain().to_ascii_lowercase())
            .unwrap_or_else(|_| domain_of(&self.from_addr).unwrap_or_default())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Counts {
    pub queued: i64,
    pub sent: i64,
    pub deferred: i64,
    pub failed: i64,
    pub skipped: i64,
    pub cancelled: i64,
}

impl Counts {
    pub fn processed(&self) -> i64 {
        self.sent + self.failed + self.skipped + self.cancelled
    }
}

pub async fn get(db: &sqlx::SqlitePool, id: &str) -> sqlx::Result<Option<Batch>> {
    sqlx::query_as("SELECT * FROM batches WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn counts(db: &sqlx::SqlitePool, id: &str) -> sqlx::Result<Counts> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, COUNT(*) FROM recipients WHERE batch_id = ? GROUP BY status",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    let mut c = Counts::default();
    for (status, n) in rows {
        match status.as_str() {
            "queued" => c.queued = n,
            "sent" => c.sent = n,
            "deferred" => c.deferred = n,
            "failed" => c.failed = n,
            "skipped" => c.skipped = n,
            "cancelled" => c.cancelled = n,
            _ => {}
        }
    }
    Ok(c)
}

#[derive(Clone, Debug, FromRow, Serialize)]
pub struct Recipient {
    pub id: i64,
    pub email: String,
    pub status: String,
    pub attempts: i64,
    pub smtp_response: Option<String>,
    pub message_id: Option<String>,
    pub updated_at: Option<i64>,
}

pub async fn recipients(
    db: &sqlx::SqlitePool,
    batch_id: &str,
    status: Option<&str>,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> sqlx::Result<Vec<Recipient>> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT id, email, status, attempts, smtp_response, message_id, updated_at FROM recipients WHERE batch_id = ",
    );
    qb.push_bind(batch_id);
    if let Some(s) = status {
        qb.push(" AND status = ").push_bind(s);
    }
    if let Some(q) = search.filter(|q| !q.is_empty()) {
        qb.push(" AND email LIKE ")
            .push_bind(format!("%{}%", q.to_ascii_lowercase()));
    }
    qb.push(" ORDER BY id LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    qb.build_query_as().fetch_all(db).await
}

/// Pause, resume, cancel, or retry failed recipients.
pub async fn set_paused(db: &sqlx::SqlitePool, id: &str, paused: bool) -> sqlx::Result<()> {
    let (from, to) = if paused {
        ("('queued','sending','scheduled')", "paused")
    } else {
        ("('paused')", "queued")
    };
    sqlx::query(&format!(
        "UPDATE batches SET status = ? WHERE id = ? AND status IN {from}"
    ))
    .bind(to)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn cancel(db: &sqlx::SqlitePool, id: &str) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let done = sqlx::query(
        "UPDATE batches SET status = 'cancelled', finished_at = ? WHERE id = ?
         AND status IN ('queued','sending','scheduled','paused')",
    )
    .bind(now())
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if done {
        sqlx::query("UPDATE recipients SET status = 'cancelled', updated_at = ? WHERE batch_id = ? AND status IN ('queued','deferred')")
            .bind(now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(done)
}

pub async fn retry_failed(db: &sqlx::SqlitePool, id: &str) -> sqlx::Result<u64> {
    let mut tx = db.begin().await?;
    let n = sqlx::query(
        "UPDATE recipients SET status = 'queued', attempts = 0, next_attempt_at = 0 WHERE batch_id = ? AND status = 'failed'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n > 0 {
        sqlx::query("UPDATE batches SET status = 'queued', finished_at = NULL WHERE id = ? AND status <> 'paused'")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_emails() {
        assert!(valid_email("andi@contoh.co.id"));
        assert!(!valid_email("budi@perusahaan"));
        assert!(!valid_email("bukan-email"));
        assert!(!valid_email(""));
    }

    #[test]
    fn parses_csv_with_variables() {
        let csv = "\u{feff}Email,Nama,Kode Promo\nandi@contoh.co.id,Andi,OKT-A12\nsiti@contoh.com,Siti,OKT-B07\n";
        let rows = parse_csv(csv.as_bytes()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].email, "andi@contoh.co.id");
        assert_eq!(rows[0].vars["nama"], "Andi");
        assert_eq!(rows[1].vars["kode_promo"], "OKT-B07");
    }

    #[test]
    fn parses_semicolon_csv_and_requires_email_column() {
        let rows = parse_csv(b"email;nama\na@b.id;A\n").unwrap();
        assert_eq!(rows[0].vars["nama"], "A");
        assert!(parse_csv(b"nama,kota\nA,B\n").is_err());
    }

    #[test]
    fn parses_pasted_lines() {
        let rows = parse_pasted("andi@contoh.co.id,Andi\n\n siti@contoh.com \nbudi@x.id\tBudi");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].vars["nama"], "Andi");
        assert!(rows[1].vars.is_empty());
        assert_eq!(rows[2].vars["nama"], "Budi");
    }
}
