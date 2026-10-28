//! The HTTP API.
//!
//! `POST /v1/analyze` takes the document as the raw request body and answers
//! with the [`Analysis`](crate::analyze::Analysis) as JSON. Requests carry a
//! bearer token as [`Auth`] demands: a JWT, an API key, or none.
//!
//! `GET /openapi.json` describes the API as OpenAPI 3.1; [`openapi`] returns
//! the same document, which `openapi.json` in the repository holds.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::openapi::{RefOr, Schema};
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::analyze::{Analysis, AnalyzeError, analyze};
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

#[derive(OpenApi)]
#[openapi(
    info(
        title = "docpipe",
        description = "Analyses German invoices and quotes: PDF classification, the text of every page, OCR of \
            scanned pages, embedded ZUGFeRD/Factur-X and XRechnung XML, and field candidates for a person to \
            confirm. The service is stateless and keeps nothing it receives."
    ),
    servers((
        url = "{base_url}",
        description = "A docpipe instance",
        variables(("base_url" = (default = "http://localhost:8080", description = "Where docpipe runs"))),
    )),
    tags(
        (name = "analysis", description = "Document analysis"),
        (name = "operations", description = "Health checks"),
    ),
    modifiers(&BearerAuth),
)]
struct ApiDoc;

/// Every field of a response is present, `null` when it has no value: serde
/// serializes `None` as `null`, so `Option` means nullable, not optional.
/// Applied after the routes added their schemas.
struct PresentFields;

impl Modify for PresentFields {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let schemas = openapi
            .components
            .iter_mut()
            .flat_map(|components| components.schemas.values_mut());

        for schema in schemas {
            if let RefOr::T(Schema::Object(object)) = schema {
                object.required = object.properties.keys().cloned().collect();
            }
        }
    }
}

/// The `bearer` security scheme: a JWT or a static API key, as `AUTH_MODE`
/// names.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let mut bearer = Http::new(HttpAuthScheme::Bearer);
        bearer.description = Some(
            "With `AUTH_MODE=jwt` an RS256 access token of the configured issuer, with `AUTH_MODE=api_key` the \
             static API key. With `AUTH_MODE=none` no token is required."
                .to_owned(),
        );

        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme("bearer", SecurityScheme::Http(bearer));
    }
}

/// The OpenAPI document of the API.
pub fn openapi() -> utoipa::openapi::OpenApi {
    api().1
}

fn api() -> (Router<Arc<AppState>>, utoipa::openapi::OpenApi) {
    let (router, mut openapi) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(health))
        .routes(routes!(analyze_document))
        .split_for_parts();
    PresentFields.modify(&mut openapi);

    (router, openapi)
}

pub fn router(config: Config) -> Router {
    let max_body_bytes = config.max_body_bytes;
    let analyses = Arc::new(Semaphore::new(config.max_concurrent));
    let (router, openapi) = api();
    let openapi = Arc::new(openapi);

    router
        .route(
            "/openapi.json",
            get(move || async move { axum::Json(Arc::clone(&openapi)) }),
        )
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(AppState { config, analyses }))
}

/// The error codes of the API.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    Unauthorized,
    AuthUnavailable,
    EmptyBody,
    UnsupportedFormat,
    UnreadablePdf,
    Timeout,
    InternalError,
}

/// An error answer.
#[derive(Debug, Serialize, ToSchema)]
struct ErrorBody {
    error: ErrorCode,
    /// A sentence for people; match on `error` instead.
    message: &'static str,
}

/// Health check
///
/// Answers `ok` while the service runs. It needs no token.
#[utoipa::path(
    get,
    path = "/health",
    tag = "operations",
    responses((status = 200, description = "The service runs.", body = String, content_type = "text/plain", example = "ok")),
    security(()),
)]
async fn health() -> &'static str {
    "ok"
}

/// Analyse a document
///
/// Takes a PDF or a standalone CII/UBL e-invoice XML as the raw request body.
/// Text-based pages are read from their text layer; pages without one go to
/// OCR, one document at a time. Candidates are proposals: a field that cannot
/// be read with confidence is `null`.
#[utoipa::path(
    post,
    path = "/v1/analyze",
    tag = "analysis",
    request_body(
        description = "The document as the raw body, at most `DOCPIPE_MAX_BODY_MB` megabytes.",
        content(("application/pdf"), ("application/xml"), ("text/xml")),
    ),
    responses(
        (status = 200, description = "The analysis.", body = Analysis),
        (status = 400, description = "The body is empty (`empty_body`).", body = ErrorBody),
        (status = 401, description = "The bearer token is missing or invalid (`unauthorized`).", body = ErrorBody),
        (status = 413, description = "The body exceeds `DOCPIPE_MAX_BODY_MB`."),
        (status = 415, description = "The body is neither a PDF nor a CII/UBL e-invoice XML (`unsupported_format`).", body = ErrorBody),
        (status = 422, description = "The PDF cannot be read (`unreadable_pdf`).", body = ErrorBody),
        (status = 500, description = "The analysis failed (`internal_error`).", body = ErrorBody),
        (status = 503, description = "The issuer's keys cannot be fetched (`auth_unavailable`), or no analysis slot freed up or the analysis did not finish within `DOCPIPE_TIMEOUT_SECS` (`timeout`).", body = ErrorBody),
    ),
    security(("bearer" = [])),
)]
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
                ErrorCode::AuthUnavailable,
                "The token issuer's keys cannot be fetched.",
            );
        }
        Ok(Err(reason)) => {
            tracing::info!(%reason, "request rejected");
            return error(
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthorized,
                "A valid bearer token is required.",
            );
        }
        Err(join_error) => {
            tracing::error!(%join_error, "authorization panicked");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                "The analysis failed.",
            );
        }
    }

    if body.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            ErrorCode::EmptyBody,
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
            ErrorCode::UnsupportedFormat,
            "Send a PDF or a CII/UBL e-invoice XML.",
        ),
        Ok(Ok(Err(AnalyzeError::UnreadablePdf(reason)))) => {
            tracing::info!(%reason, "unreadable PDF");
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::UnreadablePdf,
                "The PDF cannot be read.",
            )
        }
        Err(_) => {
            tracing::warn!(timeout = ?state.config.timeout, "analysis timed out");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Timeout,
                "The analysis did not finish in time.",
            )
        }
        Ok(Err(join_error)) => {
            tracing::error!(%join_error, "analysis panicked");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                "The analysis failed.",
            )
        }
    }
}

fn error(status: StatusCode, error: ErrorCode, message: &'static str) -> Response {
    (status, axum::Json(ErrorBody { error, message })).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
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
    async fn serves_its_openapi_document_without_a_token() {
        let response = app(Auth::ApiKey("secret".to_owned()))
            .oneshot(Request::get("/openapi.json").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(document["openapi"], "3.1.0");
        assert!(document["paths"]["/v1/analyze"]["post"].is_object());
    }

    /// `openapi.json` in the repository is the document the code describes;
    /// `cargo run -- openapi > openapi.json` updates it.
    #[test]
    fn the_committed_openapi_document_is_current() {
        let committed: serde_json::Value = serde_json::from_str(include_str!("../openapi.json")).unwrap();

        assert_eq!(serde_json::to_value(openapi()).unwrap(), committed);
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
