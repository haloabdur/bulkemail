//! Server-rendered dashboard and the public unsubscribe page.

use crate::AppState;
use crate::auth::{self, User};
use crate::batches::{self, Batch, Kind, NewBatch, RecipientInput};
use crate::domains::{self, Domain};
use crate::error::PageResult;
use crate::mailer::Outcome;
use crate::unsubscribe;
use crate::util::{fmt_num, fmt_opt_time, fmt_time, mask_email, now, parse_local_datetime};
use crate::worker;
use askama::Template;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/static/app.css", get(css))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/", get(overview))
        .route("/batches", get(batch_list).post(batch_create))
        .route("/batches/new", get(compose_page))
        .route("/batches/{id}", get(batch_detail))
        .route("/batches/{id}/export.csv", get(batch_export))
        .route("/batches/{id}/{action}", post(batch_action))
        .route("/api-keys", get(keys_page).post(key_create))
        .route("/api-keys/{id}/revoke", post(key_revoke))
        .route("/settings", get(settings_page))
        .route("/settings/test-smtp", post(test_smtp))
        .route(
            "/suppressions",
            get(suppressions_page).post(suppression_add),
        )
        .route("/suppressions/delete", post(suppression_delete))
        .route("/u/{token}", get(unsub_page).post(unsub_submit))
        .route("/u/{token}/resubscribe", post(resubscribe))
}

pub(crate) fn render<T: Template>(t: T) -> PageResult {
    Ok(Html(t.render()?).into_response())
}

async fn css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/app.css"),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub(crate) struct MsgQuery {
    pub(crate) msg: Option<String>,
}

/// Fixed notices for redirects (`?msg=code`), so no user text is reflected.
pub(crate) fn notice(code: Option<&str>) -> Option<String> {
    let text = match code? {
        "paused" => "Batch dijeda.",
        "resumed" => "Batch dilanjutkan.",
        "cancelled" => "Batch dibatalkan.",
        "retried" => "Penerima yang gagal dimasukkan lagi ke antrean.",
        "not_cancellable" => "Batch ini sudah selesai dan tidak bisa dibatalkan.",
        "revoked" => "API key dicabut. Sistem yang memakainya tidak bisa mengirim lagi.",
        "domain_added" => {
            "Domain ditambahkan. Pasang record DNS di bawah (manual atau lewat Cloudflare), lalu periksa ulang."
        }
        "domain_checked" => "DNS sudah diperiksa ulang.",
        "domain_synced" => "Record DNS diambil ulang dari Stalwart.",
        "domain_removed" => "Domain dihapus.",
        "cf_connected" => {
            "Cloudflare terhubung. Klik Pratinjau untuk melihat perubahan sebelum diterapkan."
        }
        "cf_disconnected" => {
            "Cloudflare diputus. Record yang sudah dibuat tetap ada di Cloudflare."
        }
        "suppressed" => "Alamat ditambahkan ke daftar unsubscribe.",
        "unsuppressed" => "Alamat dihapus dari daftar unsubscribe.",
        _ => return None,
    };
    Some(text.to_string())
}

pub(crate) fn redirect_msg(path: &str, code: &str) -> Response {
    Redirect::to(&format!("{path}?msg={code}")).into_response()
}

// ---------- status labels ----------

fn batch_status(b: &Batch, failed: i64) -> (&'static str, &'static str) {
    match b.status.as_str() {
        "scheduled" => ("Terjadwal", "purple"),
        "queued" => ("Antre", "gray"),
        "sending" => ("Mengirim", "blue"),
        "paused" => ("Dijeda", "gray"),
        "cancelled" => ("Dibatalkan", "gray"),
        "completed" if failed > 0 => ("Selesai · ada gagal", "orange"),
        "completed" => ("Selesai", "gray"),
        _ => ("—", "gray"),
    }
}

fn recipient_status(s: &str) -> (&'static str, &'static str) {
    match s {
        "queued" => ("Antrean", "white"),
        "sent" => ("Diterima Stalwart", "blue"),
        "deferred" => ("Ditunda", "gray"),
        "failed" => ("Gagal", "orange"),
        "skipped" => ("Dilewati", "gray"),
        "cancelled" => ("Dibatalkan", "gray"),
        _ => ("—", "gray"),
    }
}

struct BatchRow {
    id: String,
    name: String,
    source: String,
    total: String,
    pct: i64,
    status: &'static str,
    status_class: &'static str,
    time: String,
}

async fn batch_rows(state: &AppState, list: Vec<Batch>) -> PageResult<Vec<BatchRow>> {
    let mut rows = Vec::with_capacity(list.len());
    for b in list {
        let c = batches::counts(&state.db, &b.id).await?;
        let (status, status_class) = batch_status(&b, c.failed);
        let pct = if b.total > 0 {
            c.processed() * 100 / b.total
        } else {
            0
        };
        let when = if b.status == "scheduled" {
            b.send_at
        } else {
            b.created_at
        };
        rows.push(BatchRow {
            time: fmt_time(when, state.cfg.tz_offset_hours),
            total: fmt_num(b.total),
            id: b.id,
            name: b.name,
            source: b.source,
            pct,
            status,
            status_class,
        });
    }
    Ok(rows)
}

// ---------- login ----------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: bool,
    email: String,
    smtp: String,
}

async fn login_page(State(state): State<AppState>) -> PageResult {
    render(LoginPage {
        error: false,
        email: String::new(),
        smtp: state.cfg.smtp_host.clone(),
    })
}

#[derive(Deserialize)]
struct LoginForm {
    email: String,
    password: String,
    remember: Option<String>,
}

async fn login_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(f): Form<LoginForm>,
) -> PageResult {
    match auth::login(&state.db, &f.email, &f.password).await? {
        Some(token) => {
            let jar = jar.add(auth::session_cookie(&state, token, f.remember.is_some()));
            Ok((jar, Redirect::to("/")).into_response())
        }
        None => {
            let page = LoginPage {
                error: true,
                email: f.email,
                smtp: state.cfg.smtp_host.clone(),
            };
            Ok((StatusCode::UNAUTHORIZED, Html(page.render()?)).into_response())
        }
    }
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> PageResult {
    if let Some(c) = jar.get(auth::SESSION_COOKIE) {
        auth::logout(&state.db, c.value()).await?;
    }
    let jar =
        jar.remove(axum_extra::extract::cookie::Cookie::build(auth::SESSION_COOKIE).path("/"));
    Ok((jar, Redirect::to("/login")).into_response())
}

// ---------- overview ----------

struct Bar {
    ok: i64,
    fail: i64,
    label: String,
}

struct SourceRow {
    name: String,
    count: String,
    pct: i64,
}

#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    nav: &'static str,
    user: String,
    smtp: String,
    sent: String,
    batch_count: String,
    delivered_rate: String,
    failed: String,
    failed_rate: String,
    queue: String,
    queue_note: String,
    bars: Vec<Bar>,
    sources: Vec<SourceRow>,
    batches: Vec<BatchRow>,
    no_domain: bool,
}

async fn overview(State(state): State<AppState>, user: User) -> PageResult {
    let since = now() - 30 * 86_400;
    let (sent, failed): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(status = 'sent'), 0), COALESCE(SUM(status = 'failed'), 0)
         FROM recipients WHERE updated_at > ?",
    )
    .bind(since)
    .fetch_one(&state.db)
    .await?;
    let batch_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM batches WHERE created_at > ?")
        .bind(since)
        .fetch_one(&state.db)
        .await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recipients r JOIN batches b ON b.id = r.batch_id
         WHERE r.status IN ('queued','deferred') AND b.status IN ('queued','sending','scheduled')",
    )
    .fetch_one(&state.db)
    .await?;
    let stalwart_queue = match &state.stalwart {
        Some(api) => api.queue_size().await.ok(),
        None => None,
    };
    let done = sent + failed;
    let pct = |n: i64| {
        if done > 0 {
            format!("{:.1}%", n as f64 * 100.0 / done as f64).replace('.', ",")
        } else {
            "—".into()
        }
    };

    // Volume per day, last 14 days, in the dashboard's time zone.
    let tz = state.cfg.tz_offset_hours as i64 * 3600;
    let today = (now() + tz) / 86_400;
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
        "SELECT (updated_at + ?) / 86400 AS day, SUM(status = 'sent'), SUM(status = 'failed')
         FROM recipients WHERE updated_at > ? GROUP BY day",
    )
    .bind(tz)
    .bind((today - 13) * 86_400 - tz)
    .fetch_all(&state.db)
    .await?;
    let per_day: HashMap<i64, (i64, i64)> = rows.into_iter().map(|(d, s, f)| (d, (s, f))).collect();
    let max = per_day
        .values()
        .map(|(s, f)| s + f)
        .max()
        .unwrap_or(0)
        .max(1);
    let bars = (today - 13..=today)
        .map(|day| {
            let (s, f) = per_day.get(&day).copied().unwrap_or((0, 0));
            Bar {
                ok: s * 100 / max,
                fail: f * 100 / max,
                label: format!(
                    "{} · {} diterima, {} gagal",
                    fmt_time(day * 86_400 - tz, state.cfg.tz_offset_hours)
                        .split(',')
                        .next()
                        .unwrap_or(""),
                    fmt_num(s),
                    fmt_num(f)
                ),
            }
        })
        .collect();

    let src: Vec<(String, i64)> = sqlx::query_as(
        "SELECT source, SUM(total) AS n FROM batches WHERE created_at > ? GROUP BY source ORDER BY n DESC LIMIT 6",
    )
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let src_total: i64 = src.iter().map(|(_, n)| n).sum::<i64>().max(1);
    let sources = src
        .into_iter()
        .map(|(name, n)| SourceRow {
            name,
            count: fmt_num(n),
            pct: n * 100 / src_total,
        })
        .collect();

    let recent: Vec<Batch> =
        sqlx::query_as("SELECT * FROM batches ORDER BY created_at DESC LIMIT 8")
            .fetch_all(&state.db)
            .await?;
    let no_domain = domains::list(&state).await?.is_empty();

    render(OverviewPage {
        nav: "overview",
        user: user.email,
        smtp: state.mailer.description.clone(),
        sent: fmt_num(sent),
        batch_count: fmt_num(batch_count),
        delivered_rate: pct(sent),
        failed: fmt_num(failed),
        failed_rate: pct(failed),
        queue: fmt_num(stalwart_queue.map(|q| q as i64).unwrap_or(pending)),
        queue_note: match stalwart_queue {
            Some(_) => format!(
                "di antrean Stalwart · {} menunggu di BatchMail",
                fmt_num(pending)
            ),
            None => "menunggu dikirim ke Stalwart".into(),
        },
        bars,
        sources,
        batches: batch_rows(&state, recent).await?,
        no_domain,
    })
}

// ---------- batches ----------

#[derive(Template)]
#[template(path = "batches.html")]
struct BatchListPage {
    nav: &'static str,
    user: String,
    smtp: String,
    batches: Vec<BatchRow>,
}

async fn batch_list(State(state): State<AppState>, user: User) -> PageResult {
    let list: Vec<Batch> =
        sqlx::query_as("SELECT * FROM batches ORDER BY created_at DESC LIMIT 200")
            .fetch_all(&state.db)
            .await?;
    render(BatchListPage {
        nav: "batches",
        user: user.email,
        smtp: state.mailer.description.clone(),
        batches: batch_rows(&state, list).await?,
    })
}

#[derive(Template, Default)]
#[template(path = "compose.html")]
struct ComposePage {
    nav: &'static str,
    user: String,
    smtp: String,
    error: Option<String>,
    info: Option<String>,
    domains: Vec<String>,
    unverified: Vec<String>,
    from_name: String,
    from_email: String,
    reply_to: String,
    subject: String,
    html: String,
    text: String,
    paste: String,
    kind: String,
    schedule: String,
    send_at: String,
    rate: i64,
}

async fn compose_base(state: &AppState, user: &User) -> PageResult<ComposePage> {
    let list = domains::list(state).await?;
    let allow = state.cfg.allow_unverified_domains;
    let (ok, not_ok): (Vec<Domain>, Vec<Domain>) =
        list.into_iter().partition(|d| d.verified() || allow);
    Ok(ComposePage {
        nav: "compose",
        user: user.email.clone(),
        smtp: state.mailer.description.clone(),
        from_email: ok.first().map(|d| format!("info@{}", d.name)).unwrap_or_default(),
        domains: ok.into_iter().map(|d| d.name).collect(),
        unverified: not_ok.into_iter().map(|d| d.name).collect(),
        html: "<p>Halo {{nama}},</p>\n<p>…</p>\n<p><a href=\"{{unsubscribe_url}}\">Berhenti berlangganan</a></p>".into(),
        kind: "marketing".into(),
        schedule: "now".into(),
        rate: state.cfg.default_rate_per_minute,
        ..Default::default()
    })
}

async fn compose_page(State(state): State<AppState>, user: User) -> PageResult {
    render(compose_base(&state, &user).await?)
}

async fn batch_create(
    State(state): State<AppState>,
    user: User,
    mut form: Multipart,
) -> PageResult {
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut csv: Option<Vec<u8>> = None;
    while let Some(field) = form.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "csv" {
            let bytes = field.bytes().await?;
            if !bytes.is_empty() {
                csv = Some(bytes.to_vec());
            }
        } else {
            fields.insert(name, field.text().await?);
        }
    }
    let f = |k: &str| fields.get(k).cloned().unwrap_or_default();

    let mut page = compose_base(&state, &user).await?;
    page.from_name = f("from_name");
    page.from_email = f("from_email");
    page.reply_to = f("reply_to");
    page.subject = f("subject");
    page.html = f("html");
    page.text = f("text");
    page.paste = f("paste");
    page.kind = if f("kind") == "transactional" {
        "transactional".into()
    } else {
        "marketing".into()
    };
    page.schedule = if f("schedule") == "later" {
        "later".into()
    } else {
        "now".into()
    };
    page.send_at = f("send_at");
    page.rate = f("rate")
        .parse()
        .unwrap_or(state.cfg.default_rate_per_minute);

    let recipients: Vec<RecipientInput> = match &csv {
        Some(bytes) => match batches::parse_csv(bytes) {
            Ok(r) => r,
            Err(e) => {
                page.error = Some(e);
                return render(page);
            }
        },
        None => batches::parse_pasted(&page.paste),
    };
    let from = if page.from_name.trim().is_empty() {
        page.from_email.trim().to_string()
    } else {
        format!(
            "\"{}\" <{}>",
            page.from_name.trim().replace('"', ""),
            page.from_email.trim()
        )
    };

    if f("action") == "test" {
        let vars = recipients
            .first()
            .map(|r| r.vars.clone())
            .unwrap_or_default();
        let preview = Batch {
            id: String::new(),
            name: String::new(),
            source: String::new(),
            api_key_id: None,
            kind: page.kind.clone(),
            test_mode: false,
            from_addr: from.clone(),
            reply_to: None,
            subject: page.subject.clone(),
            html: Some(page.html.clone()).filter(|s| !s.trim().is_empty()),
            text: Some(page.text.clone()).filter(|s| !s.trim().is_empty()),
            rate_per_minute: 1,
            status: String::new(),
            total: 1,
            send_at: 0,
            created_at: 0,
            finished_at: None,
        };
        match worker::send_test(&state, &preview, &user.email, vars).await {
            Outcome::Accepted(r) => {
                page.info = Some(format!("Email tes dikirim ke {} ({r}).", user.email))
            }
            Outcome::Transient(r) | Outcome::Permanent(r) | Outcome::Unavailable(r) => {
                page.error = Some(format!("Email tes gagal: {r}"))
            }
        }
        return render(page);
    }

    let send_at = if page.schedule == "later" {
        match parse_local_datetime(&page.send_at, state.cfg.tz_offset_hours) {
            Some(t) => Some(t),
            None => {
                page.error = Some("Isi tanggal & jam pengiriman.".into());
                return render(page);
            }
        }
    } else {
        None
    };
    let source = if csv.is_some() {
        "Manual · CSV"
    } else {
        "Manual · tempel daftar"
    };
    let result = batches::create(
        &state,
        NewBatch {
            name: f("name"),
            source: source.into(),
            api_key: None,
            kind: Kind::parse(&page.kind).unwrap_or(Kind::Marketing),
            from,
            reply_to: Some(page.reply_to.clone()),
            subject: page.subject.clone(),
            html: Some(page.html.clone()),
            text: Some(page.text.clone()),
            send_at,
            rate_per_minute: Some(page.rate),
            recipients,
            max_recipients: state.cfg.max_recipients_dashboard,
        },
    )
    .await;
    match result {
        Ok(created) => {
            let skipped = created.rejected.len();
            let path = format!("/batches/{}", created.id);
            Ok(Redirect::to(&if skipped > 0 {
                format!("{path}?skipped={skipped}")
            } else {
                path
            })
            .into_response())
        }
        Err(e) => {
            page.error = Some(e.message);
            render(page)
        }
    }
}

struct RecipientRow {
    email: String,
    status: &'static str,
    status_class: &'static str,
    smtp: String,
    attempts: i64,
    time: String,
}

struct FilterTab {
    href: String,
    label: String,
    active: bool,
}

#[derive(Template)]
#[template(path = "batch.html")]
struct BatchPage {
    nav: &'static str,
    user: String,
    smtp: String,
    notice: Option<String>,
    b: Batch,
    status: &'static str,
    status_class: &'static str,
    started: String,
    processed: String,
    total: String,
    pct_sent: f64,
    pct_failed: f64,
    pct_deferred: f64,
    counts: Vec<(String, String, &'static str)>,
    eta: String,
    can_pause: bool,
    can_resume: bool,
    can_cancel: bool,
    has_failed: bool,
    filters: Vec<FilterTab>,
    rows: Vec<RecipientRow>,
    q: String,
    status_filter: String,
    page: i64,
    has_next: bool,
}

#[derive(Deserialize, Default)]
struct DetailQuery {
    status: Option<String>,
    q: Option<String>,
    page: Option<i64>,
    msg: Option<String>,
    skipped: Option<i64>,
}

async fn batch_detail(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<String>,
    Query(q): Query<DetailQuery>,
) -> PageResult {
    let Some(b) = batches::get(&state.db, &id).await? else {
        return Ok((StatusCode::NOT_FOUND, "Batch tidak ditemukan").into_response());
    };
    let c = batches::counts(&state.db, &b.id).await?;
    let (status, status_class) = batch_status(&b, c.failed);
    let total = b.total.max(1) as f64;
    let status_filter = q
        .status
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let search = q.q.clone().unwrap_or_default();
    let page = q.page.unwrap_or(1).max(1);
    const PER_PAGE: i64 = 50;
    let rows = batches::recipients(
        &state.db,
        &b.id,
        Some(status_filter.as_str()).filter(|s| !s.is_empty()),
        Some(search.as_str()),
        PER_PAGE + 1,
        (page - 1) * PER_PAGE,
    )
    .await?;
    let has_next = rows.len() as i64 > PER_PAGE;
    let rows = rows
        .into_iter()
        .take(PER_PAGE as usize)
        .map(|r| {
            let (status, status_class) = recipient_status(&r.status);
            RecipientRow {
                email: r.email,
                status,
                status_class,
                smtp: r.smtp_response.unwrap_or_else(|| "—".into()),
                attempts: r.attempts,
                time: fmt_opt_time(r.updated_at, state.cfg.tz_offset_hours),
            }
        })
        .collect();
    let filters = [
        ("", "Semua"),
        ("sent", "Diterima"),
        ("failed", "Gagal"),
        ("deferred", "Ditunda"),
        ("queued", "Antrean"),
        ("skipped", "Dilewati"),
    ]
    .into_iter()
    .map(|(s, label)| FilterTab {
        href: if s.is_empty() {
            format!("/batches/{}", b.id)
        } else {
            format!("/batches/{}?status={s}", b.id)
        },
        label: label.into(),
        active: s == status_filter,
    })
    .collect();
    let remaining = c.queued + c.deferred;
    let eta = if matches!(b.status.as_str(), "queued" | "sending") && remaining > 0 {
        format!(
            "± {} menit lagi · {}/menit",
            (remaining + b.rate_per_minute - 1) / b.rate_per_minute,
            fmt_num(b.rate_per_minute)
        )
    } else {
        format!("{}/menit", fmt_num(b.rate_per_minute))
    };
    let mut notice = notice(q.msg.as_deref());
    if let Some(n) = q.skipped.filter(|n| *n > 0) {
        notice = Some(format!(
            "Batch dibuat. {} alamat dilewati (tidak valid, duplikat, atau sudah unsubscribe).",
            fmt_num(n)
        ));
    }
    let active = matches!(b.status.as_str(), "queued" | "sending" | "scheduled");
    render(BatchPage {
        nav: "batches",
        user: user.email,
        smtp: state.mailer.description.clone(),
        notice,
        status,
        status_class,
        started: fmt_time(
            if b.status == "scheduled" {
                b.send_at
            } else {
                b.created_at
            },
            state.cfg.tz_offset_hours,
        ),
        processed: fmt_num(c.processed()),
        total: fmt_num(b.total),
        pct_sent: c.sent as f64 * 100.0 / total,
        pct_failed: c.failed as f64 * 100.0 / total,
        pct_deferred: c.deferred as f64 * 100.0 / total,
        counts: vec![
            ("Diterima Stalwart".into(), fmt_num(c.sent), "blue"),
            ("Gagal".into(), fmt_num(c.failed), "orange"),
            ("Ditunda, dicoba ulang".into(), fmt_num(c.deferred), "gray"),
            ("Antrean".into(), fmt_num(c.queued), "light"),
            (
                "Dilewati / dibatalkan".into(),
                fmt_num(c.skipped + c.cancelled),
                "light",
            ),
        ],
        eta,
        can_pause: active,
        can_resume: b.status == "paused",
        can_cancel: active || b.status == "paused",
        has_failed: c.failed > 0,
        filters,
        rows,
        q: search,
        status_filter,
        page,
        has_next,
        b,
    })
}

async fn batch_action(
    State(state): State<AppState>,
    _user: User,
    Path((id, action)): Path<(String, String)>,
) -> PageResult {
    let path = format!("/batches/{id}");
    let code = match action.as_str() {
        "pause" => {
            batches::set_paused(&state.db, &id, true).await?;
            "paused"
        }
        "resume" => {
            batches::set_paused(&state.db, &id, false).await?;
            "resumed"
        }
        "cancel" => {
            if batches::cancel(&state.db, &id).await? {
                "cancelled"
            } else {
                "not_cancellable"
            }
        }
        "retry" => {
            batches::retry_failed(&state.db, &id).await?;
            "retried"
        }
        _ => return Ok(StatusCode::NOT_FOUND.into_response()),
    };
    Ok(redirect_msg(&path, code))
}

async fn batch_export(
    State(state): State<AppState>,
    _user: User,
    Path(id): Path<String>,
) -> PageResult {
    let rows = batches::recipients(&state.db, &id, None, None, i64::MAX, 0).await?;
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record([
        "email",
        "status",
        "attempts",
        "smtp_response",
        "message_id",
        "updated_at",
    ])?;
    for r in rows {
        w.write_record([
            r.email,
            r.status,
            r.attempts.to_string(),
            r.smtp_response.unwrap_or_default(),
            r.message_id.unwrap_or_default(),
            r.updated_at
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
        ])?;
    }
    let body = w.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{id}.csv\""),
            ),
        ],
        body,
    )
        .into_response())
}

// ---------- API keys ----------

struct KeyRow {
    id: i64,
    name: String,
    display: String,
    test_mode: bool,
    domain: String,
    limit: String,
    usage: String,
    last: String,
}

#[derive(Template)]
#[template(path = "api_keys.html")]
struct KeysPage {
    nav: &'static str,
    user: String,
    smtp: String,
    notice: Option<String>,
    error: Option<String>,
    new_key: Option<(String, String)>,
    keys: Vec<KeyRow>,
    domains: Vec<(i64, String)>,
    base_url: String,
    curl_example: String,
    response_example: String,
}

async fn keys_view(state: &AppState, user: &User) -> PageResult<KeysPage> {
    let list: Vec<auth::ApiKey> =
        sqlx::query_as("SELECT * FROM api_keys WHERE revoked_at IS NULL ORDER BY created_at DESC")
            .fetch_all(&state.db)
            .await?;
    let domain_list = domains::list(state).await?;
    let mut keys = Vec::new();
    for k in list {
        let usage: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(total), 0) FROM batches WHERE api_key_id = ? AND created_at > ?",
        )
        .bind(k.id)
        .bind(now() - 30 * 86_400)
        .fetch_one(&state.db)
        .await?;
        keys.push(KeyRow {
            id: k.id,
            domain: k
                .domain_id
                .and_then(|id| {
                    domain_list
                        .iter()
                        .find(|d| d.id == id)
                        .map(|d| d.name.clone())
                })
                .unwrap_or_else(|| "Semua domain".into()),
            limit: k
                .daily_limit
                .map(|l| format!("{}/hari", fmt_num(l)))
                .unwrap_or_else(|| "Tanpa batas".into()),
            usage: fmt_num(usage),
            last: k
                .last_used_at
                .map(|t| fmt_time(t, state.cfg.tz_offset_hours))
                .unwrap_or_else(|| "Belum pernah".into()),
            name: k.name,
            display: k.display,
            test_mode: k.test_mode,
        });
    }
    let base = state.cfg.public_base_url.clone();
    let from = domain_list
        .first()
        .map(|d| format!("info@{}", d.name))
        .unwrap_or_else(|| "info@domainanda.id".into());
    let curl_example = format!(
        r#"curl -X POST {base}/api/v1/batches \
  -H "Authorization: Bearer $BATCHMAIL_KEY" \
  -H "Content-Type: application/json" \
  -d '{{
    "from": "Nama Pengirim <{from}>",
    "subject": "Halo {{{{nama}}}}",
    "html": "<p>Halo {{{{nama}}}}, jadwal Anda {{{{jam}}}}.</p>",
    "recipients": [
      {{ "email": "andi@contoh.co.id", "vars": {{ "nama": "Andi", "jam": "09:00" }} }},
      {{ "email": "siti@contoh.com",   "vars": {{ "nama": "Siti", "jam": "10:30" }} }}
    ]
  }}'"#
    );
    Ok(KeysPage {
        nav: "keys",
        user: user.email.clone(),
        smtp: state.mailer.description.clone(),
        notice: None,
        error: None,
        new_key: None,
        keys,
        domains: domain_list.into_iter().map(|d| (d.id, d.name)).collect(),
        base_url: base,
        curl_example,
        response_example: "HTTP/1.1 202 Accepted\n{\n  \"id\": \"b_2Ky4hTq9Lm\",\n  \"status\": \"queued\",\n  \"accepted\": 2,\n  \"rejected\": [],\n  \"status_url\": \"/api/v1/batches/b_2Ky4hTq9Lm\"\n}".into(),
    })
}

async fn keys_page(
    State(state): State<AppState>,
    user: User,
    Query(q): Query<MsgQuery>,
) -> PageResult {
    let mut page = keys_view(&state, &user).await?;
    page.notice = notice(q.msg.as_deref());
    render(page)
}

#[derive(Deserialize)]
struct KeyForm {
    name: String,
    mode: Option<String>,
    domain_id: Option<String>,
    daily_limit: Option<String>,
}

async fn key_create(
    State(state): State<AppState>,
    user: User,
    Form(f): Form<KeyForm>,
) -> PageResult {
    let domain_id = f.domain_id.and_then(|d| d.parse::<i64>().ok());
    let limit = f
        .daily_limit
        .and_then(|l| l.trim().replace('.', "").parse::<i64>().ok())
        .filter(|l| *l > 0);
    let test = f.mode.as_deref() == Some("test");
    match auth::create_key(&state.db, &f.name, test, domain_id, limit).await {
        Ok(key) => {
            let mut page = keys_view(&state, &user).await?;
            page.new_key = Some((f.name.trim().to_string(), key.plaintext));
            render(page)
        }
        Err(e) => {
            let mut page = keys_view(&state, &user).await?;
            page.error = Some(e.to_string());
            render(page)
        }
    }
}

async fn key_revoke(State(state): State<AppState>, _user: User, Path(id): Path<i64>) -> PageResult {
    auth::revoke_key(&state.db, id).await?;
    Ok(redirect_msg("/api-keys", "revoked"))
}

// ---------- settings ----------

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    nav: &'static str,
    user: String,
    smtp: String,
    notice: Option<String>,
    error: Option<String>,
    smtp_result: Option<(bool, String)>,
    smtp_user: String,
    stalwart_url: String,
    stalwart_status: String,
    stalwart_ok: bool,
    domain_count: usize,
    domains_pending: usize,
    allow_unverified: bool,
}

async fn settings_view(state: &AppState, user: &User) -> PageResult<SettingsPage> {
    let (stalwart_status, stalwart_ok) = match &state.stalwart {
        None => (
            "Belum diatur — isi STALWART_API_URL dan STALWART_API_TOKEN".to_string(),
            false,
        ),
        Some(api) => match api.queue_size().await {
            Ok(n) => (
                format!("Terhubung · {} pesan di antrean keluar", fmt_num(n as i64)),
                true,
            ),
            Err(e) => (format!("Gagal: {e:#}"), false),
        },
    };
    let domain_list = domains::list(state).await?;
    Ok(SettingsPage {
        nav: "settings",
        user: user.email.clone(),
        smtp: state.mailer.description.clone(),
        notice: None,
        error: None,
        smtp_result: None,
        smtp_user: state
            .cfg
            .smtp_user
            .clone()
            .unwrap_or_else(|| "(tanpa autentikasi)".into()),
        stalwart_url: state
            .cfg
            .stalwart_api_url
            .clone()
            .unwrap_or_else(|| "—".into()),
        stalwart_status,
        stalwart_ok,
        domain_count: domain_list.len(),
        domains_pending: domain_list.iter().filter(|d| !d.verified()).count(),
        allow_unverified: state.cfg.allow_unverified_domains,
    })
}

async fn settings_page(
    State(state): State<AppState>,
    user: User,
    Query(q): Query<MsgQuery>,
) -> PageResult {
    let mut page = settings_view(&state, &user).await?;
    page.notice = notice(q.msg.as_deref());
    render(page)
}

async fn test_smtp(State(state): State<AppState>, user: User) -> PageResult {
    let mut page = settings_view(&state, &user).await?;
    page.smtp_result = Some(match state.mailer.test_connection().await {
        Ok(()) => (true, "Koneksi SMTP ke Stalwart berhasil.".into()),
        Err(e) => (false, format!("Koneksi gagal: {e}")),
    });
    render(page)
}

// ---------- suppression list ----------

struct SuppressionRow {
    email: String,
    reason: String,
    source: String,
    time: String,
}

#[derive(Template)]
#[template(path = "suppressions.html")]
struct SuppressionsPage {
    nav: &'static str,
    user: String,
    smtp: String,
    notice: Option<String>,
    rows: Vec<SuppressionRow>,
    total: String,
    q: String,
}

#[derive(Deserialize, Default)]
struct SearchQuery {
    q: Option<String>,
    msg: Option<String>,
}

async fn suppressions_page(
    State(state): State<AppState>,
    user: User,
    Query(q): Query<SearchQuery>,
) -> PageResult {
    let search = q.q.clone().unwrap_or_default();
    let rows: Vec<(String, Option<String>, String, i64)> = sqlx::query_as(
        "SELECT email, reason, source, created_at FROM suppressions WHERE email LIKE ? ORDER BY created_at DESC LIMIT 500",
    )
    .bind(format!("%{}%", search.to_ascii_lowercase()))
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM suppressions")
        .fetch_one(&state.db)
        .await?;
    render(SuppressionsPage {
        nav: "suppressions",
        user: user.email,
        smtp: state.mailer.description.clone(),
        notice: notice(q.msg.as_deref()),
        rows: rows
            .into_iter()
            .map(|(email, reason, source, t)| SuppressionRow {
                email,
                reason: reason.unwrap_or_else(|| "—".into()),
                source: match source.as_str() {
                    "unsubscribe" => "Klik unsubscribe".into(),
                    "manual" => "Ditambah manual".into(),
                    other => other.to_string(),
                },
                time: fmt_time(t, state.cfg.tz_offset_hours),
            })
            .collect(),
        total: fmt_num(total),
        q: search,
    })
}

#[derive(Deserialize)]
struct SuppressionForm {
    email: String,
    reason: Option<String>,
}

pub async fn suppress(
    db: &sqlx::SqlitePool,
    email: &str,
    reason: Option<&str>,
    source: &str,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO suppressions (email, reason, source, created_at) VALUES (?, ?, ?, ?) ON CONFLICT(email) DO NOTHING")
        .bind(email.trim().to_ascii_lowercase())
        .bind(reason.map(str::trim).filter(|r| !r.is_empty()))
        .bind(source)
        .bind(now())
        .execute(db)
        .await?;
    Ok(())
}

async fn suppression_add(
    State(state): State<AppState>,
    _user: User,
    Form(f): Form<SuppressionForm>,
) -> PageResult {
    for email in f
        .email
        .split([',', '\n', ';'])
        .map(str::trim)
        .filter(|e| batches::valid_email(e))
    {
        suppress(&state.db, email, f.reason.as_deref(), "manual").await?;
    }
    Ok(redirect_msg("/suppressions", "suppressed"))
}

#[derive(Deserialize)]
struct EmailForm {
    email: String,
}

async fn suppression_delete(
    State(state): State<AppState>,
    _user: User,
    Form(f): Form<EmailForm>,
) -> PageResult {
    sqlx::query("DELETE FROM suppressions WHERE email = ?")
        .bind(f.email.trim().to_ascii_lowercase())
        .execute(&state.db)
        .await?;
    Ok(redirect_msg("/suppressions", "unsuppressed"))
}

// ---------- public unsubscribe page ----------

#[derive(Template)]
#[template(path = "unsubscribe.html")]
struct UnsubPage {
    state: &'static str,
    token: String,
    email: String,
    domain: String,
}

fn unsub_invalid() -> PageResult {
    let page = UnsubPage {
        state: "invalid",
        token: String::new(),
        email: String::new(),
        domain: String::new(),
    };
    Ok((StatusCode::NOT_FOUND, Html(page.render()?)).into_response())
}

async fn unsub_page(State(state): State<AppState>, Path(token): Path<String>) -> PageResult {
    let Some(t) = unsubscribe::verify(&state.cfg.secret, &token) else {
        return unsub_invalid();
    };
    let already: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM suppressions WHERE email = ?)")
            .bind(&t.email)
            .fetch_one(&state.db)
            .await?;
    render(UnsubPage {
        state: if already { "done" } else { "confirm" },
        token,
        email: mask_email(&t.email),
        domain: t.domain,
    })
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        let decode = |s: &str| {
            urlencoding::decode(&s.replace('+', " "))
                .map(|c| c.into_owned())
                .ok()
        };
        (decode(k)? == key).then(|| decode(v)).flatten()
    })
}

/// Handles both the form on the page and RFC 8058 one-click POSTs from mail clients
/// (body `List-Unsubscribe=One-Click`, urlencoded or multipart).
async fn unsub_submit(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: String,
) -> PageResult {
    let Some(t) = unsubscribe::verify(&state.cfg.secret, &token) else {
        return unsub_invalid();
    };
    let reason = form_value(&body, "reason");
    suppress(&state.db, &t.email, reason.as_deref(), "unsubscribe").await?;
    if body.contains("One-Click") {
        return Ok((StatusCode::OK, "Unsubscribed").into_response());
    }
    render(UnsubPage {
        state: "done",
        token,
        email: mask_email(&t.email),
        domain: t.domain,
    })
}

async fn resubscribe(State(state): State<AppState>, Path(token): Path<String>) -> PageResult {
    let Some(t) = unsubscribe::verify(&state.cfg.secret, &token) else {
        return unsub_invalid();
    };
    sqlx::query("DELETE FROM suppressions WHERE email = ? AND source = 'unsubscribe'")
        .bind(&t.email)
        .execute(&state.db)
        .await?;
    render(UnsubPage {
        state: "resubscribed",
        token,
        email: mask_email(&t.email),
        domain: t.domain,
    })
}
