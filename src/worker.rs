//! Background sender: drains queued recipients into Stalwart at each batch's rate.

use crate::AppState;
use crate::batches::Batch;
use crate::mailer::{Outcome, OutgoingEmail};
use crate::template::{self, Escape};
use crate::unsubscribe;
use crate::util::{now, random_alnum};
use anyhow::{Result, anyhow};
use lettre::message::Mailbox;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

const MAX_ATTEMPTS: i64 = 3;
const RETRY_DELAYS: [i64; 3] = [60, 300, 1800];

pub async fn run(state: AppState) {
    let mut credits: HashMap<String, f64> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        if let Err(e) = step(&state, &mut credits).await {
            tracing::warn!("sender paused: {e:#}");
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }
}

pub async fn step(state: &AppState, credits: &mut HashMap<String, f64>) -> Result<()> {
    let t = now();
    sqlx::query("UPDATE batches SET status = 'queued' WHERE status = 'scheduled' AND send_at <= ?")
        .bind(t)
        .execute(&state.db)
        .await?;
    let active: Vec<Batch> = sqlx::query_as(
        "SELECT * FROM batches WHERE status IN ('queued','sending') ORDER BY created_at",
    )
    .fetch_all(&state.db)
    .await?;
    credits.retain(|id, _| active.iter().any(|b| &b.id == id));

    for batch in &active {
        let per_second = batch.rate_per_minute as f64 / 60.0;
        let credit = credits.entry(batch.id.clone()).or_insert(0.0);
        *credit = (*credit + per_second).min(per_second.max(1.0) * 2.0);
        let n = credit.floor() as i64;
        if n == 0 {
            continue;
        }
        let due: Vec<(i64, String, String, i64)> = sqlx::query_as(
            "SELECT id, email, vars, attempts FROM recipients
             WHERE batch_id = ? AND status IN ('queued','deferred') AND next_attempt_at <= ?
             ORDER BY id LIMIT ?",
        )
        .bind(&batch.id)
        .bind(t)
        .bind(n)
        .fetch_all(&state.db)
        .await?;

        if due.is_empty() {
            let remaining: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM recipients WHERE batch_id = ? AND status IN ('queued','deferred')",
            )
            .bind(&batch.id)
            .fetch_one(&state.db)
            .await?;
            if remaining == 0 {
                sqlx::query("UPDATE batches SET status = 'completed', finished_at = ? WHERE id = ? AND status IN ('queued','sending')")
                    .bind(now())
                    .bind(&batch.id)
                    .execute(&state.db)
                    .await?;
            }
            continue;
        }
        if batch.status == "queued" {
            sqlx::query("UPDATE batches SET status = 'sending' WHERE id = ? AND status = 'queued'")
                .bind(&batch.id)
                .execute(&state.db)
                .await?;
        }
        for (rid, email, vars, attempts) in due {
            *credit -= 1.0;
            send_one(state, batch, rid, &email, &vars, attempts).await?;
        }
    }
    Ok(())
}

async fn send_one(
    state: &AppState,
    batch: &Batch,
    rid: i64,
    email: &str,
    vars: &str,
    attempts: i64,
) -> Result<()> {
    let marketing = batch.kind == "marketing";
    if marketing {
        let suppressed: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM suppressions WHERE email = ?)")
                .bind(email)
                .fetch_one(&state.db)
                .await?;
        if suppressed {
            return update(
                state,
                rid,
                "skipped",
                attempts,
                0,
                "Dilewati: penerima sudah berhenti berlangganan",
                None,
            )
            .await;
        }
    }

    let from = Mailbox::from_str(&batch.from_addr).map_err(|e| anyhow!("from tidak valid: {e}"))?;
    let reply_to = batch
        .reply_to
        .as_deref()
        .and_then(|r| Mailbox::from_str(r).ok());
    let domain = batch.from_domain();
    let unsubscribe_url = marketing.then(|| {
        unsubscribe::url(
            &state.cfg.public_base_url,
            &state.cfg.secret,
            email,
            &domain,
        )
    });

    let mut vars: Map<String, Value> = serde_json::from_str(vars).unwrap_or_default();
    vars.insert("email".into(), Value::String(email.to_string()));
    if let Some(url) = &unsubscribe_url {
        vars.insert("unsubscribe_url".into(), Value::String(url.clone()));
    }
    let subject = template::render(&batch.subject, &vars, Escape::None);
    let mut html = batch
        .html
        .as_deref()
        .map(|h| template::render(h, &vars, Escape::Html));
    let mut text = batch
        .text
        .as_deref()
        .map(|t| template::render(t, &vars, Escape::None));
    if let Some(url) = &unsubscribe_url {
        // Every bulk email must have a visible unsubscribe link; add one if the template has none.
        let has_link =
            |t: &Option<String>| t.as_deref().is_some_and(|t| t.contains("unsubscribe_url"));
        if !has_link(&batch.html)
            && let Some(h) = html.as_mut()
        {
            h.push_str(&format!(
                    "<p style=\"font-size:12px;color:#666\"><a href=\"{}\">Berhenti berlangganan</a></p>",
                    html_escape::encode_double_quoted_attribute(url)
                ));
        }
        if !has_link(&batch.text)
            && let Some(t) = text.as_mut()
        {
            t.push_str(&format!("\n\n--\nBerhenti berlangganan: {url}\n"));
        }
    }
    let message_id = format!("<{}.{}.{}@{}>", batch.id, rid, random_alnum(8), domain);

    let outcome = if batch.test_mode {
        Outcome::Accepted("250 Mode test: tidak benar-benar dikirim".into())
    } else {
        state
            .mailer
            .send(&OutgoingEmail {
                from: &from,
                reply_to: reply_to.as_ref(),
                to: email,
                subject: &subject,
                html: html.as_deref(),
                text: text.as_deref(),
                message_id: &message_id,
                unsubscribe_url: unsubscribe_url.as_deref(),
            })
            .await
    };

    match outcome {
        Outcome::Accepted(resp) => {
            update(
                state,
                rid,
                "sent",
                attempts + 1,
                0,
                &resp,
                Some(&message_id),
            )
            .await
        }
        Outcome::Permanent(resp) => {
            update(state, rid, "failed", attempts + 1, 0, &resp, None).await
        }
        Outcome::Transient(resp) => {
            let attempts = attempts + 1;
            if attempts >= MAX_ATTEMPTS {
                update(state, rid, "failed", attempts, 0, &resp, None).await
            } else {
                let delay = RETRY_DELAYS[(attempts - 1) as usize];
                update(state, rid, "deferred", attempts, now() + delay, &resp, None).await
            }
        }
        Outcome::Unavailable(err) => {
            // Nothing was sent; keep the recipient queued and back off.
            sqlx::query("UPDATE recipients SET smtp_response = ?, updated_at = ? WHERE id = ?")
                .bind(format!("Stalwart tidak terjangkau: {err}"))
                .bind(now())
                .bind(rid)
                .execute(&state.db)
                .await?;
            Err(anyhow!("Stalwart tidak terjangkau: {err}"))
        }
    }
}

async fn update(
    state: &AppState,
    rid: i64,
    status: &str,
    attempts: i64,
    next_attempt_at: i64,
    response: &str,
    message_id: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE recipients SET status = ?, attempts = ?, next_attempt_at = ?, smtp_response = ?,
           message_id = COALESCE(?, message_id), updated_at = ? WHERE id = ?",
    )
    .bind(status)
    .bind(attempts)
    .bind(next_attempt_at)
    .bind(response)
    .bind(message_id)
    .bind(now())
    .bind(rid)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Sends one message right away (the dashboard's "send test" button).
pub async fn send_test(
    state: &AppState,
    batch_like: &Batch,
    to: &str,
    vars: Map<String, Value>,
) -> Outcome {
    let from = match Mailbox::from_str(&batch_like.from_addr) {
        Ok(f) => f,
        Err(e) => return Outcome::Permanent(format!("from tidak valid: {e}")),
    };
    let mut vars = vars;
    vars.insert("email".into(), Value::String(to.to_string()));
    vars.insert(
        "unsubscribe_url".into(),
        Value::String(format!("{}/u/contoh", state.cfg.public_base_url)),
    );
    let subject = format!(
        "[TES] {}",
        template::render(&batch_like.subject, &vars, Escape::None)
    );
    let html = batch_like
        .html
        .as_deref()
        .map(|h| template::render(h, &vars, Escape::Html));
    let text = batch_like
        .text
        .as_deref()
        .map(|t| template::render(t, &vars, Escape::None));
    let message_id = format!("<test.{}@{}>", random_alnum(12), batch_like.from_domain());
    state
        .mailer
        .send(&OutgoingEmail {
            from: &from,
            reply_to: None,
            to,
            subject: &subject,
            html: html.as_deref(),
            text: text.as_deref(),
            message_id: &message_id,
            unsubscribe_url: None,
        })
        .await
}
