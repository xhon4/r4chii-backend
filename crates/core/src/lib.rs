use std::env;

use thiserror::Error;
pub use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required environment variable: {0}")]
    Missing(&'static str),

    #[error("invalid value for {name}: {reason}")]
    Invalid {
        name: &'static str,
        reason: String,
    },
}

/// Generic SMTP settings. Held as plain strings here because
/// `core` sits below `mailer` in the crate order; the `server` binary turns
/// these into a `mailer::SmtpConfig`.
#[derive(Debug, Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// `starttls` (default), `implicit_tls`, or `none`.
    pub security: String,
    pub from: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    /// `None` when `SMTP_HOST` is unset. Registration then cannot issue codes,
    /// which is why the server refuses to start rather than accepting signups
    /// whose mail silently goes nowhere — see `crates/server`.
    pub smtp: Option<SmtpSettings>,
    /// Directory holding the built web client's static assets
    /// (`index.html` + hashed bundles). `None` when unset — the server then
    /// only exposes the API, no static/SPA fallback (the case for every
    /// existing test and for local `cargo run` without a client build on
    /// hand).
    pub client_dist_dir: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            database_url: env::var("DATABASE_URL")
                .map_err(|_| ConfigError::Missing("DATABASE_URL"))?,
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            smtp: Self::smtp_from_env()?,
            client_dist_dir: env::var("CLIENT_DIST_DIR").ok(),
        })
    }

    /// `SMTP_HOST` is the switch: absent means no mail configured at all.
    /// Once it is set, `SMTP_FROM` is required — a relay with no envelope
    /// sender would fail per-message at send time, and failing at startup
    /// instead means the operator learns about it before any user does.
    fn smtp_from_env() -> Result<Option<SmtpSettings>, ConfigError> {
        let Ok(host) = env::var("SMTP_HOST") else {
            return Ok(None);
        };

        let port = match env::var("SMTP_PORT") {
            Ok(raw) => raw.trim().parse::<u16>().map_err(|err| ConfigError::Invalid {
                name: "SMTP_PORT",
                reason: err.to_string(),
            })?,
            Err(_) => 587,
        };

        Ok(Some(SmtpSettings {
            host,
            port,
            username: env::var("SMTP_USERNAME").ok().filter(|v| !v.is_empty()),
            password: env::var("SMTP_PASSWORD").ok().filter(|v| !v.is_empty()),
            security: env::var("SMTP_SECURITY").unwrap_or_else(|_| "starttls".to_string()),
            from: env::var("SMTP_FROM").map_err(|_| ConfigError::Missing("SMTP_FROM"))?,
        }))
    }
}

pub fn new_id() -> Uuid {
    Uuid::now_v7()
}

/// The resolved identity of an authenticated request. Produced by
/// `auth::AuthService::verify_session` from either the `Authorization: Bearer`
/// header or the session cookie, and
/// consumed by the `api` crate's extractor and, later, `domain` services for
/// authorization checks.
#[derive(Debug, Clone, Copy)]
pub struct AuthContext {
    pub account_id: Uuid,
    pub session_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every case lives in one test function because env vars are
    // process-global state; splitting them into separate #[test] fns races
    // under the default parallel runner — as adding a second one for the SMTP
    // cases promptly proved.
    #[test]
    fn from_env_reads_the_environment() {
        for name in [
            "DATABASE_URL",
            "BIND_ADDR",
            "SMTP_HOST",
            "SMTP_PORT",
            "SMTP_FROM",
            "SMTP_USERNAME",
            "SMTP_PASSWORD",
            "SMTP_SECURITY",
        ] {
            env::remove_var(name);
        }

        let missing = Config::from_env();
        assert!(matches!(missing, Err(ConfigError::Missing("DATABASE_URL"))));

        env::set_var("DATABASE_URL", "postgres://user:pass@localhost/db");

        let config = Config::from_env().expect("DATABASE_URL is set");
        assert_eq!(config.database_url, "postgres://user:pass@localhost/db");
        assert_eq!(config.bind_addr, "0.0.0.0:8080");
        assert!(config.smtp.is_none(), "no SMTP_HOST means no mail configured");

        // Host set but no from-address: refused at startup rather than
        // failing per-message later.
        env::set_var("SMTP_HOST", "smtp.example.com");
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::Missing("SMTP_FROM"))
        ));

        env::set_var("SMTP_FROM", "r4chii <no-reply@example.com>");
        let config = Config::from_env().expect("host and from are set");
        let smtp = config.smtp.expect("smtp configured");
        assert_eq!(smtp.port, 587, "port defaults to submission");
        assert_eq!(smtp.security, "starttls");
        assert!(smtp.username.is_none());

        // A non-numeric port is a hard error, not a silent fallback to 587.
        env::set_var("SMTP_PORT", "not-a-port");
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::Invalid {
                name: "SMTP_PORT",
                ..
            })
        ));

        for name in ["DATABASE_URL", "SMTP_HOST", "SMTP_PORT", "SMTP_FROM"] {
            env::remove_var(name);
        }
    }
}
