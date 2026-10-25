//! Dashboard pages for sending domains and their DNS records: publish them by
//! hand (copy the records or import a zone file) or let BatchMail write them
//! through the Cloudflare API.

use crate::AppState;
use crate::auth::User;
use crate::cloudflare::Change;
use crate::domains::{self, Domain};
use crate::error::PageResult;
use crate::stalwart::DnsRecord;
use crate::util::fmt_opt_time;
use crate::web::{MsgQuery, notice, redirect_msg, render};
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/domains", get(list_page).post(add))
        .route("/domains/{id}", get(detail_page))
        .route("/domains/{id}/{action}", post(action))
        .route("/domains/{id}/cloudflare/connect", post(cf_connect))
        .route("/domains/{id}/cloudflare/disconnect", post(cf_disconnect))
        .route("/domains/{id}/cloudflare/preview", post(cf_preview))
        .route("/domains/{id}/cloudflare/apply", post(cf_apply))
}

fn status_of(d: &Domain) -> (&'static str, &'static str) {
    if d.verified() && d.dmarc_ok {
        ("Siap dipakai", "blue")
    } else if d.verified() {
        ("Siap · DMARC belum", "blue")
    } else if d.checked_at.is_none() {
        ("Belum diperiksa", "gray")
    } else {
        ("Belum terverifikasi", "orange")
    }
}

// ---------- list ----------

struct DomainRow {
    id: i64,
    name: String,
    status: &'static str,
    status_class: &'static str,
    spf: bool,
    dkim: bool,
    dmarc: bool,
    provider: &'static str,
    checked: String,
}

#[derive(Template)]
#[template(path = "domains.html")]
struct ListPage {
    nav: &'static str,
    user: String,
    smtp: String,
    notice: Option<String>,
    error: Option<String>,
    rows: Vec<DomainRow>,
    stalwart_api: bool,
}

async fn list_view(state: &AppState, user: &User) -> PageResult<ListPage> {
    let rows = domains::list(state)
        .await?
        .into_iter()
        .map(|d| {
            let (status, status_class) = status_of(&d);
            DomainRow {
                id: d.id,
                status,
                status_class,
                spf: d.spf_ok,
                dkim: d.dkim_ok,
                dmarc: d.dmarc_ok,
                provider: if d.uses_cloudflare() {
                    "Cloudflare"
                } else {
                    "Manual"
                },
                checked: fmt_opt_time(d.checked_at, state.cfg.tz_offset_hours),
                name: d.name,
            }
        })
        .collect();
    Ok(ListPage {
        nav: "domains",
        user: user.email.clone(),
        smtp: state.mailer.description.clone(),
        notice: None,
        error: None,
        rows,
        stalwart_api: state.stalwart.is_some(),
    })
}

async fn list_page(
    State(state): State<AppState>,
    user: User,
    Query(q): Query<MsgQuery>,
) -> PageResult {
    let mut page = list_view(&state, &user).await?;
    page.notice = notice(q.msg.as_deref());
    render(page)
}

#[derive(Deserialize)]
struct AddForm {
    name: String,
    dkim_selector: Option<String>,
}

async fn add(State(state): State<AppState>, user: User, Form(f): Form<AddForm>) -> PageResult {
    match domains::add(&state, &f.name, f.dkim_selector.as_deref()).await {
        Ok(d) => Ok(redirect_msg(&format!("/domains/{}", d.id), "domain_added")),
        Err(e) => {
            let mut page = list_view(&state, &user).await?;
            page.error = Some(e.to_string());
            render(page)
        }
    }
}

// ---------- detail ----------

struct RecordRow {
    kind: String,
    name: String,
    value: String,
    purpose: &'static str,
    required: bool,
    status: &'static str,
    status_class: &'static str,
}

struct ChangeRow {
    label: &'static str,
    class: &'static str,
    kind: String,
    name: String,
    content: String,
    note: String,
}

fn change_rows(changes: &[Change]) -> Vec<ChangeRow> {
    changes
        .iter()
        .map(|c| {
            let (label, class) = domains::change_label(c);
            ChangeRow {
                label,
                class,
                kind: c.kind.clone(),
                name: c.name.clone(),
                content: match c.priority {
                    Some(p) => format!("{p} {}", c.content),
                    None => c.content.clone(),
                },
                note: c.note.clone(),
            }
        })
        .collect()
}

/// A BIND zone-file snippet; most DNS providers (Cloudflare included) can import it.
pub fn zone_file(records: &[DnsRecord]) -> String {
    let mut out =
        String::from("; Record DNS dari BatchMail — impor atau salin ke penyedia DNS Anda\n");
    let mut receive_header = false;
    for r in records {
        let required = r.purpose != "receive";
        if r.value.contains('…') {
            out.push_str(&format!(
                "; {} {} — salin kunci DKIM dari Stalwart\n",
                r.name, r.kind
            ));
            continue;
        }
        if !required && !receive_header {
            out.push_str("\n; Opsional — hanya jika domain ini MENERIMA email lewat Stalwart:\n");
            receive_header = true;
        }
        let name = if r.name.ends_with('.') {
            r.name.clone()
        } else {
            format!("{}.", r.name)
        };
        let value = if r.kind.eq_ignore_ascii_case("TXT") {
            // TXT strings are limited to 255 characters each; long DKIM keys are split.
            let chars: Vec<char> = r.value.chars().collect();
            chars
                .chunks(250)
                .map(|c| format!("\"{}\"", c.iter().collect::<String>().replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            r.value.clone()
        };
        let prefix = if required { "" } else { ";" };
        out.push_str(&format!(
            "{prefix}{name}\t3600\tIN\t{}\t{value}\n",
            r.kind.to_ascii_uppercase()
        ));
    }
    out
}

#[derive(Template)]
#[template(path = "domain.html")]
struct DetailPage {
    nav: &'static str,
    user: String,
    smtp: String,
    notice: Option<String>,
    error: Option<String>,
    id: i64,
    name: String,
    status: &'static str,
    status_class: &'static str,
    spf: bool,
    dkim: bool,
    dmarc: bool,
    checked: String,
    stalwart_synced: bool,
    stalwart_error: String,
    stalwart_api: bool,
    records: Vec<RecordRow>,
    has_receive: bool,
    zone_file: String,
    spf_mechanism: String,
    via_cloudflare: bool,
    cf_connected: bool,
    cf_zone: String,
    cf_env_token: bool,
    cf_last_sync: String,
    plan: Vec<ChangeRow>,
    plan_title: String,
    plan_is_preview: bool,
    last_changes: Vec<ChangeRow>,
    include_receiving: bool,
}

async fn detail_view(
    state: &AppState,
    user: &User,
    d: &Domain,
    via: Option<&str>,
) -> PageResult<DetailPage> {
    let (status, status_class) = status_of(d);
    let records = d.records();
    let rows = records
        .iter()
        .map(|r| {
            let (status, status_class) = match r.published {
                Some(true) => ("Terpasang", "blue"),
                Some(false) => ("Belum ditemukan", "orange"),
                None => ("Tidak dicek", "white"),
            };
            RecordRow {
                kind: r.kind.clone(),
                name: r.name.clone(),
                value: r.value.clone(),
                purpose: match r.purpose.as_str() {
                    "spf" => "SPF · izinkan Stalwart mengirim",
                    "dkim" => "DKIM · tanda tangan email",
                    "dmarc" => "DMARC · kebijakan domain",
                    _ => "Opsional · menerima email",
                },
                required: r.purpose != "receive",
                status,
                status_class,
            }
        })
        .collect();
    let via_cloudflare = match via {
        Some("cloudflare") => true,
        Some(_) => false,
        None => d.uses_cloudflare(),
    };
    Ok(DetailPage {
        nav: "domains",
        user: user.email.clone(),
        smtp: state.mailer.description.clone(),
        notice: None,
        error: None,
        id: d.id,
        name: d.name.clone(),
        status,
        status_class,
        spf: d.spf_ok,
        dkim: d.dkim_ok,
        dmarc: d.dmarc_ok,
        checked: fmt_opt_time(d.checked_at, state.cfg.tz_offset_hours),
        stalwart_synced: d.stalwart_synced,
        stalwart_error: d.last_error.clone().unwrap_or_default(),
        stalwart_api: state.stalwart.is_some(),
        has_receive: records.iter().any(|r| r.purpose == "receive"),
        zone_file: zone_file(&records),
        records: rows,
        spf_mechanism: domains::spf_mechanism(&state.cfg),
        via_cloudflare,
        cf_connected: d.uses_cloudflare(),
        cf_zone: d.cf_zone_name.clone().unwrap_or_default(),
        cf_env_token: state.cfg.cloudflare_api_token.is_some(),
        cf_last_sync: fmt_opt_time(d.cf_synced_at, state.cfg.tz_offset_hours),
        plan: Vec::new(),
        plan_title: String::new(),
        plan_is_preview: false,
        last_changes: change_rows(&d.last_changes()),
        include_receiving: false,
    })
}

#[derive(Deserialize, Default)]
struct DetailQuery {
    msg: Option<String>,
    via: Option<String>,
}

async fn load(state: &AppState, id: i64) -> PageResult<Option<Domain>> {
    Ok(domains::get(state, id).await?)
}

fn not_found() -> PageResult {
    Ok((StatusCode::NOT_FOUND, "Domain tidak ditemukan").into_response())
}

async fn detail_page(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Query(q): Query<DetailQuery>,
) -> PageResult {
    let Some(d) = load(&state, id).await? else {
        return not_found();
    };
    let mut page = detail_view(&state, &user, &d, q.via.as_deref()).await?;
    page.notice = notice(q.msg.as_deref());
    render(page)
}

async fn action(
    State(state): State<AppState>,
    _user: User,
    Path((id, action)): Path<(i64, String)>,
) -> PageResult {
    let Some(d) = load(&state, id).await? else {
        return not_found();
    };
    let path = format!("/domains/{id}");
    match action.as_str() {
        "verify" => {
            domains::verify(&state, &d).await?;
            Ok(redirect_msg(&path, "domain_checked"))
        }
        "sync" => {
            domains::sync_with_stalwart(&state, id, None).await?;
            let d = load(&state, id).await?.expect("exists");
            domains::verify(&state, &d).await?;
            Ok(redirect_msg(&path, "domain_synced"))
        }
        "delete" => {
            domains::remove(&state, id).await?;
            Ok(redirect_msg("/domains", "domain_removed"))
        }
        _ => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

// ---------- Cloudflare ----------

#[derive(Deserialize)]
struct ConnectForm {
    token: Option<String>,
}

async fn cf_connect(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Form(f): Form<ConnectForm>,
) -> PageResult {
    let Some(d) = load(&state, id).await? else {
        return not_found();
    };
    match domains::connect_cloudflare(&state, &d, f.token.as_deref()).await {
        Ok(()) => Ok(
            Redirect::to(&format!("/domains/{id}?via=cloudflare&msg=cf_connected")).into_response(),
        ),
        Err(e) => {
            let mut page = detail_view(&state, &user, &d, Some("cloudflare")).await?;
            page.error = Some(format!("{e:#}"));
            render(page)
        }
    }
}

async fn cf_disconnect(
    State(state): State<AppState>,
    _user: User,
    Path(id): Path<i64>,
) -> PageResult {
    domains::disconnect_cloudflare(&state, id).await?;
    Ok(Redirect::to(&format!("/domains/{id}?via=manual&msg=cf_disconnected")).into_response())
}

#[derive(Deserialize)]
struct SyncForm {
    include_receiving: Option<String>,
}

async fn cf_preview(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Form(f): Form<SyncForm>,
) -> PageResult {
    let Some(d) = load(&state, id).await? else {
        return not_found();
    };
    let include = f.include_receiving.is_some();
    let mut page = detail_view(&state, &user, &d, Some("cloudflare")).await?;
    page.include_receiving = include;
    match domains::cloudflare_plan(&state, &d, include).await {
        Ok(plan) => {
            page.plan = change_rows(&plan);
            page.plan_title = "Pratinjau perubahan di Cloudflare".into();
            page.plan_is_preview = true;
        }
        Err(e) => page.error = Some(format!("{e:#}")),
    }
    render(page)
}

async fn cf_apply(
    State(state): State<AppState>,
    user: User,
    Path(id): Path<i64>,
    Form(f): Form<SyncForm>,
) -> PageResult {
    let Some(d) = load(&state, id).await? else {
        return not_found();
    };
    let include = f.include_receiving.is_some();
    match domains::cloudflare_apply(&state, &d, include).await {
        Ok(changes) => {
            let d = load(&state, id).await?.expect("exists");
            let mut page = detail_view(&state, &user, &d, Some("cloudflare")).await?;
            page.include_receiving = include;
            let failed = changes.iter().filter(|c| c.ok == Some(false)).count();
            page.plan = change_rows(&changes);
            page.plan_title = "Hasil sinkron ke Cloudflare".into();
            if failed > 0 {
                page.error = Some(format!(
                    "{failed} perubahan gagal diterapkan — lihat tabel di bawah."
                ));
            } else if d.verified() {
                page.notice =
                    Some("Record DNS terpasang di Cloudflare dan sudah terverifikasi.".into());
            } else {
                page.notice = Some(
                    "Record DNS terpasang di Cloudflare. Verifikasi bisa butuh beberapa menit — klik Periksa ulang DNS nanti."
                        .into(),
                );
            }
            render(page)
        }
        Err(e) => {
            let mut page = detail_view(&state, &user, &d, Some("cloudflare")).await?;
            page.error = Some(format!("{e:#}"));
            render(page)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::zone_file;
    use crate::stalwart::DnsRecord;

    #[test]
    fn writes_zone_file_with_split_txt_and_optional_records() {
        let long_key = format!("v=DKIM1; k=rsa; p={}", "A".repeat(400));
        let recs = vec![
            DnsRecord {
                kind: "TXT".into(),
                name: "x.id.".into(),
                value: "v=spf1 a:mail.x.id ~all".into(),
                purpose: "spf".into(),
                published: None,
            },
            DnsRecord {
                kind: "TXT".into(),
                name: "s._domainkey.x.id".into(),
                value: long_key,
                purpose: "dkim".into(),
                published: None,
            },
            DnsRecord {
                kind: "MX".into(),
                name: "x.id.".into(),
                value: "10 mail.x.id.".into(),
                purpose: "receive".into(),
                published: None,
            },
        ];
        let z = zone_file(&recs);
        assert!(z.contains("x.id.\t3600\tIN\tTXT\t\"v=spf1 a:mail.x.id ~all\""));
        assert!(z.contains("s._domainkey.x.id.\t3600\tIN\tTXT\t\"v=DKIM1"));
        assert!(z.contains("\" \""), "long TXT is split");
        assert!(z.contains(";x.id.\t3600\tIN\tMX\t10 mail.x.id."));
    }
}
