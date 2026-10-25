//! Submits messages to Stalwart over SMTP (submission port, authenticated).
//! Stalwart then signs them with DKIM and handles delivery, queueing and retries
//! towards the recipient's mail server.

use crate::config::{Config, SmtpTls};
use anyhow::{Context, Result};
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

#[derive(Clone)]
struct ListUnsubscribe(String);

impl Header for ListUnsubscribe {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("List-Unsubscribe")
    }
    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(s.to_string()))
    }
    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), self.0.clone())
    }
}

/// RFC 8058 one-click unsubscribe, required by Gmail and Yahoo for bulk senders.
#[derive(Clone)]
struct ListUnsubscribePost;

impl Header for ListUnsubscribePost {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("List-Unsubscribe-Post")
    }
    fn parse(_: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self)
    }
    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), "List-Unsubscribe=One-Click".to_string())
    }
}

pub struct OutgoingEmail<'a> {
    pub from: &'a Mailbox,
    pub reply_to: Option<&'a Mailbox>,
    pub to: &'a str,
    pub subject: &'a str,
    pub html: Option<&'a str>,
    pub text: Option<&'a str>,
    pub message_id: &'a str,
    pub unsubscribe_url: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Stalwart accepted the message for delivery (2xx).
    Accepted(String),
    /// Temporary rejection (4xx): retry later.
    Transient(String),
    /// Permanent rejection (5xx) or an invalid message.
    Permanent(String),
    /// Could not talk to Stalwart at all (connection, TLS, auth). Nothing was sent.
    Unavailable(String),
}

pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    pub description: String,
}

impl Mailer {
    pub fn new(cfg: &Config) -> Result<Self> {
        let builder = match cfg.smtp_tls {
            SmtpTls::StartTls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
                    .context("host SMTP tidak valid")?
            }
            SmtpTls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)
                .context("host SMTP tidak valid")?,
            SmtpTls::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp_host)
            }
        };
        let mut builder = builder.port(cfg.smtp_port);
        if let (Some(user), Some(pass)) = (&cfg.smtp_user, &cfg.smtp_pass) {
            builder = builder.credentials(Credentials::new(user.clone(), pass.clone()));
        }
        let tls = match cfg.smtp_tls {
            SmtpTls::StartTls => "STARTTLS",
            SmtpTls::Tls => "TLS",
            SmtpTls::None => "tanpa TLS",
        };
        Ok(Mailer {
            transport: builder.build(),
            description: format!("{}:{} · {}", cfg.smtp_host, cfg.smtp_port, tls),
        })
    }

    pub async fn test_connection(&self) -> Result<(), String> {
        match self.transport.test_connection().await {
            Ok(true) => Ok(()),
            Ok(false) => Err("Server tidak merespons dengan benar".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub async fn send(&self, email: &OutgoingEmail<'_>) -> Outcome {
        let message = match build_message(email) {
            Ok(m) => m,
            Err(e) => return Outcome::Permanent(format!("pesan tidak valid: {e}")),
        };
        match self.transport.send(message).await {
            Ok(resp) => {
                let text: Vec<&str> = resp.message().collect();
                Outcome::Accepted(
                    format!("{} {}", resp.code(), text.join(" "))
                        .trim()
                        .to_string(),
                )
            }
            Err(e) => match e.status() {
                Some(code) => {
                    let text = format!("{code} {e}");
                    if code.to_string().starts_with('4') {
                        Outcome::Transient(text)
                    } else {
                        Outcome::Permanent(text)
                    }
                }
                None => Outcome::Unavailable(e.to_string()),
            },
        }
    }
}

pub fn build_message(email: &OutgoingEmail<'_>) -> Result<Message> {
    let mut b = Message::builder()
        .from(email.from.clone())
        .to(email.to.parse().context("alamat penerima tidak valid")?)
        .subject(email.subject)
        .message_id(Some(email.message_id.to_string()));
    if let Some(r) = email.reply_to {
        b = b.reply_to(r.clone());
    }
    if let Some(url) = email.unsubscribe_url {
        b = b
            .header(ListUnsubscribe(format!("<{url}>")))
            .header(ListUnsubscribePost);
    }
    let message = match (email.html, email.text) {
        (Some(html), Some(text)) => b.multipart(MultiPart::alternative_plain_html(
            text.to_string(),
            html.to_string(),
        ))?,
        (Some(html), None) => b.singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(html.to_string()),
        )?,
        (None, Some(text)) => b.singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(text.to_string()),
        )?,
        (None, None) => anyhow::bail!("isi pesan kosong"),
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_message_with_unsubscribe_headers() {
        let from: Mailbox = "Tim Toko <info@domainanda.id>".parse().unwrap();
        let msg = build_message(&OutgoingEmail {
            from: &from,
            reply_to: None,
            to: "andi@contoh.co.id",
            subject: "Halo Andi",
            html: Some("<p>Halo</p>"),
            text: Some("Halo"),
            message_id: "<b_1.1.x@domainanda.id>",
            unsubscribe_url: Some("https://mail.domainanda.id/u/abc"),
        })
        .unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("List-Unsubscribe: <https://mail.domainanda.id/u/abc>"));
        assert!(raw.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"));
        assert!(raw.contains("Message-ID: <b_1.1.x@domainanda.id>"));
        assert!(raw.contains("multipart/alternative"));
    }
}
