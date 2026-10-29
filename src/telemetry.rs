//! Logs and error reports.
//!
//! Logs go to stdout as JSON. With `SENTRY_DSN` set, they also reach a
//! Sentry-compatible server such as Rustrak: `ERROR` events become issues,
//! `INFO` and above become breadcrumbs of those issues, panics are reported,
//! and `SENTRY_LOGS` forwards events from that level on as structured logs.
//! No log line carries document content, and Sentry sends no request data.

use std::borrow::Cow;
use std::str::FromStr;

use sentry::integrations::tracing::{BreadcrumbLayer, ErrorLayer, LogLayer};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// lopdf warns about every recoverable defect in a PDF and ONNX Runtime logs
/// every allocation; neither is ours to act on.
const DEFAULT_FILTER: &str = "info,lopdf=error,ort=warn";

/// Where and what to report to Sentry.
#[derive(Debug)]
pub struct SentrySettings {
    /// The project's DSN.
    pub dsn: sentry::types::Dsn,
    /// The release the reports belong to.
    pub release: String,
    /// The environment the reports belong to.
    pub environment: Option<String>,
    /// The lowest level forwarded as structured logs; `OFF` sends none.
    pub logs: LevelFilter,
}

impl SentrySettings {
    /// The settings from `SENTRY_DSN`, `SENTRY_RELEASE`, `SENTRY_ENVIRONMENT`
    /// and `SENTRY_LOGS`, or `None` without a DSN. [`crate::config`] passes
    /// the environment through `lookup`, so the whole configuration is
    /// testable.
    pub(crate) fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let var = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };

        let Some(dsn) = var("SENTRY_DSN") else {
            return Ok(None);
        };

        Ok(Some(Self {
            dsn: dsn.parse().map_err(|error| format!("invalid SENTRY_DSN: {error}"))?,
            release: var("SENTRY_RELEASE").unwrap_or_else(|| concat!("docpipe@", env!("CARGO_PKG_VERSION")).to_owned()),
            environment: var("SENTRY_ENVIRONMENT"),
            logs: match var("SENTRY_LOGS") {
                Some(level) => LevelFilter::from_str(&level)
                    .map_err(|_| format!("invalid SENTRY_LOGS {level:?}; use off, error, warn, info or debug"))?,
                None => LevelFilter::OFF,
            },
        }))
    }

    /// Which events become logs is up to the log layer's filter, not to the
    /// client options.
    fn client_options(&self) -> sentry::ClientOptions {
        let mut options = sentry::ClientOptions::new().send_default_pii(false);
        options.dsn = Some(self.dsn.clone());
        options.release = Some(Cow::Owned(self.release.clone()));
        options.environment = self.environment.clone().map(Cow::Owned);

        options
    }
}

/// Reports to Sentry until dropped, which flushes what is still queued.
pub struct Telemetry {
    _sentry: Option<sentry::ClientInitGuard>,
}

/// Installs the global subscriber. Call it before the async runtime starts,
/// so Sentry sees every thread.
pub fn init(sentry: Option<SentrySettings>) -> Telemetry {
    let guard = sentry.as_ref().map(|settings| sentry::init(settings.client_options()));
    let sentry_layers = sentry.map(|settings| {
        ErrorLayer::new()
            .with_filter(LevelFilter::ERROR)
            .and_then(BreadcrumbLayer::new().with_filter(LevelFilter::INFO))
            .and_then(LogLayer::new().with_filter(settings.logs))
    });

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER)))
        .with(fmt::layer().json())
        .with(sentry_layers)
        .init();

    Telemetry { _sentry: guard }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSN: &str = "https://public@errors.example.test/1";

    fn settings(vars: &[(&str, &str)]) -> Result<Option<SentrySettings>, String> {
        SentrySettings::from_vars(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn stays_off_without_a_dsn() {
        assert!(settings(&[]).unwrap().is_none());
        assert!(
            settings(&[("SENTRY_DSN", " "), ("SENTRY_LOGS", "info")])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reports_the_crate_version_without_logs_by_default() {
        let settings = settings(&[("SENTRY_DSN", DSN)]).unwrap().unwrap();

        assert_eq!(settings.release, concat!("docpipe@", env!("CARGO_PKG_VERSION")));
        assert_eq!(settings.environment, None);
        assert_eq!(settings.logs, LevelFilter::OFF);
    }

    #[test]
    fn reads_release_environment_and_log_level() {
        let settings = settings(&[
            ("SENTRY_DSN", DSN),
            ("SENTRY_RELEASE", "docpipe@abc123"),
            ("SENTRY_ENVIRONMENT", "production"),
            ("SENTRY_LOGS", "warn"),
        ])
        .unwrap()
        .unwrap();
        let options = settings.client_options();

        assert_eq!(options.release.as_deref(), Some("docpipe@abc123"));
        assert_eq!(options.environment.as_deref(), Some("production"));
        assert_eq!(settings.logs, LevelFilter::WARN);
        assert!(!options.send_default_pii);
    }

    #[test]
    fn rejects_an_invalid_dsn_or_log_level() {
        assert!(settings(&[("SENTRY_DSN", "not a dsn")]).is_err());
        assert!(settings(&[("SENTRY_DSN", DSN), ("SENTRY_LOGS", "loud")]).is_err());
    }
}
