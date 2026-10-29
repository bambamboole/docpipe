//! The service's settings, read from the environment in one place.
//!
//! [`Settings::from_env`] reads every variable the README's configuration
//! table lists. A value that cannot be used — a port that is not a number,
//! zero concurrent analyses, an OCR model directory without its models — is a
//! startup error, never a silent default: the service refuses to start rather
//! than run with a configuration nobody meant.
//!
//! `AUTH_MODE` and `SENTRY_*` are parsed by `Auth::from_vars` and
//! `SentrySettings::from_vars`, whose errors are wrapped here; no module
//! but this one reads the environment.

use std::path::{Path, PathBuf};
use std::thread::available_parallelism;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::auth::Auth;
use crate::http::Config;
use crate::ocr::OcrSettings;
use crate::telemetry::SentrySettings;

/// The port the service listens on when `PORT` is unset.
const DEFAULT_PORT: u16 = 8080;

/// The largest accepted document, in MiB, when `DOCPIPE_MAX_BODY_MB` is unset.
const DEFAULT_MAX_BODY_MB: usize = 50;

/// How long a request may wait for a slot and its analysis when
/// `DOCPIPE_TIMEOUT_SECS` is unset.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);

/// The files pdf-inspector expects in `DOCPIPE_OCR_MODEL_DIR`: the detection
/// and recognition models of PP-OCRv6 Small and the character dictionary, as
/// the Dockerfile downloads them.
const OCR_MODEL_FILES: [&str; 3] = ["pp-ocrv6_small_det.onnx", "pp-ocrv6_small_rec.onnx", "ppocrv6_dict.txt"];

/// Why the service cannot start with the environment it was given.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A variable holds a value that cannot be used.
    #[error("invalid {name}={value:?}: {reason}")]
    Invalid {
        /// The variable's name.
        name: &'static str,
        /// What it holds.
        value: String,
        /// Why that cannot be used.
        reason: String,
    },
    /// A file or directory a variable points to does not exist.
    #[error("{name} points to {path}, which does not exist{detail}")]
    MissingPath {
        /// The variable's name.
        name: &'static str,
        /// The path that is missing.
        path: PathBuf,
        /// What should have been there, or empty.
        detail: String,
    },
    /// The authentication settings are incomplete or contradictory.
    #[error("{0}")]
    Auth(String),
    /// The error reporting settings cannot be used.
    #[error("{0}")]
    Sentry(String),
}

/// Everything the service reads from its environment.
#[derive(Debug)]
pub struct Settings {
    /// The port to listen on, from `PORT`.
    pub port: u16,
    /// Where to report errors; `None` without `SENTRY_DSN`.
    pub sentry: Option<SentrySettings>,
    /// What the HTTP API needs.
    pub http: Config,
}

impl Settings {
    /// The settings from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// The settings from `lookup`. Separate from [`Self::from_env`] so tests
    /// can pass a map.
    pub fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let auth = Auth::from_vars(&lookup).map_err(ConfigError::Auth)?;
        let max_concurrent = parse_or(&lookup, "DOCPIPE_MAX_CONCURRENT", || {
            available_parallelism().map_or(1, std::num::NonZero::get)
        })?;
        let max_queued = parse_or(&lookup, "DOCPIPE_MAX_QUEUED", || max_concurrent.saturating_mul(2))?;
        let max_body_mb = parse_or(&lookup, "DOCPIPE_MAX_BODY_MB", || DEFAULT_MAX_BODY_MB)?;
        let timeout_secs = parse_or(&lookup, "DOCPIPE_TIMEOUT_SECS", || DEFAULT_TIMEOUT.as_secs())?;

        at_least_one("DOCPIPE_MAX_CONCURRENT", max_concurrent)?;
        at_least_one("DOCPIPE_MAX_BODY_MB", max_body_mb)?;
        at_least_one("DOCPIPE_TIMEOUT_SECS", timeout_secs)?;

        // The sums and products below feed semaphores and the body limit; an
        // absurd value is a mistake to report, not something to wrap around.
        if max_concurrent
            .checked_add(max_queued)
            .is_none_or(|in_flight| in_flight > Semaphore::MAX_PERMITS)
        {
            return Err(invalid(
                "DOCPIPE_MAX_QUEUED",
                max_queued,
                format!(
                    "with DOCPIPE_MAX_CONCURRENT it exceeds {} requests in flight",
                    Semaphore::MAX_PERMITS
                ),
            ));
        }
        let max_body_bytes = max_body_mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| invalid("DOCPIPE_MAX_BODY_MB", max_body_mb, "too large for this machine"))?;

        let ocr = OcrSettings {
            enabled: ocr_enabled(&lookup)?,
            model_directory: path(&lookup, "DOCPIPE_OCR_MODEL_DIR"),
        };
        if ocr.enabled {
            check_ocr_assets(&lookup, ocr.model_directory.as_deref())?;
        }

        Ok(Self {
            port: port(&lookup)?,
            sentry: SentrySettings::from_vars(&lookup).map_err(ConfigError::Sentry)?,
            http: Config {
                auth,
                max_body_bytes,
                ocr,
                max_concurrent,
                max_queued,
                timeout: Duration::from_secs(timeout_secs),
            },
        })
    }
}

/// The port from `PORT` in the process environment, for `docpipe health`,
/// which needs nothing else.
pub fn port_from_env() -> Result<u16, ConfigError> {
    port(|name| std::env::var(name).ok())
}

fn port(lookup: impl Fn(&str) -> Option<String>) -> Result<u16, ConfigError> {
    let port = parse_or(lookup, "PORT", || DEFAULT_PORT)?;
    // Port 0 would bind an ephemeral port that `docpipe health` cannot know.
    at_least_one("PORT", port)?;
    Ok(port)
}

fn invalid(name: &'static str, value: impl std::fmt::Display, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        value: value.to_string(),
        reason: reason.into(),
    }
}

/// A trimmed, non-empty value of `name`; a blank value counts as unset.
fn var(lookup: impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The value of `name` parsed as `T`, or `default` when unset.
fn parse_or<T>(
    lookup: impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: impl FnOnce() -> T,
) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match var(lookup, name) {
        Some(value) => value.parse().map_err(|error: T::Err| ConfigError::Invalid {
            name,
            value,
            reason: error.to_string(),
        }),
        None => Ok(default()),
    }
}

fn at_least_one<T>(name: &'static str, value: T) -> Result<(), ConfigError>
where
    T: Copy + PartialEq + Default + std::fmt::Display,
{
    if value == T::default() {
        return Err(invalid(name, value, "must be at least 1"));
    }

    Ok(())
}

fn path(lookup: impl Fn(&str) -> Option<String>, name: &str) -> Option<PathBuf> {
    var(lookup, name).map(PathBuf::from)
}

/// `DOCPIPE_OCR`: `on` (the default) or `off`.
fn ocr_enabled(lookup: impl Fn(&str) -> Option<String>) -> Result<bool, ConfigError> {
    match var(lookup, "DOCPIPE_OCR") {
        None => Ok(true),
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err(ConfigError::Invalid {
                name: "DOCPIPE_OCR",
                value,
                reason: "use on or off".to_owned(),
            }),
        },
    }
}

/// Fails when the OCR runtime cannot work: a model directory without its
/// models, or a library path that points nowhere. Checked at startup so a
/// misconfigured image does not answer `ocr.status: "failed"` per request.
fn check_ocr_assets(
    lookup: impl Fn(&str) -> Option<String>,
    model_directory: Option<&Path>,
) -> Result<(), ConfigError> {
    if let Some(directory) = model_directory {
        if !directory.is_dir() {
            return Err(ConfigError::MissingPath {
                name: "DOCPIPE_OCR_MODEL_DIR",
                path: directory.to_owned(),
                detail: String::new(),
            });
        }

        for file in OCR_MODEL_FILES {
            if !directory.join(file).is_file() {
                return Err(ConfigError::MissingPath {
                    name: "DOCPIPE_OCR_MODEL_DIR",
                    path: directory.to_owned(),
                    detail: format!(" as a complete model directory: {file} is missing"),
                });
            }
        }
    }

    for name in ["PDFIUM_LIB_PATH", "ORT_DYLIB_PATH"] {
        if let Some(library) = path(&lookup, name)
            && !library.is_file()
        {
            return Err(ConfigError::MissingPath {
                name,
                path: library,
                detail: String::new(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Settings from `vars`, with `AUTH_MODE=none` unless `vars` names one.
    fn settings(vars: &[(&str, &str)]) -> Result<Settings, ConfigError> {
        Settings::from_vars(|name| {
            vars.iter()
                .chain([("AUTH_MODE", "none")].iter())
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn wraps_auth_errors() {
        assert!(matches!(settings(&[("AUTH_MODE", "basic")]), Err(ConfigError::Auth(_))));
        assert!(settings(&[("AUTH_MODE", "api_key"), ("API_KEY", "s")]).is_ok());
    }

    /// A fresh directory under the system's temporary directory.
    fn temp_dir(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("docpipe-config-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn applies_the_defaults() {
        let settings = settings(&[]).unwrap();

        assert_eq!(settings.port, 8080);
        assert!(settings.sentry.is_none());
        assert_eq!(settings.http.max_body_bytes, 50 * 1024 * 1024);
        assert!(settings.http.max_concurrent >= 1);
        assert_eq!(settings.http.max_queued, settings.http.max_concurrent * 2);
        assert_eq!(settings.http.timeout, Duration::from_mins(5));
        assert!(settings.http.ocr.enabled);
        assert_eq!(settings.http.ocr.model_directory, None);
    }

    #[test]
    fn reads_every_variable() {
        let settings = settings(&[
            ("PORT", "9000"),
            ("DOCPIPE_MAX_BODY_MB", "8"),
            ("DOCPIPE_MAX_CONCURRENT", "3"),
            ("DOCPIPE_MAX_QUEUED", "1"),
            ("DOCPIPE_TIMEOUT_SECS", "20"),
            ("DOCPIPE_OCR", "off"),
            ("SENTRY_DSN", "https://public@errors.example.test/1"),
        ])
        .unwrap();

        assert_eq!(settings.port, 9000);
        assert_eq!(settings.http.max_body_bytes, 8 * 1024 * 1024);
        assert_eq!(settings.http.max_concurrent, 3);
        assert_eq!(settings.http.max_queued, 1);
        assert_eq!(settings.http.timeout, Duration::from_secs(20));
        assert!(!settings.http.ocr.enabled);
        assert!(settings.sentry.is_some());
    }

    #[test]
    fn rejects_values_that_are_not_numbers() {
        for (name, value) in [
            ("PORT", "abc"),
            ("PORT", "70000"),
            ("DOCPIPE_MAX_BODY_MB", "x"),
            ("DOCPIPE_MAX_CONCURRENT", "-1"),
            ("DOCPIPE_MAX_QUEUED", "two"),
            ("DOCPIPE_TIMEOUT_SECS", "1.5"),
        ] {
            let error = settings(&[(name, value)]).unwrap_err();

            assert!(
                matches!(&error, ConfigError::Invalid { name: invalid, .. } if *invalid == name),
                "{name}={value}: {error}"
            );
        }
    }

    #[test]
    fn rejects_zero_where_the_service_could_not_work() {
        for name in [
            "PORT",
            "DOCPIPE_MAX_CONCURRENT",
            "DOCPIPE_MAX_BODY_MB",
            "DOCPIPE_TIMEOUT_SECS",
        ] {
            assert!(settings(&[(name, "0")]).is_err(), "{name}=0");
        }

        // A queue of zero sheds every request beyond the running ones.
        assert_eq!(settings(&[("DOCPIPE_MAX_QUEUED", "0")]).unwrap().http.max_queued, 0);
    }

    #[test]
    fn treats_blank_values_as_unset_and_rejects_unknown_ocr_modes() {
        assert_eq!(settings(&[("PORT", "  ")]).unwrap().port, 8080);
        assert!(settings(&[("DOCPIPE_OCR", "ON")]).unwrap().http.ocr.enabled);
        assert!(!settings(&[("DOCPIPE_OCR", "off")]).unwrap().http.ocr.enabled);
        assert!(settings(&[("DOCPIPE_OCR", "false")]).is_err());
    }

    #[test]
    fn rejects_sizes_that_would_overflow() {
        let huge = usize::MAX.to_string();

        for (name, value) in [
            ("DOCPIPE_MAX_BODY_MB", huge.as_str()),
            ("DOCPIPE_MAX_QUEUED", huge.as_str()),
            ("DOCPIPE_MAX_CONCURRENT", huge.as_str()),
            ("DOCPIPE_MAX_CONCURRENT", "4611686018427387904"),
        ] {
            assert!(settings(&[(name, value)]).is_err(), "{name}={value}");
        }
    }

    #[test]
    fn wraps_sentry_errors() {
        assert!(matches!(
            settings(&[("SENTRY_DSN", "not a dsn")]),
            Err(ConfigError::Sentry(_))
        ));
    }

    #[test]
    fn requires_the_ocr_models_and_libraries_to_exist() {
        let directory = temp_dir("models");
        let model_dir = directory.to_str().unwrap();

        let incomplete = settings(&[("DOCPIPE_OCR_MODEL_DIR", model_dir)]);
        assert!(
            matches!(
                &incomplete,
                Err(ConfigError::MissingPath {
                    name: "DOCPIPE_OCR_MODEL_DIR",
                    ..
                })
            ),
            "{incomplete:?}"
        );

        for file in OCR_MODEL_FILES {
            fs::write(directory.join(file), b"").unwrap();
        }
        assert!(settings(&[("DOCPIPE_OCR_MODEL_DIR", model_dir)]).is_ok());

        let missing = directory.join("libpdfium.so");
        let missing = missing.to_str().unwrap();
        assert!(matches!(
            settings(&[("DOCPIPE_OCR_MODEL_DIR", model_dir), ("PDFIUM_LIB_PATH", missing)]),
            Err(ConfigError::MissingPath {
                name: "PDFIUM_LIB_PATH",
                ..
            })
        ));
        assert!(matches!(
            settings(&[("ORT_DYLIB_PATH", missing)]),
            Err(ConfigError::MissingPath {
                name: "ORT_DYLIB_PATH",
                ..
            })
        ));

        // With OCR off, the assets are not needed.
        assert!(settings(&[("DOCPIPE_OCR", "off"), ("ORT_DYLIB_PATH", missing)]).is_ok());

        let _ = fs::remove_dir_all(&directory);
    }
}
