use std::net::SocketAddr;
use std::path::PathBuf;
use std::thread::available_parallelism;
use std::time::Duration;

use docpipe::auth::Auth;
use docpipe::http::{Config, router};
use docpipe::ocr::OcrSettings;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // lopdf warns about every recoverable defect in a PDF and ONNX Runtime
    // logs every allocation; neither is ours to act on.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,lopdf=error,ort=warn")),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8080);
    let max_body_mb: usize = std::env::var("DOCPIPE_MAX_BODY_MB")
        .ok()
        .and_then(|mb| mb.parse().ok())
        .unwrap_or(50);
    let max_concurrent: usize = std::env::var("DOCPIPE_MAX_CONCURRENT")
        .ok()
        .and_then(|count| count.parse().ok())
        .filter(|&count| count > 0)
        .unwrap_or_else(|| available_parallelism().map_or(1, |count| count.get()));
    let timeout_secs: u64 = std::env::var("DOCPIPE_TIMEOUT_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .unwrap_or(300);
    let auth = Auth::from_env()?;

    match &auth {
        Auth::Public => tracing::warn!("AUTH_MODE is none; /v1 accepts every request"),
        auth => tracing::info!(mode = auth.mode(), "authentication enabled"),
    }

    let ocr = OcrSettings {
        enabled: std::env::var("DOCPIPE_OCR").map_or(true, |mode| mode != "off"),
        model_directory: std::env::var_os("DOCPIPE_OCR_MODEL_DIR").map(PathBuf::from),
    };

    if ocr.enabled && ocr.model_directory.is_none() {
        tracing::warn!("DOCPIPE_OCR_MODEL_DIR is not set; OCR models are downloaded on first use");
    }

    let app = router(Config {
        auth,
        max_body_bytes: max_body_mb * 1024 * 1024,
        ocr,
        max_concurrent,
        timeout: Duration::from_secs(timeout_secs),
    });
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(address).await?;

    tracing::info!(%address, "docpipe listening");

    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;

    Ok(())
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
        _ = terminate => {},
    }
}
