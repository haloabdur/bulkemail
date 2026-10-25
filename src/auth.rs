//! Dashboard users and sessions, and API keys.

use crate::AppState;
use crate::error::ApiError;
use crate::util::{now, random_hex, sha256_hex};
use anyhow::{Result, anyhow, bail};
use argon2::password_hash::{PasswordHash, SaltString, rand_core::OsRng};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use sqlx::FromRow;

pub const SESSION_COOKIE: &str = "bm_session";
const SESSION_DAYS: i64 = 30;

static DUMMY_HASH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| hash_password("dummy-password").unwrap_or_default());

// ---------- users ----------

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("hash password gagal: {e}"))
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

pub async fn upsert_user(db: &sqlx::SqlitePool, email: &str, password: &str) -> Result<()> {
    if password.len() < 10 {
        bail!("Password minimal 10 karakter");
    }
    let hash = hash_password(password)?;
    sqlx::query(
        "INSERT INTO users (email, password_hash, created_at) VALUES (?, ?, ?)
         ON CONFLICT(email) DO UPDATE SET password_hash = excluded.password_hash",
    )
    .bind(email.trim().to_ascii_lowercase())
    .bind(hash)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Clone, Debug, FromRow)]
pub struct User {
    pub id: i64,
    pub email: String,
}

/// Checks the password and returns a new session token for the cookie.
pub async fn login(db: &sqlx::SqlitePool, email: &str, password: &str) -> Result<Option<String>> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE email = ?")
            .bind(email.trim().to_ascii_lowercase())
            .fetch_optional(db)
            .await?;
    let Some((user_id, hash)) = row else {
        // Same work as a real check, so timing does not reveal which emails exist.
        let _ = verify_password(password, &DUMMY_HASH);
        return Ok(None);
    };
    if !verify_password(password, &hash) {
        return Ok(None);
    }
    let token = random_hex(32);
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(sha256_hex(&token))
        .bind(user_id)
        .bind(now() + SESSION_DAYS * 86_400)
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(now())
        .execute(db)
        .await?;
    Ok(Some(token))
}

pub async fn logout(db: &sqlx::SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(sha256_hex(token))
        .execute(db)
        .await?;
    Ok(())
}

pub fn session_cookie(state: &AppState, token: String, remember: bool) -> Cookie<'static> {
    let mut c = Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.cfg.public_base_url.starts_with("https://"));
    if remember {
        c = c.max_age(cookie::time::Duration::days(SESSION_DAYS));
    }
    c.build()
}

/// Extractor for dashboard pages: a logged-in user, or a redirect to /login.
impl FromRequestParts<AppState> for User {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let Some(token) = jar.get(SESSION_COOKIE).map(|c| c.value().to_string()) else {
            return Err(Redirect::to("/login").into_response());
        };
        let user: Option<User> = sqlx::query_as(
            "SELECT u.id, u.email FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ? AND s.expires_at > ?",
        )
        .bind(sha256_hex(&token))
        .bind(now())
        .fetch_optional(&state.db)
        .await
        .map_err(|e| crate::error::AppError::from(e).into_response())?;
        user.ok_or_else(|| Redirect::to("/login").into_response())
    }
}

// ---------- API keys ----------

#[derive(Clone, Debug, FromRow)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub display: String,
    pub test_mode: bool,
    pub domain_id: Option<i64>,
    pub daily_limit: Option<i64>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

pub struct NewKey {
    pub id: i64,
    /// Shown to the user once; only its hash is stored.
    pub plaintext: String,
}

pub async fn create_key(
    db: &sqlx::SqlitePool,
    name: &str,
    test_mode: bool,
    domain_id: Option<i64>,
    daily_limit: Option<i64>,
) -> Result<NewKey> {
    let name = name.trim();
    if name.is_empty() {
        bail!("Nama key wajib diisi");
    }
    let plaintext = format!(
        "bk_{}_{}",
        if test_mode { "test" } else { "live" },
        random_hex(16)
    );
    let display = format!("{}…{}", &plaintext[..12], &plaintext[plaintext.len() - 4..]);
    let id = sqlx::query_scalar(
        "INSERT INTO api_keys (name, display, key_hash, test_mode, domain_id, daily_limit, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(display)
    .bind(sha256_hex(&plaintext))
    .bind(test_mode)
    .bind(domain_id)
    .bind(daily_limit)
    .bind(now())
    .fetch_one(db)
    .await?;
    Ok(NewKey { id, plaintext })
}

pub async fn revoke_key(db: &sqlx::SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE api_keys SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Extractor for the JSON API: `Authorization: Bearer bk_live_…`.
impl FromRequestParts<AppState> for ApiKey {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .unwrap_or("")
            .trim();
        if !token.starts_with("bk_") {
            return Err(ApiError::unauthorized());
        }
        let key: Option<ApiKey> =
            sqlx::query_as("SELECT * FROM api_keys WHERE key_hash = ? AND revoked_at IS NULL")
                .bind(sha256_hex(token))
                .fetch_optional(&state.db)
                .await?;
        let key = key.ok_or_else(ApiError::unauthorized)?;
        sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE id = ?")
            .bind(now())
            .bind(key.id)
            .execute(&state.db)
            .await?;
        Ok(key)
    }
}
