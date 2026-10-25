//! JSON API for other systems: `Authorization: Bearer bk_live_…`.

use crate::AppState;
use crate::auth::ApiKey;
use crate::batches::{self, Kind, NewBatch, RecipientInput};
use crate::domains;
use crate::error::ApiError;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/batches", post(create_batch))
        .route("/batches/{id}", get(get_batch).delete(cancel_batch))
        .route("/batches/{id}/recipients", get(list_recipients))
        .route("/emails", post(send_email))
        .route("/domains", get(list_domains))
}

fn body<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload
        .map(|Json(v)| v)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_json", e.body_text()))
}

fn ts(t: i64) -> Value {
    DateTime::from_timestamp(t, 0)
        .map(|d| Value::String(d.to_rfc3339()))
        .unwrap_or(Value::Null)
}

fn parse_send_at(s: Option<String>) -> Result<Option<i64>, ApiError> {
    match s.filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(&s)
            .map(|d| Some(d.timestamp()))
            .map_err(|_| {
                ApiError::bad_request(
                    "invalid_send_at",
                    "send_at harus format RFC 3339, mis. 2026-10-26T08:00:00+07:00",
                )
            }),
    }
}

fn parse_kind(s: Option<&str>, default: Kind) -> Result<Kind, ApiError> {
    match s {
        None => Ok(default),
        Some(k) => Kind::parse(k).ok_or_else(|| {
            ApiError::bad_request("invalid_kind", "kind harus marketing atau transactional")
        }),
    }
}

#[derive(Deserialize)]
struct RecipientReq {
    email: String,
    #[serde(default)]
    vars: Map<String, Value>,
}

#[derive(Deserialize)]
struct BatchReq {
    name: Option<String>,
    from: String,
    reply_to: Option<String>,
    subject: String,
    html: Option<String>,
    text: Option<String>,
    recipients: Vec<RecipientReq>,
    send_at: Option<String>,
    rate_per_minute: Option<i64>,
    kind: Option<String>,
}

fn created_response(created: batches::Created) -> Response {
    let status_url = format!("/api/v1/batches/{}", created.id);
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "id": created.id,
            "status": created.status,
            "accepted": created.accepted,
            "rejected": created.rejected,
            "status_url": status_url,
        })),
    )
        .into_response()
}

async fn create_batch(
    State(state): State<AppState>,
    key: ApiKey,
    payload: Result<Json<BatchReq>, JsonRejection>,
) -> Result<Response, ApiError> {
    let req = body(payload)?;
    let created = batches::create(
        &state,
        NewBatch {
            name: req.name.unwrap_or_default(),
            source: format!("API · {}", key.name),
            api_key: Some(&key),
            kind: parse_kind(req.kind.as_deref(), Kind::Marketing)?,
            from: req.from,
            reply_to: req.reply_to,
            subject: req.subject,
            html: req.html,
            text: req.text,
            send_at: parse_send_at(req.send_at)?,
            rate_per_minute: req.rate_per_minute,
            recipients: req
                .recipients
                .into_iter()
                .map(|r| RecipientInput {
                    email: r.email,
                    vars: r.vars,
                })
                .collect(),
            max_recipients: state.cfg.max_recipients_api,
        },
    )
    .await?;
    Ok(created_response(created))
}

#[derive(Deserialize)]
struct EmailReq {
    from: String,
    to: String,
    reply_to: Option<String>,
    subject: String,
    html: Option<String>,
    text: Option<String>,
    #[serde(default)]
    vars: Map<String, Value>,
    kind: Option<String>,
}

/// A single message, e.g. a password reset. Transactional by default.
async fn send_email(
    State(state): State<AppState>,
    key: ApiKey,
    payload: Result<Json<EmailReq>, JsonRejection>,
) -> Result<Response, ApiError> {
    let req = body(payload)?;
    let created = batches::create(
        &state,
        NewBatch {
            name: String::new(),
            source: format!("API · {}", key.name),
            api_key: Some(&key),
            kind: parse_kind(req.kind.as_deref(), Kind::Transactional)?,
            from: req.from,
            reply_to: req.reply_to,
            subject: req.subject,
            html: req.html,
            text: req.text,
            send_at: None,
            rate_per_minute: Some(10_000),
            recipients: vec![RecipientInput {
                email: req.to,
                vars: req.vars,
            }],
            max_recipients: 1,
        },
    )
    .await?;
    Ok(created_response(created))
}

async fn owned_batch(state: &AppState, key: &ApiKey, id: &str) -> Result<batches::Batch, ApiError> {
    batches::get(&state.db, id)
        .await?
        .filter(|b| b.api_key_id == Some(key.id))
        .ok_or_else(ApiError::not_found)
}

async fn get_batch(
    State(state): State<AppState>,
    key: ApiKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let b = owned_batch(&state, &key, &id).await?;
    let counts = batches::counts(&state.db, &b.id).await?;
    Ok(Json(json!({
        "id": b.id,
        "name": b.name,
        "status": b.status,
        "kind": b.kind,
        "test_mode": b.test_mode,
        "from": b.from_addr,
        "subject": b.subject,
        "total": b.total,
        "counts": counts,
        "send_at": ts(b.send_at),
        "created_at": ts(b.created_at),
        "finished_at": b.finished_at.map(ts),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct RecipientQuery {
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_recipients(
    State(state): State<AppState>,
    key: ApiKey,
    Path(id): Path<String>,
    Query(q): Query<RecipientQuery>,
) -> Result<Response, ApiError> {
    let b = owned_batch(&state, &key, &id).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);
    let rows =
        batches::recipients(&state.db, &b.id, q.status.as_deref(), None, limit, offset).await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "email": r.email,
                "status": r.status,
                "attempts": r.attempts,
                "smtp_response": r.smtp_response,
                "message_id": r.message_id,
                "updated_at": r.updated_at.map(ts),
            })
        })
        .collect();
    Ok(Json(json!({ "data": data, "limit": limit, "offset": offset })).into_response())
}

async fn cancel_batch(
    State(state): State<AppState>,
    key: ApiKey,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let b = owned_batch(&state, &key, &id).await?;
    if !batches::cancel(&state.db, &b.id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_cancellable",
            format!("Batch berstatus {} tidak bisa dibatalkan", b.status),
        ));
    }
    Ok(Json(json!({ "id": b.id, "status": "cancelled" })).into_response())
}

async fn list_domains(State(state): State<AppState>, key: ApiKey) -> Result<Response, ApiError> {
    let list = domains::list(&state).await?;
    let data: Vec<Value> = list
        .into_iter()
        .filter(|d| key.domain_id.is_none_or(|id| id == d.id))
        .map(|d| json!({ "name": d.name, "verified": d.verified(), "spf": d.spf_ok, "dkim": d.dkim_ok, "dmarc": d.dmarc_ok }))
        .collect();
    Ok(Json(json!({ "data": data })).into_response())
}
