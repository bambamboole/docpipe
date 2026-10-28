//! The HTTP API.
//!
//! `POST /v1/analyze` takes the document as the raw request body and answers
//! with the [`Analysis`](crate::analyze::Analysis) as JSON. Requests carry a
//! bearer token as [`Auth`] demands: a JWT, an API key, or none.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::analyze::{AnalyzeError, analyze};
use crate::auth::{Auth, AuthError};
use crate::ocr::OcrSettings;

#[derive(Debug)]
pub struct Config {
    pub auth: Auth,
    pub max_body_bytes: usize,
    pub ocr: OcrSettings,
    /// Analyses that run at once; further requests wait for a slot.
    pub max_concurrent: usize,
    /// How long a request may wait for a slot and its analysis.
    pub timeout: Duration,
}

struct AppState {
    config: Config,
    analyses: Arc<Semaphore>,
}

pub fn router(config: Config) -> Router {
    let max_body_bytes = config.max_body_bytes;
    let analyses = Arc::new(Semaphore::new(config.max_concurrent));

    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/analyze", post(analyze_document))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(AppState { config, analyses }))
}

async fn analyze_document(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let auth_state = Arc::clone(&state);

    // JWT verification may fetch the issuer's JWKS with a blocking client.
    match tokio::task::spawn_blocking(move || auth_state.config.auth.authorize(authorization.as_deref())).await {
        Ok(Ok(())) => {}
        Ok(Err(AuthError::Jwks(reason))) => {
            tracing::error!(%reason, "JWKS unavailable");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_unavailable",
                "The token issuer's keys cannot be fetched.",
            );
        }
        Ok(Err(reason)) => {
            tracing::info!(%reason, "request rejected");
            return error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "A valid bearer token is required.",
            );
        }
        Err(join_error) => {
            tracing::error!(%join_error, "authorization panicked");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The analysis failed.",
            );
        }
    }

    if body.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "empty_body",
            "Send the document as the request body.",
        );
    }

    // Parsing is CPU-bound and can take a while on large documents. The slot
    // moves into the blocking task, so an analysis that outlives its request
    // still holds it until it ends.
    let ocr = state.config.ocr.clone();
    let analyses = Arc::clone(&state.analyses);
    let analysis = async move {
        let slot = analyses.acquire_owned().await.expect("the semaphore is never closed");

        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            analyze(&body, &ocr)
        })
        .await
    };

    match tokio::time::timeout(state.config.timeout, analysis).await {
        Ok(Ok(Ok(analysis))) => axum::Json(analysis).into_response(),
        Ok(Ok(Err(AnalyzeError::UnsupportedFormat))) => error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_format",
            "Send a PDF or a CII/UBL e-invoice XML.",
        ),
        Ok(Ok(Err(AnalyzeError::UnreadablePdf(reason)))) => {
            tracing::info!(%reason, "unreadable PDF");
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unreadable_pdf",
                "The PDF cannot be read.",
            )
        }
        Err(_) => {
            tracing::warn!(timeout = ?state.config.timeout, "analysis timed out");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "timeout",
                "The analysis did not finish in time.",
            )
        }
        Ok(Err(join_error)) => {
            tracing::error!(%join_error, "analysis panicked");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The analysis failed.",
            )
        }
    }
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, axum::Json(json!({ "error": code, "message": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::attachments::tests::{CII, pdf_with_attachments};
    use crate::auth::tests::{ISSUER, token, verifier};

    fn config(auth: Auth) -> Config {
        Config {
            auth,
            max_body_bytes: 1024 * 1024,
            ocr: OcrSettings::default(),
            max_concurrent: 2,
            timeout: Duration::from_secs(30),
        }
    }

    fn app(auth: Auth) -> Router {
        router(config(auth))
    }

    async fn post(app: Router, body: Vec<u8>, token: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut request = Request::post("/v1/analyze");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }

        let response = app.oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();

        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn analyzes_a_pdf() {
        let (status, json) = post(app(Auth::Public), pdf_with_attachments(&[("factur-x.xml", CII)]), None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["format"], "pdf");
        assert_eq!(json["einvoice"]["syntax"], "cii");
        assert_eq!(json["einvoice"]["filename"], "factur-x.xml");
    }

    #[tokio::test]
    async fn requires_a_jwt_of_an_allowed_client() {
        let pdf = pdf_with_attachments(&[]);
        let jwt = || app(Auth::Jwt(verifier(Some(ISSUER), &["example-client"])));

        assert_eq!(post(jwt(), pdf.clone(), None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            post(jwt(), pdf.clone(), Some(&token(json!({ "client_id": "other-client" }))))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(post(jwt(), pdf, Some(&token(json!({})))).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn requires_the_api_key() {
        let pdf = pdf_with_attachments(&[]);
        let keyed = || app(Auth::ApiKey("secret".to_owned()));

        assert_eq!(
            post(keyed(), pdf.clone(), Some("wrong")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(post(keyed(), pdf, Some("secret")).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_unsupported_and_broken_documents() {
        let (status, json) = post(app(Auth::Public), b"GIF89a".to_vec(), None).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(json["error"], "unsupported_format");

        assert_eq!(
            post(app(Auth::Public), b"%PDF-1.7 broken".to_vec(), None).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            post(app(Auth::Public), Vec::new(), None).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn times_out_while_no_analysis_slot_is_free() {
        let busy = router(Config {
            max_concurrent: 0,
            timeout: Duration::from_millis(10),
            ..config(Auth::Public)
        });

        let (status, json) = post(busy, pdf_with_attachments(&[]), None).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["error"], "timeout");
    }

    #[tokio::test]
    async fn answers_health_checks() {
        let response = app(Auth::ApiKey("secret".to_owned()))
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
