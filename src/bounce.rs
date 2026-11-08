//! Bounces and complaints: `POST /hooks/bounce`.
//!
//! Stalwart (or any other system) tells BatchMail that a message it had accepted was
//! rejected later. The payload can be JSON or a raw RFC 3464 DSN email. We do not depend
//! on one vendor's schema: every message BatchMail sends carries a Message-ID of the form
//! `<b_xxxxxxxxxx.<recipient id>.<random>@domain>`, which is stored per recipient. We
//! look for that ID anywhere in the payload, classify the event (hard / soft / complaint)
//! and update the recipient.
//!
//! * hard bounce  → recipient becomes `bounced`; the address is suppressed when the error
//!   says the *address* is bad (enhanced status 5.1.x, or "user unknown"). Policy/spam
//!   blocks (5.7.x), full mailboxes (5.2.x) and the like are recorded but not suppressed,
//!   because they say something about us or a temporary state, not about the address.
//! * complaint    → recipient becomes `complained`, address suppressed.
//! * soft bounce  → logged only.

use crate::AppState;
use crate::error::ApiError;
use crate::util::now;
use crate::web;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use chrono::DateTime;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;

const MAX_BODY: usize = 2 * 1024 * 1024;
const KEEP_EVENTS: i64 = 1000;
const MAX_PAYLOAD_CHARS: usize = 4000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/hooks/bounce", post(receive))
        .route("/hooks/bounce/events", get(recent))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

// ---------- classification ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hard,
    Soft,
    Complaint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub kind: Kind,
    /// Enhanced status code such as `5.1.1`, when the payload has one.
    pub code: Option<String>,
    /// True when the address itself should stop receiving mail.
    pub suppress: bool,
}

/// Enhanced status codes (`X.Y.Z`, X in 2/4/5) found in the text, in order.
fn status_codes(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let starts = matches!(b[i], b'2' | b'4' | b'5')
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.'));
        if starts && b.get(i + 1) == Some(&b'.') {
            let mut j = i + 2;
            let mut parts = 0;
            while parts < 2 {
                let s = j;
                while j < b.len() && b[j].is_ascii_digit() && j - s < 3 {
                    j += 1;
                }
                if j == s {
                    break;
                }
                parts += 1;
                if parts == 1 {
                    if b.get(j) == Some(&b'.') {
                        j += 1;
                    } else {
                        break;
                    }
                }
            }
            // "5.6.7.8" (an IP address) continues with ".<digit>" and is rejected here;
            // a full stop at the end of a sentence is fine.
            let ends = j >= b.len()
                || !(b[j].is_ascii_alphanumeric()
                    || (b[j] == b'.' && b.get(j + 1).is_some_and(u8::is_ascii_digit)));
            if parts == 2 && ends {
                out.push(text[i..j].to_string());
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Value of a DSN field such as `Action: failed`, lowercased, first word only.
fn dsn_field(lower: &str, name: &str) -> Option<String> {
    lower.lines().find_map(|l| {
        let l = l.trim_start();
        l.strip_prefix(name)
            .and_then(|r| r.trim_start().strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next())
            .map(str::to_string)
    })
}

/// Whole words only: "delivery.attempt-start" must not match "temp".
const SOFT_WORDS: [&str; 9] = [
    "soft",
    "temp",
    "temporary",
    "tempfail",
    "delay",
    "delayed",
    "defer",
    "deferred",
    "transient",
];
const HARD_WORDS: [&str; 10] = [
    "hard",
    "perm",
    "permanent",
    "permfail",
    "bounce",
    "bounced",
    "fail",
    "failed",
    "failure",
    "rejected",
];
const BAD_ADDRESS_WORDS: [&str; 7] = [
    "user unknown",
    "unknown user",
    "no such user",
    "unknown recipient",
    "invalid recipient",
    "mailbox not found",
    "does not exist",
];

fn has_word(labels: &str, words: &[&str]) -> bool {
    labels
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| words.contains(&t))
}

/// `text` is everything in the event; `labels` only the event's type/status/name fields.
/// Returns None for events that are not failures (delivered, connecting, …).
pub fn classify(text: &str, labels: &str) -> Option<Verdict> {
    let lower = text.to_ascii_lowercase();
    let labels = labels.to_ascii_lowercase();
    let code = status_codes(&lower)
        .into_iter()
        .find(|c| c.starts_with('4') || c.starts_with('5'));

    let kind = if lower.contains("feedback-type:") || lower.contains("\"complaint\"") {
        Kind::Complaint
    } else if let Some(action) = dsn_field(&lower, "action") {
        match action.as_str() {
            "failed" => Kind::Hard,
            "delayed" => Kind::Soft,
            _ => return None,
        }
    } else if let Some(c) = &code {
        if c.starts_with('5') {
            Kind::Hard
        } else {
            Kind::Soft
        }
    } else if has_word(&labels, &SOFT_WORDS) {
        Kind::Soft
    } else if has_word(&labels, &HARD_WORDS) {
        Kind::Hard
    } else {
        return None;
    };

    let suppress = match kind {
        Kind::Complaint => true,
        Kind::Soft => false,
        Kind::Hard => match &code {
            Some(c) => c.starts_with("5.1."),
            None => BAD_ADDRESS_WORDS.iter().any(|w| lower.contains(w)),
        },
    };
    Some(Verdict {
        kind,
        code,
        suppress,
    })
}

// ---------- finding our Message-IDs ----------

/// Message-IDs generated by BatchMail (`<b_xxxxxxxxxx.123.abcdEFGH@domain>`) found in the text.
pub fn find_message_ids(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'b'
            && b[i + 1] == b'_'
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && let Some((id, end)) = parse_message_id(b, i)
        {
            if !out.contains(&id) {
                out.push(id);
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

fn parse_message_id(b: &[u8], start: usize) -> Option<(String, usize)> {
    let take = |from: usize, pred: fn(&u8) -> bool| {
        let mut j = from;
        while j < b.len() && pred(&b[j]) {
            j += 1;
        }
        j
    };
    let id_end = take(start + 2, u8::is_ascii_alphanumeric);
    if id_end - (start + 2) != 10 || b.get(id_end) != Some(&b'.') {
        return None;
    }
    let rid_end = take(id_end + 1, u8::is_ascii_digit);
    if rid_end == id_end + 1 || b.get(rid_end) != Some(&b'.') {
        return None;
    }
    let rand_end = take(rid_end + 1, u8::is_ascii_alphanumeric);
    if rand_end - (rid_end + 1) != 8 || b.get(rand_end) != Some(&b'@') {
        return None;
    }
    let dom_end = take(rand_end + 1, |c| {
        c.is_ascii_alphanumeric() || *c == b'.' || *c == b'-'
    });
    let mut dom_end = dom_end;
    while dom_end > rand_end + 1 && b[dom_end - 1] == b'.' {
        dom_end -= 1;
    }
    if dom_end == rand_end + 1 {
        return None;
    }
    let s = std::str::from_utf8(&b[start..dom_end]).ok()?;
    Some((format!("<{s}>"), dom_end))
}

// ---------- events ----------

struct Event {
    /// Everything in the event, as text.
    text: String,
    /// Only the type/status/name values, used for keyword matching.
    labels: String,
    /// Explicit `email` field, used when no Message-ID is present.
    email: Option<String>,
    reason: Option<String>,
}

const LABEL_KEYS: [&str; 7] = ["type", "event", "kind", "status", "id", "name", "category"];

fn event_from_json(v: &Value) -> Event {
    let obj: Option<&Map<String, Value>> = v.as_object();
    let field = |k: &str| {
        obj.and_then(|o| o.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let labels = LABEL_KEYS
        .iter()
        .filter_map(|k| field(k))
        .collect::<Vec<_>>()
        .join(" ");
    let email = field("email").map(|e| e.trim().to_ascii_lowercase());
    let reason = field("reason").or_else(|| field("message"));
    Event {
        // Escaped newlines become real ones so DSN-style lines (`Action: failed`) are found.
        text: v.to_string().replace("\\r\\n", "\n").replace("\\n", "\n"),
        labels,
        email: email.filter(|e| e.contains('@')),
        reason,
    }
}

fn split_events(body: &[u8]) -> Vec<Event> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(o)) if o.get("events").is_some_and(Value::is_array) => o["events"]
            .as_array()
            .map(|a| a.iter().map(event_from_json).collect())
            .unwrap_or_default(),
        Ok(Value::Array(a)) => a.iter().map(event_from_json).collect(),
        Ok(v @ Value::Object(_)) => vec![event_from_json(&v)],
        _ => vec![Event {
            text: String::from_utf8_lossy(body).into_owned(),
            labels: String::new(),
            email: None,
            reason: None,
        }],
    }
}

fn excerpt(s: &str, max: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(max).collect()
}

/// Short human-readable reason: the DSN `Diagnostic-Code`, or the event's `reason`.
fn detail_of(ev: &Event) -> String {
    let diag = ev.text.lines().find_map(|l| {
        let t = l.trim_start();
        t.get(..16)
            .filter(|p| p.eq_ignore_ascii_case("diagnostic-code:"))
            .map(|_| t[16..].trim().to_string())
    });
    excerpt(&diag.or_else(|| ev.reason.clone()).unwrap_or_default(), 300)
}

// ---------- applying ----------

async fn record(
    state: &AppState,
    outcome: &str,
    verdict: Option<&Verdict>,
    ev: &Event,
    message_id: Option<&str>,
    recipient: Option<(i64, &str)>,
    detail: &str,
) -> Result<(), sqlx::Error> {
    let payload: String = ev.text.chars().take(MAX_PAYLOAD_CHARS).collect();
    sqlx::query(
        "INSERT INTO bounce_events (received_at, outcome, kind, code, email, message_id, recipient_id, detail, payload)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(now())
    .bind(outcome)
    .bind(verdict.map(|v| format!("{:?}", v.kind).to_ascii_lowercase()))
    .bind(verdict.and_then(|v| v.code.clone()))
    .bind(recipient.map(|r| r.1.to_string()).or_else(|| ev.email.clone()))
    .bind(message_id)
    .bind(recipient.map(|r| r.0))
    .bind(detail)
    .bind(payload)
    .execute(&state.db)
    .await?;
    sqlx::query("DELETE FROM bounce_events WHERE id <= (SELECT MAX(id) FROM bounce_events) - ?")
        .bind(KEEP_EVENTS)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn find_recipient(
    state: &AppState,
    ids: &[String],
) -> Result<Option<(i64, String, String)>, sqlx::Error> {
    for id in ids {
        let row: Option<(i64, String, String)> =
            sqlx::query_as("SELECT id, email, message_id FROM recipients WHERE message_id = ?")
                .bind(id)
                .fetch_optional(&state.db)
                .await?;
        if row.is_some() {
            return Ok(row);
        }
    }
    Ok(None)
}

/// Handles one event; returns its outcome label.
async fn apply(state: &AppState, ev: &Event, log_all: bool) -> Result<&'static str, sqlx::Error> {
    let Some(verdict) = classify(&ev.text, &ev.labels) else {
        if log_all {
            record(state, "ignored", None, ev, None, None, "").await?;
        }
        return Ok("ignored");
    };
    let detail = detail_of(ev);
    let ids = find_message_ids(&ev.text);
    let found = find_recipient(state, &ids).await?;

    let Some((rid, email, message_id)) = found else {
        // No known Message-ID. Only an explicit `email` field can still suppress an address.
        if let (Some(email), true) = (&ev.email, verdict.suppress) {
            let source = if verdict.kind == Kind::Complaint {
                "complaint"
            } else {
                "bounce"
            };
            web::suppress(
                &state.db,
                email,
                Some(&reason_text(&verdict, &detail)),
                source,
            )
            .await?;
            record(state, "suppressed", Some(&verdict), ev, None, None, &detail).await?;
            return Ok("suppressed");
        }
        record(
            state,
            "unmatched",
            Some(&verdict),
            ev,
            ids.first().map(String::as_str),
            None,
            &detail,
        )
        .await?;
        return Ok("unmatched");
    };

    if verdict.kind == Kind::Soft {
        record(
            state,
            "soft",
            Some(&verdict),
            ev,
            Some(&message_id),
            Some((rid, &email)),
            &detail,
        )
        .await?;
        return Ok("soft");
    }

    let (status, outcome, source) = if verdict.kind == Kind::Complaint {
        ("complained", "complained", "complaint")
    } else {
        ("bounced", "bounced", "bounce")
    };
    let text = reason_text(&verdict, &detail);
    let changed = sqlx::query(
        "UPDATE recipients SET status = ?, smtp_response = ?, updated_at = ? WHERE id = ? AND status = 'sent'",
    )
    .bind(status)
    .bind(&text)
    .bind(now())
    .bind(rid)
    .execute(&state.db)
    .await?
    .rows_affected();
    if verdict.suppress {
        web::suppress(&state.db, &email, Some(&text), source).await?;
    }
    let outcome = if changed == 0 { "duplicate" } else { outcome };
    record(
        state,
        outcome,
        Some(&verdict),
        ev,
        Some(&message_id),
        Some((rid, &email)),
        &detail,
    )
    .await?;
    Ok(outcome)
}

fn reason_text(v: &Verdict, detail: &str) -> String {
    let label = match v.kind {
        Kind::Complaint => "Komplain",
        Kind::Hard => "Bounce",
        Kind::Soft => "Bounce sementara",
    };
    let code = v
        .code
        .as_deref()
        .map(|c| format!(" {c}"))
        .unwrap_or_default();
    if detail.is_empty() {
        format!("{label}{code}")
    } else {
        format!("{label}{code}: {detail}")
    }
}

// ---------- HTTP ----------

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Accepts the token as `Authorization: Bearer`, the password of HTTP Basic auth,
/// `?token=`, or an `X-Signature` header (base64 HMAC-SHA256 of the body keyed with the token).
fn authorized(token: &str, headers: &HeaderMap, query_token: Option<&str>, body: &[u8]) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD;
    if query_token.is_some_and(|q| ct_eq(q.as_bytes(), token.as_bytes())) {
        return true;
    }
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = auth.strip_prefix("Bearer ")
            && ct_eq(t.trim().as_bytes(), token.as_bytes())
        {
            return true;
        }
        if let Some(enc) = auth.strip_prefix("Basic ")
            && let Ok(raw) = b64.decode(enc.trim())
            && let Some(pass) = raw.splitn(2, |c| *c == b':').nth(1)
            && ct_eq(pass, token.as_bytes())
        {
            return true;
        }
    }
    if let Some(sig) = headers.get("x-signature").and_then(|v| v.to_str().ok())
        && let Ok(sig) = b64.decode(sig.trim())
    {
        let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("any key length");
        mac.update(body);
        if mac.verify_slice(&sig).is_ok() {
            return true;
        }
    }
    false
}

fn check(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
    body: &[u8],
) -> Result<(), ApiError> {
    // With no token configured the endpoint does not exist.
    let Some(token) = state.cfg.bounce_webhook_token.as_deref() else {
        return Err(ApiError::not_found());
    };
    if authorized(token, headers, query_token, body) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Token webhook tidak valid",
        ))
    }
}

#[derive(Deserialize)]
struct HookQuery {
    token: Option<String>,
    /// `log=all` also stores events that were not failures (for the first setup).
    log: Option<String>,
    limit: Option<i64>,
}

async fn receive(
    State(state): State<AppState>,
    Query(q): Query<HookQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    check(&state, &headers, q.token.as_deref(), &body)?;
    let events = split_events(&body);
    let log_all = q.log.as_deref() == Some("all");
    let mut results: BTreeMap<&str, i64> = BTreeMap::new();
    for ev in &events {
        *results
            .entry(apply(&state, ev, log_all).await?)
            .or_default() += 1;
    }
    Ok(Json(json!({ "received": events.len(), "results": results })).into_response())
}

async fn recent(
    State(state): State<AppState>,
    Query(q): Query<HookQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    check(&state, &headers, q.token.as_deref(), b"")?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    )> = sqlx::query_as(
        "SELECT id, received_at, outcome, kind, code, email, message_id, detail, payload
             FROM bounce_events ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(
            |(id, at, outcome, kind, code, email, message_id, detail, payload)| {
                json!({
                    "id": id,
                    "received_at": DateTime::from_timestamp(at, 0).map(|d| d.to_rfc3339()),
                    "outcome": outcome, "kind": kind, "code": code, "email": email,
                    "message_id": message_id, "detail": detail,
                    "payload": payload.chars().take(600).collect::<String>(),
                })
            },
        )
        .collect();
    Ok(Json(json!({ "data": data })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSN: &str = "Content-Type: multipart/report; report-type=delivery-status\r\n\r\n\
Final-Recipient: rfc822; budi@contoh.com\r\nAction: failed\r\nStatus: 5.1.1\r\n\
Diagnostic-Code: smtp; 550 5.1.1 User unknown\r\n\r\n\
Message-ID: <b_2Ky4hTq9Lm.42.AbCd1234@domainanda.id>\r\nSubject: Halo\r\n";

    #[test]
    fn finds_our_message_ids_anywhere() {
        let ids = find_message_ids(DSN);
        assert_eq!(ids, vec!["<b_2Ky4hTq9Lm.42.AbCd1234@domainanda.id>"]);
        // inside JSON, without angle brackets, and repeated
        let json = r#"{"message_id":"b_2Ky4hTq9Lm.42.AbCd1234@sub.domainanda.id.","x":"<b_2Ky4hTq9Lm.42.AbCd1234@sub.domainanda.id>"}"#;
        assert_eq!(
            find_message_ids(json),
            vec!["<b_2Ky4hTq9Lm.42.AbCd1234@sub.domainanda.id>"]
        );
        // not ours
        assert!(
            find_message_ids("<abc.12.xyz@mail.example.com> b_short.1.abcdefgh@x.id").is_empty()
        );
        assert!(find_message_ids("xb_2Ky4hTq9Lm.42.AbCd1234@domainanda.id").is_empty());
    }

    #[test]
    fn status_code_extraction_skips_ip_addresses() {
        assert_eq!(status_codes("550 5.1.1 user unknown"), vec!["5.1.1"]);
        assert_eq!(
            status_codes("from 5.6.7.8 to 4.3.2.1"),
            Vec::<String>::new()
        );
        assert_eq!(status_codes("code=4.2.2."), vec!["4.2.2"]);
        assert_eq!(status_codes("version 2.0.0"), vec!["2.0.0"]);
    }

    #[test]
    fn classifies_dsn_and_json() {
        let v = classify(DSN, "").unwrap();
        assert_eq!(v.kind, Kind::Hard);
        assert_eq!(v.code.as_deref(), Some("5.1.1"));
        assert!(v.suppress);

        // 5.7.1 is about us, not about the address
        let v = classify("Action: failed\nStatus: 5.7.1\n", "").unwrap();
        assert_eq!((v.kind, v.suppress), (Kind::Hard, false));
        // mailbox full
        let v = classify(r#"{"code":"5.2.2"}"#, "hard_bounce").unwrap();
        assert_eq!((v.kind, v.suppress), (Kind::Hard, false));
        // temporary
        let v = classify("Action: delayed\nStatus: 4.4.1\n", "").unwrap();
        assert_eq!(v.kind, Kind::Soft);
        let v = classify(r#"{"type":"soft_bounce"}"#, "soft_bounce").unwrap();
        assert_eq!(v.kind, Kind::Soft);
        // no code: keywords decide
        let v = classify(r#"{"reason":"550 User unknown"}"#, "hard_bounce").unwrap();
        assert!(v.suppress);
        // complaint
        let v = classify(r#"{"type":"complaint"}"#, "complaint").unwrap();
        assert_eq!((v.kind, v.suppress), (Kind::Complaint, true));
        // not failures
        assert!(classify("Action: delivered\nStatus: 2.0.0\n", "").is_none());
        assert!(classify(r#"{"type":"delivery.delivered"}"#, "delivery.delivered").is_none());
        assert!(
            classify(
                r#"{"type":"delivery.attempt-start"}"#,
                "delivery.attempt-start"
            )
            .is_none()
        );
        // a policy reason mentioning "complaints" is not a complaint
        let v = classify(
            "Action: failed\nStatus: 5.7.1 blocked, too many complaints",
            "",
        )
        .unwrap();
        assert_eq!(v.kind, Kind::Hard);
    }

    #[test]
    fn auth_methods() {
        let t = "s3cret-token";
        let h = HeaderMap::new();
        assert!(!authorized(t, &h, None, b"{}"));
        assert!(authorized(t, &h, Some(t), b"{}"));
        assert!(!authorized(t, &h, Some("nope"), b"{}"));

        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            "Bearer s3cret-token".parse().unwrap(),
        );
        assert!(authorized(t, &h, None, b"{}"));

        let basic = base64::engine::general_purpose::STANDARD.encode("stalwart:s3cret-token");
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {basic}").parse().unwrap(),
        );
        assert!(authorized(t, &h, None, b"{}"));

        let mut mac = Hmac::<Sha256>::new_from_slice(t.as_bytes()).unwrap();
        mac.update(b"{\"a\":1}");
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let mut h = HeaderMap::new();
        h.insert("x-signature", sig.parse().unwrap());
        assert!(authorized(t, &h, None, b"{\"a\":1}"));
        assert!(!authorized(t, &h, None, b"{\"a\":2}"));
    }
}
