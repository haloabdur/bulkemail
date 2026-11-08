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
