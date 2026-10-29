//! The `docpipe` binary: the service, and two subcommands for the image.
//!
//! - `docpipe` reads its [`Settings`] from the environment and serves the API.
//! - `docpipe openapi` prints the `OpenAPI` document; `openapi.json` holds it.
//! - `docpipe health` asks the running instance for `/health` and exits with
//!   0 on `200`, 1 otherwise, for the Docker `HEALTHCHECK` without curl.

use std::io::Write;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use docpipe::auth::Auth;
use docpipe::config::{self, ConfigError, Settings};
use docpipe::http::router;
use docpipe::telemetry;

/// How long `docpipe health` waits for the instance.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the binary stopped before it could do its work.
#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("cannot start the runtime: {0}")]
    Runtime(std::io::Error),
    #[error("cannot listen on {address}: {source}")]
    Bind {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("the server stopped: {0}")]
    Serve(std::io::Error),
    #[error("cannot write the OpenAPI document: {0}")]
    OpenApi(#[from] serde_json::Error),
    #[error("cannot write to stdout: {0}")]
    Stdout(std::io::Error),
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("docpipe: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, StartupError> {
    match std::env::args().nth(1).as_deref() {
        Some("openapi") => {
            let document = docpipe::http::openapi().to_pretty_json()?;
            writeln!(std::io::stdout().lock(), "{document}").map_err(StartupError::Stdout)?;
            return Ok(ExitCode::SUCCESS);
        }
        Some("health") => return Ok(health(config::port_from_env()?)),
        _ => {}
    }

    let settings = Settings::from_env()?;

    // Sentry starts before the runtime, so its hub reaches every worker thread.
    let reports_to_sentry = settings.sentry.is_some();
    let _telemetry = telemetry::init(settings.sentry);
    tracing::info!(sentry = reports_to_sentry, "telemetry configured");

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?
        .block_on(serve(settings.port, settings.http))?;

    Ok(ExitCode::SUCCESS)
}

async fn serve(port: u16, config: docpipe::http::Config) -> Result<(), StartupError> {
    match &config.auth {
        Auth::Public => tracing::warn!("AUTH_MODE is none; /v1 accepts every request"),
        auth => tracing::info!(mode = auth.mode(), "authentication enabled"),
    }

    // The issuer may be down right now; the first request fetches the keys
    // again, so this is a warning and not the end.
    if let Err(error) = config.auth.preload() {
        tracing::warn!(%error, "the issuer's keys could not be preloaded");
    }

    if config.ocr.enabled && config.ocr.model_directory.is_none() {
        tracing::warn!("DOCPIPE_OCR_MODEL_DIR is not set; OCR models are downloaded on first use");
    }

    let app = router(config);
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| StartupError::Bind { address, source })?;

    tracing::info!(address = %listener.local_addr().unwrap_or(address), "docpipe listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(StartupError::Serve)
}

/// `GET /health` of the instance on `port`; `200` is healthy.
fn health(port: u16) -> ExitCode {
    let healthy = ureq::get(format!("http://127.0.0.1:{port}/health"))
        .config()
        .timeout_global(Some(HEALTH_TIMEOUT))
        .build()
        .call()
        .is_ok_and(|response| response.status() == 200);

    if healthy { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

async fn shutdown() {
    let interrupt = tokio::signal::ctrl_c();

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {},
        () = terminate => {},
    }
}
