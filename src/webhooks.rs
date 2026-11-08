//! Status webhooks to your own system.
//!
//! Each API key can have a webhook URL (set in the dashboard). BatchMail POSTs JSON
//! events to it:
//!
//! * `batch.completed`      every recipient of a batch has been processed
//! * `recipient.bounced`    a message Stalwart accepted was rejected later
//! * `recipient.complained` a recipient reported the message as spam
//! * `webhook.test`         the "send test event" button
//!
//! Events go into an outbox table first and are delivered by a background loop with
//! retries, so a restart or an outage at the receiver loses nothing. Delivery is
//! at-least-once: dedupe on `id` (also the `X-BatchMail-Event-Id` header).
//!
//! Every request is signed: `X-BatchMail-Signature: t=<unix>,v1=<hex>` where
//! `v1 = HMAC_SHA256(secret, "<t>.<raw body>")`. Reject requests whose `t` is too old.

use crate::AppState;
use crate::batches;
use crate::secretbox;
use crate::util::{now, random_alnum, random_hex};
use anyhow::{Result, anyhow, bail};
use chrono::DateTime;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::Duration;
use tokio::task::JoinSet;

/// Seconds to wait after the 1st, 2nd, … failed attempt. After the last one the event is `failed`.
const RETRY_DELAYS: [i64; 5] = [60, 300, 1800, 7200, 21600];
const MAX_ATTEMPTS: i64 = RETRY_DELAYS.len() as i64 + 1;
const PER_TICK: i64 = 20;

// ---------- configuration ----------

/// Accepts http(s) URLs only.
pub fn valid_url(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.len() > 500 {
        bail!("URL webhook terlalu panjang");
    }
    let url = reqwest::Url::parse(raw).map_err(|_| {
        anyhow!("URL webhook tidak valid. Contoh: https://sistem-anda.id/hooks/batchmail")
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("URL webhook harus diawali http:// atau https://");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URL webhook tidak boleh memuat username/password");
    }
    Ok(url.to_string())
}

/// Sets or clears the webhook URL of a key. The signing secret is created on first use and kept
/// when the URL is cleared, so turning the webhook back on does not change the secret.
pub async fn set_url(state: &AppState, key_id: i64, url: Option<&str>) -> Result<()> {
    let url = match url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => Some(valid_url(u)?),
        None => None,
    };
    if url.is_some() {
        let has: Option<String> =
            sqlx::query_scalar("SELECT webhook_secret FROM api_keys WHERE id = ?")
                .bind(key_id)
                .fetch_one(&state.db)
                .await?;
        if has.is_none() {
            let secret = format!("whsec_{}", random_hex(24));
            let sealed = secretbox::seal(&state.cfg.secret, &secret)?;
            sqlx::query("UPDATE api_keys SET webhook_secret = ? WHERE id = ?")
                .bind(sealed)
                .bind(key_id)
                .execute(&state.db)
                .await?;
        }
    }
    sqlx::query("UPDATE api_keys SET webhook_url = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(url)
        .bind(key_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn secret(state: &AppState, key_id: i64) -> Result<Option<String>> {
    let sealed: Option<String> =
        sqlx::query_scalar("SELECT webhook_secret FROM api_keys WHERE id = ?")
            .bind(key_id)
            .fetch_optional(&state.db)
            .await?
            .flatten();
    sealed
        .map(|s| secretbox::open(&state.cfg.secret, &s))
        .transpose()
}

// ---------- enqueue ----------

async fn insert(db: &sqlx::SqlitePool, key_id: i64, event: &str, data: Value) -> Result<()> {
    let event_id = format!("evt_{}", random_alnum(20));
    let created = now();
    let payload = json!({
        "id": event_id,
        "event": event,
        "created_at": DateTime::from_timestamp(created, 0).map(|d| d.to_rfc3339()),
        "data": data,
    });
    sqlx::query(
        "INSERT INTO webhook_deliveries (api_key_id, event_id, event, payload, next_attempt_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(key_id)
    .bind(event_id)
    .bind(event)
    .bind(payload.to_string())
    .bind(created)
    .bind(created)
    .execute(db)
    .await?;
    Ok(())
}

/// Queues an event for the key that owns the batch, if that key has a webhook.
/// Never fails the caller: a webhook problem must not stop sending or bounce handling.
pub async fn enqueue_for_batch(db: &sqlx::SqlitePool, batch_id: &str, event: &str, data: Value) {
    let result: Result<()> = async {
        let key: Option<i64> = sqlx::query_scalar(
            "SELECT k.id FROM batches b JOIN api_keys k ON k.id = b.api_key_id
             WHERE b.id = ? AND k.webhook_url IS NOT NULL AND k.revoked_at IS NULL",
        )
        .bind(batch_id)
        .fetch_optional(db)
        .await?;
        if let Some(key_id) = key {
            insert(db, key_id, event, data).await?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("webhook {event} untuk {batch_id} gagal diantrekan: {e:#}");
    }
}

pub async fn batch_completed(db: &sqlx::SqlitePool, batch_id: &str) {
    let result: Result<()> = async {
        let Some(b) = batches::get(db, batch_id).await? else {
            return Ok(());
        };
        let counts = batches::counts(db, batch_id).await?;
        let data = json!({
            "batch_id": b.id,
            "name": b.name,
            "kind": b.kind,
            "test_mode": b.test_mode,
            "total": b.total,
            "counts": counts,
            "finished_at": b.finished_at.and_then(|t| DateTime::from_timestamp(t, 0)).map(|d| d.to_rfc3339()),
        });
        enqueue_for_batch(db, batch_id, "batch.completed", data).await;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("webhook batch.completed untuk {batch_id} gagal diantrekan: {e:#}");
    }
}

pub async fn enqueue_test(state: &AppState, key_id: i64) -> Result<()> {
    insert(
        &state.db,
        key_id,
        "webhook.test",
        json!({ "message": "Webhook BatchMail terhubung." }),
    )
    .await
}

// ---------- delivery ----------

pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        // A redirect would let a receiver bounce us to somewhere it was not configured to.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("BatchMail-Webhook/1")
        .build()?)
}

type Due = (
    i64,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
);

async fn post(
    client: &reqwest::Client,
    url: &str,
    secret: &str,
    event: &str,
    event_id: &str,
    body: &str,
) -> Result<(), String> {
    let t = now();
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-BatchMail-Event", event)
        .header("X-BatchMail-Event-Id", event_id)
        .header(
            "X-BatchMail-Signature",
            format!("t={t},v1={}", sign(secret, t, body)),
        )
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("tidak terhubung: {}", e.without_url()))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {status}"))
    }
}

/// Sends the events that are due. Returns how many were attempted.
pub async fn deliver_due(state: &AppState, client: &reqwest::Client) -> Result<usize> {
    let due: Vec<Due> = sqlx::query_as(
        "SELECT d.id, d.event_id, d.event, d.payload, d.attempts, k.webhook_url, k.webhook_secret
         FROM webhook_deliveries d JOIN api_keys k ON k.id = d.api_key_id
         WHERE d.status = 'pending' AND d.next_attempt_at <= ?
         ORDER BY d.id LIMIT ?",
    )
    .bind(now())
    .bind(PER_TICK)
    .fetch_all(&state.db)
    .await?;
    let count = due.len();

    let mut jobs = JoinSet::new();
    for (id, event_id, event, payload, attempts, url, sealed) in due {
        let (Some(url), Some(sealed)) = (url, sealed) else {
            finish(
                state,
                id,
                "dropped",
                attempts,
                0,
                Some("Webhook dinonaktifkan"),
            )
            .await?;
            continue;
        };
        let secret = match secretbox::open(&state.cfg.secret, &sealed) {
            Ok(s) => s,
            Err(e) => {
                finish(state, id, "failed", attempts, 0, Some(&e.to_string())).await?;
                continue;
            }
        };
        let client = client.clone();
        jobs.spawn(async move {
            let result = post(&client, &url, &secret, &event, &event_id, &payload).await;
            (id, attempts, result)
        });
    }
    while let Some(joined) = jobs.join_next().await {
        let Ok((id, attempts, result)) = joined else {
            continue;
        };
        let attempts = attempts + 1;
        match result {
            Ok(()) => finish(state, id, "delivered", attempts, 0, None).await?,
            Err(e) if attempts >= MAX_ATTEMPTS => {
                finish(state, id, "failed", attempts, 0, Some(&e)).await?
            }
            Err(e) => {
                let delay = RETRY_DELAYS[(attempts - 1) as usize];
                finish(state, id, "pending", attempts, now() + delay, Some(&e)).await?
            }
        }
    }
    Ok(count)
}

async fn finish(
    state: &AppState,
    id: i64,
    status: &str,
    attempts: i64,
    next_attempt_at: i64,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries SET status = ?, attempts = ?, next_attempt_at = ?,
           last_error = ?, delivered_at = CASE WHEN ? = 'delivered' THEN ? ELSE NULL END
         WHERE id = ?",
    )
    .bind(status)
    .bind(attempts)
    .bind(next_attempt_at)
    .bind(error)
    .bind(status)
    .bind(now())
    .bind(id)
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn prune(state: &AppState) -> Result<()> {
    sqlx::query(
        "DELETE FROM webhook_deliveries
         WHERE (status IN ('delivered','dropped') AND created_at < ?) OR created_at < ?",
    )
    .bind(now() - 7 * 86_400)
    .bind(now() - 30 * 86_400)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Background loop, started next to the mail sender.
pub async fn run(state: AppState) {
    let client = match client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("webhook tidak aktif: {e:#}");
            return;
        }
    };
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        if let Err(e) = deliver_due(&state, &client).await {
            tracing::warn!("pengiriman webhook terhenti sebentar: {e:#}");
        }
        n += 1;
        if n % 720 == 1
            && let Err(e) = prune(&state).await
        {
            tracing::warn!("membersihkan webhook lama gagal: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_depends_on_secret_time_and_body() {
        let a = sign("whsec_x", 1000, "{}");
        assert_eq!(a, sign("whsec_x", 1000, "{}"));
        assert_eq!(a.len(), 64);
        assert_ne!(a, sign("whsec_y", 1000, "{}"));
        assert_ne!(a, sign("whsec_x", 1001, "{}"));
        assert_ne!(a, sign("whsec_x", 1000, "{ }"));
    }

    #[test]
    fn validates_urls() {
        assert!(valid_url("https://sistem.id/hooks/bm").is_ok());
        assert!(valid_url(" http://localhost:3000/x ").is_ok());
        assert!(valid_url("ftp://sistem.id/x").is_err());
        assert!(valid_url("sistem.id/hooks").is_err());
        assert!(valid_url("https://user:pass@sistem.id/x").is_err());
        assert!(valid_url("javascript:alert(1)").is_err());
    }
}
