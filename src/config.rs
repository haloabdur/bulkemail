use anyhow::{Context, Result, bail};
use std::env;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpTls {
    /// Port 587, upgrade with STARTTLS (Stalwart's submission listener).
    StartTls,
    /// Port 465, implicit TLS.
    Tls,
    /// Plain text. Only for a Stalwart on localhost / a private network.
    None,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    /// Public URL of this app, used in unsubscribe links. No trailing slash.
    pub public_base_url: String,
    /// Secret used to sign unsubscribe links.
    pub secret: String,

    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: SmtpTls,
    pub smtp_user: Option<String>,
    pub smtp_pass: Option<String>,

    /// Stalwart management API, e.g. https://mail.domainanda.id/api
    pub stalwart_api_url: Option<String>,
    /// Stalwart API key (Bearer) or "user:password" (Basic).
    pub stalwart_api_token: Option<String>,
    /// Public hostname of the Stalwart server, used in the SPF record (`a:<host>`).
    pub mail_hostname: String,
    /// Optional default Cloudflare API token (Zone › DNS › Edit) for automatic DNS setup.
    pub cloudflare_api_token: Option<String>,

    /// Allow sending from domains whose SPF/DKIM are not verified yet (dev only).
    pub allow_unverified_domains: bool,
    pub default_rate_per_minute: i64,
    pub max_recipients_api: usize,
    pub max_recipients_dashboard: usize,
    /// Offset used when showing times in the dashboard (WIB = 7).
    pub tz_offset_hours: i32,

    pub admin_email: Option<String>,
    pub admin_password: Option<String>,
}

fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match var(name) {
        Some(v) => v
            .parse()
            .map_err(|_| anyhow::anyhow!("{name} tidak valid: {v}")),
        None => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let smtp_tls = match var("SMTP_TLS").as_deref().unwrap_or("starttls") {
            "starttls" => SmtpTls::StartTls,
            "tls" => SmtpTls::Tls,
            "none" => SmtpTls::None,
            other => bail!("SMTP_TLS harus starttls, tls atau none (bukan {other})"),
        };
        let default_port = match smtp_tls {
            SmtpTls::StartTls => 587,
            SmtpTls::Tls => 465,
            SmtpTls::None => 25,
        };
        let secret =
            var("APP_SECRET").context("APP_SECRET wajib diisi (minimal 32 karakter acak)")?;
        if secret.len() < 32 {
            bail!("APP_SECRET minimal 32 karakter");
        }
        Ok(Config {
            bind_addr: var("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()),
            database_url: var("DATABASE_URL")
                .unwrap_or_else(|| "sqlite://data/batchmail.db".into()),
            public_base_url: var("PUBLIC_BASE_URL")
                .unwrap_or_else(|| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_string(),
            secret,
            smtp_host: var("SMTP_HOST").context("SMTP_HOST wajib diisi (host Stalwart)")?,
            smtp_port: parse("SMTP_PORT", default_port)?,
            smtp_tls,
            smtp_user: var("SMTP_USER"),
            smtp_pass: var("SMTP_PASS"),
            stalwart_api_url: var("STALWART_API_URL").map(|u| u.trim_end_matches('/').to_string()),
            stalwart_api_token: var("STALWART_API_TOKEN"),
            mail_hostname: var("MAIL_HOSTNAME")
                .or_else(|| var("SMTP_HOST"))
                .unwrap_or_default()
                .trim_end_matches('.')
                .to_ascii_lowercase(),
            cloudflare_api_token: var("CLOUDFLARE_API_TOKEN"),
            allow_unverified_domains: parse("ALLOW_UNVERIFIED_DOMAINS", false)?,
            default_rate_per_minute: parse("DEFAULT_RATE_PER_MINUTE", 400)?,
            max_recipients_api: parse("MAX_RECIPIENTS_API", 10_000)?,
            max_recipients_dashboard: parse("MAX_RECIPIENTS_DASHBOARD", 100_000)?,
            tz_offset_hours: parse("TZ_OFFSET_HOURS", 7)?,
            admin_email: var("ADMIN_EMAIL"),
            admin_password: var("ADMIN_PASSWORD"),
        })
    }

    /// Config for tests: no real SMTP, in-memory database.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Config {
            bind_addr: "127.0.0.1:0".into(),
            database_url: "sqlite::memory:".into(),
            public_base_url: "http://localhost:8080".into(),
            secret: "test-secret-test-secret-test-secret!".into(),
            smtp_host: "localhost".into(),
            smtp_port: 2525,
            smtp_tls: SmtpTls::None,
            smtp_user: None,
            smtp_pass: None,
            stalwart_api_url: None,
            stalwart_api_token: None,
            mail_hostname: "mail.domainanda.id".into(),
            cloudflare_api_token: None,
            allow_unverified_domains: false,
            default_rate_per_minute: 400,
            max_recipients_api: 10_000,
            max_recipients_dashboard: 100_000,
            tz_offset_hours: 7,
            admin_email: None,
            admin_password: None,
        }
    }
}
