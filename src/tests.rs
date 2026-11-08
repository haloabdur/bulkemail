//! End-to-end tests through the router, against an in-memory database.

use crate::{AppState, app, auth, config::Config, unsubscribe, worker};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

async fn setup() -> (AppState, Router) {
    let state = AppState::new(Config::for_tests()).await.unwrap();
    sqlx::query("INSERT INTO domains (name, spf_ok, dkim_ok, dmarc_ok, created_at) VALUES ('domainanda.id', 1, 1, 1, 0)")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO domains (name, created_at) VALUES ('belum.id', 0)")
        .execute(&state.db)
        .await
        .unwrap();
    let router = app(state.clone());
    (state, router)
}

async fn send(
    router: &Router,
    req: Request<Body>,
) -> (StatusCode, HashMap<String, String>, String) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).to_string())
}

fn api(method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        b = b.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

fn batch_body(from: &str, emails: &[&str], kind: &str) -> Value {
    json!({
        "from": from,
        "subject": "Halo {{nama}}",
        "html": "<p>Halo {{nama}}</p>",
        "kind": kind,
        "recipients": emails.iter().map(|e| json!({"email": e, "vars": {"nama": "Andi"}})).collect::<Vec<_>>()
    })
}

#[tokio::test]
async fn api_batch_lifecycle_in_test_mode() {
    let (state, router) = setup().await;
    let key = auth::create_key(&state.db, "Staging", true, None, None)
        .await
        .unwrap()
        .plaintext;

    let (status, _, _) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            None,
            Some(batch_body(
                "a@domainanda.id",
                &["x@contoh.com"],
                "marketing",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key),
            Some(batch_body("a@belum.id", &["x@contoh.com"], "marketing")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("domain_not_verified"), "{body}");

    let emails = [
        "andi@contoh.co.id",
        "ANDI@contoh.co.id",
        "budi@perusahaan",
        "siti@contoh.com",
    ];
    let (status, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key),
            Some(batch_body("Tim <info@domainanda.id>", &emails, "marketing")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["accepted"], 2);
    assert_eq!(created["rejected"].as_array().unwrap().len(), 2);
    let id = created["id"].as_str().unwrap().to_string();

    // Test-mode key: the worker marks messages as sent without contacting SMTP.
    let mut credits = HashMap::new();
    worker::step(&state, &mut credits).await.unwrap();
    worker::step(&state, &mut credits).await.unwrap();

    let (status, _, body) = send(
        &router,
        api("GET", &format!("/api/v1/batches/{id}"), Some(&key), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let batch: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(batch["counts"]["sent"], 2, "{body}");
    assert_eq!(batch["status"], "completed");

    let (_, _, body) = send(
        &router,
        api(
            "GET",
            &format!("/api/v1/batches/{id}/recipients"),
            Some(&key),
            None,
        ),
    )
    .await;
    assert!(body.contains("Mode test"), "{body}");

    // Another key cannot see this batch.
    let other = auth::create_key(&state.db, "Lain", false, None, None)
        .await
        .unwrap()
        .plaintext;
    let (status, _, _) = send(
        &router,
        api("GET", &format!("/api/v1/batches/{id}"), Some(&other), None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Revoked keys are rejected.
    let key_id: i64 = sqlx::query_scalar("SELECT id FROM api_keys WHERE name = 'Lain'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    auth::revoke_key(&state.db, key_id).await.unwrap();
    let (status, _, _) = send(&router, api("GET", "/api/v1/domains", Some(&other), None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn one_click_unsubscribe_suppresses_marketing_only() {
    let (state, router) = setup().await;
    let key = auth::create_key(&state.db, "Toko", true, None, None)
        .await
        .unwrap()
        .plaintext;
    let token = unsubscribe::token(&state.cfg.secret, "andi@contoh.co.id", "domainanda.id");

    let (status, _, body) = send(
        &router,
        Request::get(format!("/u/{token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("an***@contoh.co.id"));

    let req = Request::post(format!("/u/{token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("List-Unsubscribe=One-Click"))
        .unwrap();
    let (status, _, _) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key),
            Some(batch_body(
                "info@domainanda.id",
                &["andi@contoh.co.id"],
                "marketing",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("no_valid_recipients"));

    let (status, _, _) = send(
        &router,
        api("POST", "/api/v1/emails", Some(&key), Some(json!({
            "from": "info@domainanda.id", "to": "andi@contoh.co.id", "subject": "Struk", "text": "Terima kasih"
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _, _) = send(
        &router,
        Request::get("/u/rusak.token").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dashboard_requires_login_and_renders_every_page() {
    let (state, router) = setup().await;
    auth::upsert_user(&state.db, "admin@domainanda.id", "rahasia-sekali")
        .await
        .unwrap();

    let (status, headers, _) = send(&router, Request::get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login");

    let login = |pw: &str| {
        Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "email=admin%40domainanda.id&password={pw}&remember=1"
            )))
            .unwrap()
    };
    let (status, _, _) = send(&router, login("salah")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, headers, _) = send(&router, login("rahasia-sekali")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = headers["set-cookie"].split(';').next().unwrap().to_string();

    let key = auth::create_key(&state.db, "HR", true, None, None)
        .await
        .unwrap()
        .plaintext;
    let (_, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key),
            Some(batch_body(
                "info@domainanda.id",
                &["a@contoh.com"],
                "marketing",
            )),
        ),
    )
    .await;
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    for (path, needle) in [
        ("/", "Ringkasan"),
        ("/batches", "Riwayat batch"),
        ("/batches/new", "Kirim batch baru"),
        (&format!("/batches/{id}") as &str, "Penerima"),
        (
            &format!("/batches/{id}?status=queued&q=a") as &str,
            "a@contoh.com",
        ),
        ("/api-keys", "API keys"),
        ("/settings", "Stalwart"),
        ("/suppressions", "Daftar unsubscribe"),
        ("/domains", "Domain terdaftar"),
        ("/domains/1", "Record DNS"),
        ("/domains/1?via=manual", "File zona"),
        ("/domains/1?via=cloudflare", "Hubungkan Cloudflare"),
    ] {
        let req = Request::get(path)
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.contains(needle), "{path} should contain {needle}");
    }

    let req = Request::get(format!("/batches/{id}/export.csv"))
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("email,status"));

    let req = Request::post(format!("/batches/{id}/cancel"))
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&router, req).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let status: String = sqlx::query_scalar("SELECT status FROM batches WHERE id = ?")
        .bind(&id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
}

// ---------- bounces ----------

fn hook(method: &str, uri: &str, token: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn recipient_row(state: &AppState, email: &str) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT status, message_id, smtp_response FROM recipients WHERE email = ?")
        .bind(email)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

async fn suppression_source(state: &AppState, email: &str) -> Option<String> {
    sqlx::query_scalar("SELECT source FROM suppressions WHERE email = ?")
        .bind(email)
        .fetch_optional(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn bounce_webhook_updates_recipients_and_suppresses() {
    let (state, router) = setup().await;
    let key = auth::create_key(&state.db, "Toko", true, None, None)
        .await
        .unwrap()
        .plaintext;
    let emails = [
        "ok@contoh.com",
        "mati@contoh.com",
        "penuh@contoh.com",
        "spam@contoh.com",
        "blok@contoh.com",
    ];
    let (status, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key),
            Some(batch_body("info@domainanda.id", &emails, "marketing")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut credits = HashMap::new();
    for _ in 0..3 {
        worker::step(&state, &mut credits).await.unwrap();
    }
    let mid = |e: &str| {
        let e = e.to_string();
        let state = state.clone();
        async move { recipient_row(&state, &e).await.1.unwrap() }
    };

    // No / wrong token.
    let (status, _, _) = send(&router, hook("POST", "/hooks/bounce", None, "{}")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(&router, hook("POST", "/hooks/bounce", Some("salah"), "{}")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Raw DSN email: unknown user → bounced + suppressed.
    let dsn = format!(
        "Final-Recipient: rfc822; mati@contoh.com\r\nAction: failed\r\nStatus: 5.1.1\r\n\
         Diagnostic-Code: smtp; 550 5.1.1 User unknown\r\n\r\nMessage-ID: {}\r\n",
        mid("mati@contoh.com").await
    );
    let req = Request::post("/hooks/bounce")
        .header(header::AUTHORIZATION, "Bearer test-bounce-token")
        .header(header::CONTENT_TYPE, "message/rfc822")
        .body(Body::from(dsn.clone()))
        .unwrap();
    let (status, _, body) = send(&router, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["results"]["bounced"],
        1
    );
    let (st, _, resp) = recipient_row(&state, "mati@contoh.com").await;
    assert_eq!(st, "bounced");
    assert!(
        resp.unwrap()
            .contains("5.1.1: smtp; 550 5.1.1 User unknown")
    );
    assert_eq!(
        suppression_source(&state, "mati@contoh.com")
            .await
            .as_deref(),
        Some("bounce")
    );

    // Same event again (webhook retry) is a duplicate, not a second bounce.
    let req = Request::post("/hooks/bounce?token=test-bounce-token")
        .body(Body::from(dsn))
        .unwrap();
    let (_, _, body) = send(&router, req).await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["results"]["duplicate"],
        1
    );

    // JSON batch of events: soft, complaint, mailbox full, policy block, a success, a stranger.
    let events = json!({ "events": [
        { "type": "soft_bounce", "message_id": mid("ok@contoh.com").await, "code": "4.2.2" },
        { "type": "complaint", "message_id": mid("spam@contoh.com").await },
        { "type": "hard_bounce", "message_id": mid("penuh@contoh.com").await, "code": "5.2.2" },
        { "type": "hard_bounce", "message_id": mid("blok@contoh.com").await, "code": "5.7.1" },
        { "type": "delivery.delivered", "message_id": mid("ok@contoh.com").await },
        { "type": "hard_bounce", "message_id": "<b_ZZZZZZZZZZ.1.abcdefgh@domainanda.id>", "code": "5.1.1" },
    ]});
    let (status, _, body) = send(
        &router,
        hook(
            "POST",
            "/hooks/bounce",
            Some("test-bounce-token"),
            &events.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(r["received"], 6);
    assert_eq!(r["results"]["soft"], 1, "{body}");
    assert_eq!(r["results"]["complained"], 1, "{body}");
    assert_eq!(r["results"]["bounced"], 2, "{body}");
    assert_eq!(r["results"]["ignored"], 1, "{body}");
    assert_eq!(r["results"]["unmatched"], 1, "{body}");

    assert_eq!(recipient_row(&state, "ok@contoh.com").await.0, "sent");
    assert_eq!(
        recipient_row(&state, "spam@contoh.com").await.0,
        "complained"
    );
    assert_eq!(
        suppression_source(&state, "spam@contoh.com")
            .await
            .as_deref(),
        Some("complaint")
    );
    // Mailbox full / policy block: bounced, but the address stays usable.
    assert_eq!(recipient_row(&state, "penuh@contoh.com").await.0, "bounced");
    assert_eq!(suppression_source(&state, "penuh@contoh.com").await, None);
    assert_eq!(suppression_source(&state, "blok@contoh.com").await, None);

    // Counts in the API.
    let (_, _, body) = send(
        &router,
        api("GET", &format!("/api/v1/batches/{id}"), Some(&key), None),
    )
    .await;
    let counts = serde_json::from_str::<Value>(&body).unwrap()["counts"].clone();
    assert_eq!(counts["sent"], 1, "{counts}");
    assert_eq!(counts["bounced"], 3, "{counts}");
    assert_eq!(counts["complained"], 1, "{counts}");
    let (_, _, body) = send(
        &router,
        api(
            "GET",
            &format!("/api/v1/batches/{id}/recipients?status=bounced"),
            Some(&key),
            None,
        ),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["data"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // The dashboard flags a finished batch that has bounces.
    auth::upsert_user(&state.db, "admin@domainanda.id", "rahasia-sekali")
        .await
        .unwrap();
    let (_, headers, _) = send(
        &router,
        Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=admin%40domainanda.id&password=rahasia-sekali&remember=1",
            ))
            .unwrap(),
    )
    .await;
    let cookie = headers["set-cookie"].split(';').next().unwrap().to_string();
    for path in ["/batches".to_string(), format!("/batches/{id}")] {
        let req = Request::get(&path)
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.contains("Selesai · ada bounce"), "{path}");
    }

    // Dead addresses are rejected for new batches, marketing and transactional alike.
    let (status, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key),
            Some(batch_body(
                "info@domainanda.id",
                &["mati@contoh.com", "baru@contoh.com"],
                "marketing",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let r: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(r["rejected"][0]["reason"], "bounced");
    let (status, _, _) = send(
        &router,
        api("POST", "/api/v1/emails", Some(&key), Some(json!({
            "from": "info@domainanda.id", "to": "spam@contoh.com", "subject": "Struk", "text": "Halo"
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Already-queued mail to an address that bounces meanwhile is skipped by the worker.
    let (_, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key),
            Some(batch_body(
                "info@domainanda.id",
                &["lambat@contoh.com"],
                "transactional",
            )),
        ),
    )
    .await;
    let late: Value = serde_json::from_str(&body).unwrap();
    super::web::suppress(&state.db, "lambat@contoh.com", None, "bounce")
        .await
        .unwrap();
    for _ in 0..2 {
        worker::step(&state, &mut credits).await.unwrap();
    }
    let (st, _, resp) = recipient_row(&state, "lambat@contoh.com").await;
    assert_eq!(st, "skipped", "{late}");
    assert!(resp.unwrap().contains("bounce"));

    // Event log, newest first.
    let (status, _, body) = send(
        &router,
        hook(
            "GET",
            "/hooks/bounce/events?limit=3",
            Some("test-bounce-token"),
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["data"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn bounce_webhook_is_disabled_without_token() {
    let mut cfg = Config::for_tests();
    cfg.bounce_webhook_token = None;
    let router = app(AppState::new(cfg).await.unwrap());
    let (status, _, _) = send(
        &router,
        hook("POST", "/hooks/bounce", Some("apa-saja"), "{}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bounce_webhook_suppresses_an_explicit_email_without_message_id() {
    let (state, router) = setup().await;
    let (status, _, body) = send(
        &router,
        hook(
            "POST",
            "/hooks/bounce",
            Some("test-bounce-token"),
            r#"{"type":"hard_bounce","email":"Lama@Contoh.com","code":"5.1.1","reason":"User unknown"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["results"]["suppressed"],
        1
    );
    assert_eq!(
        suppression_source(&state, "lama@contoh.com")
            .await
            .as_deref(),
        Some("bounce")
    );
}

// ---------- outgoing webhooks ----------

type Received = Vec<(HashMap<String, String>, String)>;

#[derive(Clone, Default)]
struct Sink {
    got: std::sync::Arc<std::sync::Mutex<Received>>,
    fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

async fn sink_handler(
    axum::extract::State(s): axum::extract::State<Sink>,
    headers: axum::http::HeaderMap,
    body: String,
) -> StatusCode {
    let h = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    s.got.lock().unwrap().push((h, body));
    if s.fail.load(std::sync::atomic::Ordering::SeqCst) {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    }
}

async fn start_sink() -> (String, Sink) {
    let sink = Sink::default();
    let router = axum::Router::new()
        .route("/hook", axum::routing::post(sink_handler))
        .with_state(sink.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, sink)
}

async fn outbox(state: &AppState) -> Vec<(String, String, i64)> {
    sqlx::query_as("SELECT event, status, attempts FROM webhook_deliveries ORDER BY id")
        .fetch_all(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn webhooks_report_completion_and_bounces_with_signature_and_retries() {
    let (state, router) = setup().await;
    let (url, sink) = start_sink().await;
    let client = crate::webhooks::client().unwrap();

    let key = auth::create_key(&state.db, "Toko", true, None, None)
        .await
        .unwrap();
    crate::webhooks::set_url(&state, key.id, Some(&url))
        .await
        .unwrap();
    let secret = crate::webhooks::secret(&state, key.id)
        .await
        .unwrap()
        .unwrap();
    assert!(secret.starts_with("whsec_"));
    // Setting the URL again keeps the secret.
    crate::webhooks::set_url(&state, key.id, Some(&url))
        .await
        .unwrap();
    assert_eq!(
        crate::webhooks::secret(&state, key.id)
            .await
            .unwrap()
            .unwrap(),
        secret
    );

    // A batch finishes -> batch.completed.
    let (status, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key.plaintext),
            Some(batch_body(
                "info@domainanda.id",
                &["a@contoh.com", "b@contoh.com"],
                "marketing",
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let batch_id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut credits = HashMap::new();
    for _ in 0..3 {
        worker::step(&state, &mut credits).await.unwrap();
    }
    assert_eq!(
        outbox(&state).await,
        vec![("batch.completed".into(), "pending".into(), 0)]
    );
    assert_eq!(
        crate::webhooks::deliver_due(&state, &client).await.unwrap(),
        1
    );

    let (headers, body) = sink.got.lock().unwrap()[0].clone();
    let event: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(event["event"], "batch.completed");
    assert_eq!(event["data"]["batch_id"], batch_id.as_str());
    assert_eq!(event["data"]["counts"]["sent"], 2);
    assert_eq!(headers["x-batchmail-event"], "batch.completed");
    assert_eq!(
        headers["x-batchmail-event-id"],
        event["id"].as_str().unwrap()
    );
    let sig = &headers["x-batchmail-signature"];
    let (t, v1) = sig.strip_prefix("t=").unwrap().split_once(",v1=").unwrap();
    assert_eq!(
        v1,
        crate::webhooks::sign(&secret, t.parse().unwrap(), &body)
    );
    assert_eq!(outbox(&state).await[0].1, "delivered");
    // Completed once: more worker ticks add nothing.
    worker::step(&state, &mut credits).await.unwrap();
    assert_eq!(outbox(&state).await.len(), 1);

    // A bounce -> recipient.bounced, once; the retried bounce adds nothing.
    let mid: String =
        sqlx::query_scalar("SELECT message_id FROM recipients WHERE email = 'a@contoh.com'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    let bounce =
        json!({"type":"hard_bounce","message_id":mid,"code":"5.1.1","reason":"User unknown"})
            .to_string();
    for _ in 0..2 {
        let (status, _, _) = send(
            &router,
            hook("POST", "/hooks/bounce", Some("test-bounce-token"), &bounce),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(
        crate::webhooks::deliver_due(&state, &client).await.unwrap(),
        1
    );
    let (_, body) = sink.got.lock().unwrap()[1].clone();
    let event: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(event["event"], "recipient.bounced");
    assert_eq!(event["data"]["email"], "a@contoh.com");
    assert_eq!(event["data"]["code"], "5.1.1");
    assert_eq!(event["data"]["suppressed"], true);
    assert_eq!(
        crate::webhooks::deliver_due(&state, &client).await.unwrap(),
        0
    );

    // Receiver down: the event stays queued with a backoff, then arrives (same event id).
    sink.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    crate::webhooks::enqueue_test(&state, key.id).await.unwrap();
    assert_eq!(
        crate::webhooks::deliver_due(&state, &client).await.unwrap(),
        1
    );
    let rows = outbox(&state).await;
    assert_eq!(rows[2], ("webhook.test".into(), "pending".into(), 1));
    let err: String = sqlx::query_scalar("SELECT last_error FROM webhook_deliveries WHERE id = 3")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(err.contains("500"), "{err}");
    assert_eq!(
        crate::webhooks::deliver_due(&state, &client).await.unwrap(),
        0,
        "backoff not over"
    );
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = 0 WHERE id = 3")
        .execute(&state.db)
        .await
        .unwrap();
    sink.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        crate::webhooks::deliver_due(&state, &client).await.unwrap(),
        1
    );
    assert_eq!(
        outbox(&state).await[2],
        ("webhook.test".into(), "delivered".into(), 2)
    );
    let got = sink.got.lock().unwrap().clone();
    assert_eq!(got.len(), 4);
    assert_eq!(
        got[2].0["x-batchmail-event-id"],
        got[3].0["x-batchmail-event-id"]
    );

    // Giving up after the last attempt.
    crate::webhooks::enqueue_test(&state, key.id).await.unwrap();
    sink.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..6 {
        sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = 0 WHERE status = 'pending'")
            .execute(&state.db)
            .await
            .unwrap();
        crate::webhooks::deliver_due(&state, &client).await.unwrap();
    }
    assert_eq!(
        outbox(&state).await[3],
        ("webhook.test".into(), "failed".into(), 6)
    );

    // Turning the webhook off drops what is still queued and queues nothing new.
    sink.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    crate::webhooks::enqueue_test(&state, key.id).await.unwrap();
    crate::webhooks::set_url(&state, key.id, None)
        .await
        .unwrap();
    crate::webhooks::deliver_due(&state, &client).await.unwrap();
    assert_eq!(outbox(&state).await[4].1, "dropped");
    let (_, _, body) = send(
        &router,
        api(
            "POST",
            "/api/v1/batches",
            Some(&key.plaintext),
            Some(batch_body(
                "info@domainanda.id",
                &["c@contoh.com"],
                "marketing",
            )),
        ),
    )
    .await;
    assert!(body.contains("accepted"), "{body}");
    for _ in 0..3 {
        worker::step(&state, &mut credits).await.unwrap();
    }
    assert_eq!(outbox(&state).await.len(), 5);
}

#[tokio::test]
async fn webhook_settings_in_the_dashboard() {
    let (state, router) = setup().await;
    auth::upsert_user(&state.db, "admin@domainanda.id", "rahasia-sekali")
        .await
        .unwrap();
    let (_, headers, _) = send(
        &router,
        Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "email=admin%40domainanda.id&password=rahasia-sekali&remember=1",
            ))
            .unwrap(),
    )
    .await;
    let cookie = headers["set-cookie"].split(';').next().unwrap().to_string();
    let form = |path: &str, body: &str| {
        Request::post(path)
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // A bad URL is refused before any key is created.
    let (status, _, body) = send(
        &router,
        form(
            "/api-keys",
            "name=HR&mode=test&webhook_url=ftp%3A%2F%2Fx.id",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("harus diawali http"), "{body}");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);

    // A good URL at creation time.
    let (status, _, body) = send(
        &router,
        form(
            "/api-keys",
            "name=HR&mode=test&webhook_url=https%3A%2F%2Fhr.example.id%2Fhooks%2Fbm",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("whsec_"), "secret is shown to the admin");
    assert!(body.contains("https://hr.example.id/hooks/bm"));
    let id: i64 = sqlx::query_scalar("SELECT id FROM api_keys")
        .fetch_one(&state.db)
        .await
        .unwrap();

    // Test event, then change and clear the URL.
    let (status, headers, _) =
        send(&router, form(&format!("/api-keys/{id}/webhook/test"), "")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{headers:?}");
    assert_eq!(outbox(&state).await.len(), 1);
    let (_, headers, _) = send(
        &router,
        form(
            &format!("/api-keys/{id}/webhook"),
            "webhook_url=http%3A%2F%2Flocalhost%3A9%2Fx",
        ),
    )
    .await;
    assert!(headers["location"].contains("webhook_saved"));
    let (_, headers, _) = send(
        &router,
        form(&format!("/api-keys/{id}/webhook"), "webhook_url="),
    )
    .await;
    assert!(headers["location"].contains("webhook_off"));
    let (_, _, body) = send(&router, form(&format!("/api-keys/{id}/webhook/test"), "")).await;
    assert!(body.contains("Simpan URL webhook dulu"), "{body}");
}
