//! Outbound mail, behind a trait.
//!
//! The trait exists so auth code can be tested without a network. `CaptureMailer`
//! is not a test-only shim bolted on afterwards — it is the reason the seam is
//! here at all: auth code must have tests, and no test may perform real SMTP.
//!
//! Transport is generic SMTP, never a provider SDK. The operator points this at
//! whatever service they use, so changing provider is a config change rather
//! than a code change — a lesson learned the hard way when the original MinIO
//! choice turned out risky: its vendor archived its own open-source server, and
//! Garage speaks the same S3 API without that risk, one layer up.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use lettre::{
    message::{header::ContentType, Mailbox},
    transport::smtp::{
        authentication::Credentials, client::Tls, client::TlsParameters, AsyncSmtpTransport,
    },
    AsyncTransport, Message, Tokio1Executor,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MailError {
    #[error("invalid mail configuration: {0}")]
    Config(String),

    #[error("could not build the message: {0}")]
    Build(String),

    #[error("delivery failed")]
    Delivery(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// One outbound message. Plain text only: a verification code needs no markup,
/// and an HTML part would be one more thing to escape user-controlled values
/// into for no gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Boxed rather than an `async fn` in the trait, so `Arc<dyn Mailer>` works —
/// the whole point is that callers hold a mailer without knowing which one.
pub type MailFuture<'a> = Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

pub trait Mailer: Send + Sync {
    fn send<'a>(&'a self, mail: OutgoingMail) -> MailFuture<'a>;
}

/// How the connection to the relay is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// Connect in the clear, then upgrade with STARTTLS. Port 587's usual
    /// shape, and what most relays expect.
    StartTls,
    /// TLS from the first byte. Port 465.
    ImplicitTls,
    /// No transport security. Only sane for a relay on localhost; anything
    /// else sends credentials in the clear.
    None,
}

impl SmtpSecurity {
    /// Parses the `SMTP_SECURITY` value. Unknown values are an error rather
    /// than a silent downgrade to `None` — quietly disabling TLS because of a
    /// typo is exactly the failure that must not be quiet.
    pub fn parse(value: &str) -> Result<Self, MailError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "starttls" => Ok(Self::StartTls),
            "implicit_tls" | "implicit" | "tls" => Ok(Self::ImplicitTls),
            "none" => Ok(Self::None),
            other => Err(MailError::Config(format!(
                "unknown SMTP security mode '{other}' (expected starttls, implicit_tls, or none)"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub security: SmtpSecurity,
    /// The envelope sender, e.g. `r4chii <no-reply@example.com>`.
    pub from: String,
}

/// Generic SMTP delivery. Pooled, because the alternative is a fresh TCP and
/// TLS handshake per message.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMailer {
    pub fn new(config: SmtpConfig) -> Result<Self, MailError> {
        let from: Mailbox = config
            .from
            .parse()
            .map_err(|err| MailError::Config(format!("invalid SMTP_FROM address: {err}")))?;

        let mut builder =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host).port(config.port);

        builder = match config.security {
            SmtpSecurity::StartTls => {
                let params = TlsParameters::new(config.host.clone())
                    .map_err(|err| MailError::Config(format!("TLS setup failed: {err}")))?;
                builder.tls(Tls::Required(params))
            }
            SmtpSecurity::ImplicitTls => {
                let params = TlsParameters::new(config.host.clone())
                    .map_err(|err| MailError::Config(format!("TLS setup failed: {err}")))?;
                builder.tls(Tls::Wrapper(params))
            }
            SmtpSecurity::None => builder.tls(Tls::None),
        };

        // Credentials are optional: a localhost relay often takes none, and
        // sending an empty username would fail auth rather than skip it.
        if let (Some(username), Some(password)) = (config.username, config.password) {
            builder = builder.credentials(Credentials::new(username, password));
        }

        Ok(Self {
            transport: builder.build(),
            from,
        })
    }
}

impl Mailer for SmtpMailer {
    fn send<'a>(&'a self, mail: OutgoingMail) -> MailFuture<'a> {
        Box::pin(async move {
            let to: Mailbox = mail
                .to
                .parse()
                .map_err(|err| MailError::Build(format!("invalid recipient: {err}")))?;

            let message = Message::builder()
                .from(self.from.clone())
                .to(to)
                .subject(mail.subject)
                .header(ContentType::TEXT_PLAIN)
                .body(mail.body)
                .map_err(|err| MailError::Build(err.to_string()))?;

            self.transport
                .send(message)
                .await
                .map_err(|err| MailError::Delivery(Box::new(err)))?;

            Ok(())
        })
    }
}

/// Keeps every message in memory instead of sending it.
///
/// Used by the whole auth test suite to assert that a verification mail was
/// issued carrying a particular code, without a network. Also the honest
/// choice for a dry-run deployment: it drops mail loudly (the messages are
/// inspectable) rather than pretending to deliver.
#[derive(Debug, Default, Clone)]
pub struct CaptureMailer {
    sent: Arc<Mutex<Vec<OutgoingMail>>>,
}

impl CaptureMailer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every message captured so far, oldest first.
    ///
    /// Returns an empty vec if the lock was poisoned rather than panicking:
    /// a poisoned lock means some other test thread panicked, and turning
    /// that into a second panic here only obscures the original failure.
    pub fn sent(&self) -> Vec<OutgoingMail> {
        self.sent
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The most recent message, if any.
    pub fn last(&self) -> Option<OutgoingMail> {
        self.sent().last().cloned()
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.sent.lock() {
            guard.clear();
        }
    }
}

impl Mailer for CaptureMailer {
    fn send<'a>(&'a self, mail: OutgoingMail) -> MailFuture<'a> {
        Box::pin(async move {
            match self.sent.lock() {
                Ok(mut guard) => {
                    guard.push(mail);
                    Ok(())
                }
                Err(_) => Err(MailError::Delivery(Box::new(std::io::Error::other(
                    "capture mailer lock poisoned",
                )))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(to: &str) -> OutgoingMail {
        OutgoingMail {
            to: to.to_string(),
            subject: "subject".to_string(),
            body: "body".to_string(),
        }
    }

    #[tokio::test]
    async fn capture_mailer_records_messages_in_order() {
        let mailer = CaptureMailer::new();

        mailer.send(mail("a@example.com")).await.expect("captures");
        mailer.send(mail("b@example.com")).await.expect("captures");

        let sent = mailer.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].to, "a@example.com");
        assert_eq!(mailer.last().expect("a last message").to, "b@example.com");
    }

    #[tokio::test]
    async fn capture_mailer_clear_empties_the_log() {
        let mailer = CaptureMailer::new();
        mailer.send(mail("a@example.com")).await.expect("captures");

        mailer.clear();

        assert!(mailer.sent().is_empty());
        assert!(mailer.last().is_none());
    }

    #[test]
    fn smtp_security_parses_known_modes_case_insensitively() {
        assert_eq!(
            SmtpSecurity::parse("STARTTLS").expect("parses"),
            SmtpSecurity::StartTls
        );
        assert_eq!(
            SmtpSecurity::parse(" implicit_tls ").expect("parses"),
            SmtpSecurity::ImplicitTls
        );
        assert_eq!(
            SmtpSecurity::parse("none").expect("parses"),
            SmtpSecurity::None
        );
    }

    #[test]
    fn smtp_security_rejects_unknown_modes_rather_than_downgrading() {
        // The failure mode this guards against: a typo silently turning TLS
        // off and shipping credentials in the clear.
        assert!(SmtpSecurity::parse("ssl-ish").is_err());
        assert!(SmtpSecurity::parse("").is_err());
    }

    #[test]
    fn smtp_mailer_rejects_an_unparseable_from_address() {
        let config = SmtpConfig {
            host: "localhost".to_string(),
            port: 587,
            username: None,
            password: None,
            security: SmtpSecurity::None,
            from: "not a mailbox".to_string(),
        };

        assert!(matches!(SmtpMailer::new(config), Err(MailError::Config(_))));
    }
}
