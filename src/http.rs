//! The HTTP API.
//!
//! `POST /v1/analyze` takes the document as the raw request body and answers
//! with the [`Analysis`](crate::analyze::Analysis) as JSON. Requests carry a
//! bearer token as [`Auth`] demands: a JWT, an API key, or none.
//!
//! The layers around the handler run in this order: a request id is set and
//! propagated, the request is traced, the token is checked and the request is
//! admitted or shed — all before a byte of the body is buffered — and only
//! then the body is read, up to [`Config::max_body_bytes`]. The analysis
//! itself waits for one of [`Config::max_concurrent`] CPU slots for its
//! native phase and, when pages need OCR, for the single OCR slot; both
//! waits end with the request's [`Config::timeout`].
//!
//! `GET /openapi.json` describes the API as `OpenAPI` 3.1; [`openapi`] returns
//! the same document, which `openapi.json` in the repository holds. `GET /docs`
//! renders it with the Lattice API reference.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::{BytesRejection, FailedToBufferBody};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer};
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::openapi::{RefOr, Schema};
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::analyze::{Analysis, AnalyzeError, analyze_native};
use crate::auth::{Auth, AuthError};
use crate::ocr::{self, OcrReport, OcrSettings};

/// The header that carries the request id, generated here unless the client
/// sent one.
const REQUEST_ID: header::HeaderName = header::HeaderName::from_static("x-request-id");

/// What the API needs to run; [`crate::config::Settings`] reads it from the
/// environment.
#[derive(Debug)]
pub struct Config {
    /// How `/v1` requests are authenticated.
    pub auth: Auth,
    /// The largest accepted request body.
    pub max_body_bytes: usize,
    /// Whether and how pages without a text layer are read by OCR.
    pub ocr: OcrSettings,
    /// Analyses that run at once; further requests wait for a slot.
    pub max_concurrent: usize,
    /// Requests admitted beyond `max_concurrent` to wait for a slot, bodies
    /// buffered; further ones are shed with `503 overloaded` before their
    /// body is read.
    pub max_queued: usize,
    /// How long a request may wait for a slot and its analysis.
    pub timeout: Duration,
}

struct AppState {
    config: Config,
    /// In-flight `/v1/analyze` requests, waiting ones included.
    admissions: Semaphore,
    /// CPU slots for the native phase of an analysis.
    analyses: Arc<Semaphore>,
    /// The single OCR slot; see [`crate::ocr`] for why one.
    ocr: Arc<Semaphore>,
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

/// The `OpenAPI` document of the API.
pub fn openapi() -> utoipa::openapi::OpenApi {
    api(|v1| v1).1
}

type V1Router = OpenApiRouter<Arc<AppState>>;

/// The routes and their `OpenAPI` document. `protect` wraps the `/v1` routes
/// with the layers that run before a body is read.
fn api(protect: impl FnOnce(V1Router) -> V1Router) -> (Router<Arc<AppState>>, utoipa::openapi::OpenApi) {
    let v1 = protect(OpenApiRouter::new().routes(routes!(analyze_document)));
    let (router, mut openapi) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(health))
        .merge(v1)
        .split_for_parts();
    PresentFields.modify(&mut openapi);

    (router, openapi)
}

/// The API reference page; it loads the Lattice API reference from a CDN,
/// pinned by version and hash, and renders `/openapi.json`.
const DOCS: &str = include_str!("docs.html");

/// The service: the API, `/health`, `/openapi.json` and `/docs`, with their
/// layers.
pub fn router(config: Config) -> Router {
    let max_body_bytes = config.max_body_bytes;
    let state = Arc::new(AppState {
        admissions: Semaphore::new(config.max_concurrent + config.max_queued),
        analyses: Arc::new(Semaphore::new(config.max_concurrent)),
        ocr: Arc::new(Semaphore::new(1)),
        config,
    });
    let (router, openapi) = api(|v1| {
        // The last layer added runs first: authentication, then admission.
        v1.route_layer(middleware::from_fn_with_state(Arc::clone(&state), admit))
            .route_layer(middleware::from_fn_with_state(Arc::clone(&state), authorize))
    });
    let openapi = Arc::new(openapi);

    // `TraceLayer` logs requests at DEBUG by default, below the default `info`
    // filter; the span and the INFO response hook give an access log of
    // method, path, request id, status and latency. Neither logs headers or
    // bodies, so the token never reaches the log. A 5xx is already logged
    // where it arises, as an error only when it needs attention; at ERROR the
    // failure hook would open a second Sentry issue.
    let trace = TraceLayer::new_for_http()
        .make_span_with(request_span)
        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
        .on_failure(DefaultOnFailure::new().level(tracing::Level::WARN));

    router
        .route(
            "/openapi.json",
            get(move || async move { axum::Json(Arc::clone(&openapi)) }),
        )
        .route("/docs", get(|| async { Html(DOCS) }))
        // Layers added later wrap the earlier ones: the request id is set
        // first, so the trace span sees it, and the body limit applies last,
        // where the handler reads the body.
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(trace)
        .layer(PropagateRequestIdLayer::new(REQUEST_ID))
        .layer(SetRequestIdLayer::new(REQUEST_ID, MakeRequestUuid))
        .with_state(state)
}

/// The span of a request: method, path and request id, never the query or a
/// header.
fn request_span(request: &Request<Body>) -> tracing::Span {
    let request_id = request
        .headers()
        .get(&REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    tracing::info_span!(
        "request",
        method = %request.method(),
        path = %request.uri().path(),
        request_id,
    )
}

/// Checks the bearer token before the body is read, so an unauthenticated
/// client cannot make the service buffer a document.
async fn authorize(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Result<Response, ApiError> {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    match &state.config.auth {
        // Cheap and synchronous.
        auth @ (Auth::Public | Auth::ApiKey(_)) => auth.authorize(authorization)?,
        // JWT verification may fetch the issuer's JWKS with a blocking client.
        Auth::Jwt(_) => {
            let authorization = authorization.map(str::to_owned);
            let auth_state = Arc::clone(&state);

            tokio::task::spawn_blocking(move || auth_state.config.auth.authorize(authorization.as_deref())).await??;
        }
    }

    Ok(next.run(request).await)
}

/// Admits a request while fewer than `max_concurrent + max_queued` are in
/// flight and sheds it otherwise, before its body is read. The permit is held
/// until the response is complete.
async fn admit(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Result<Response, ApiError> {
    let Ok(_permit) = state.admissions.try_acquire() else {
        return Err(ApiError::Overloaded);
    };

    Ok(next.run(request).await)
}

/// The error codes of the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    Unauthorized,
    AuthUnavailable,
    EmptyBody,
    UnreadableBody,
    PayloadTooLarge,
    UnsupportedFormat,
    UnreadablePdf,
    Timeout,
    Overloaded,
    InternalError,
}

/// An error answer.
#[derive(Debug, Serialize, ToSchema)]
struct ErrorBody {
    error: ErrorCode,
    /// A sentence for people; match on `error` instead.
    message: &'static str,
}

/// Why a request was not answered with an analysis. Each variant maps to one
/// status and [`ErrorCode`]; the detail stays in the log.
#[derive(Debug)]
enum ApiError {
    Unauthorized(AuthError),
    AuthUnavailable(String),
    EmptyBody,
    UnreadableBody(BytesRejection),
    PayloadTooLarge,
    UnsupportedFormat,
    UnreadablePdf(String),
    Timeout(Duration),
    Overloaded,
    Panicked(JoinError),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::EmptyBody | Self::UnreadableBody(_) => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedFormat => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::UnreadablePdf(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::AuthUnavailable(_) | Self::Timeout(_) | Self::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            Self::Panicked(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn body(&self) -> ErrorBody {
        let (error, message) = match self {
            Self::Unauthorized(_) => (ErrorCode::Unauthorized, "A valid bearer token is required."),
            Self::AuthUnavailable(_) => (ErrorCode::AuthUnavailable, "The token issuer's keys cannot be fetched."),
            Self::EmptyBody => (ErrorCode::EmptyBody, "Send the document as the request body."),
            Self::UnreadableBody(_) => (ErrorCode::UnreadableBody, "The request body could not be read."),
            Self::PayloadTooLarge => (
                ErrorCode::PayloadTooLarge,
                "The document is larger than this instance accepts.",
            ),
            Self::UnsupportedFormat => (ErrorCode::UnsupportedFormat, "Send a PDF or a CII/UBL e-invoice XML."),
            Self::UnreadablePdf(_) => (ErrorCode::UnreadablePdf, "The PDF cannot be read."),
            Self::Timeout(_) => (ErrorCode::Timeout, "The analysis did not finish in time."),
            Self::Overloaded => (
                ErrorCode::Overloaded,
                "The service is busy with other documents; retry later.",
            ),
            Self::Panicked(_) => (ErrorCode::InternalError, "The analysis failed."),
        };

        ErrorBody { error, message }
    }

    /// Logs the error with its detail, which the response does not carry.
    fn log(&self) {
        match self {
            Self::Unauthorized(reason) => tracing::info!(%reason, "request rejected"),
            Self::AuthUnavailable(reason) => tracing::error!(%reason, "JWKS unavailable"),
            Self::EmptyBody | Self::PayloadTooLarge | Self::UnsupportedFormat => {
                tracing::info!(error = ?self.body().error, "request rejected");
            }
            Self::UnreadableBody(rejection) => tracing::info!(%rejection, "request body not read"),
            Self::UnreadablePdf(reason) => tracing::info!(%reason, "unreadable PDF"),
            Self::Timeout(timeout) => tracing::warn!(?timeout, "analysis timed out"),
            Self::Overloaded => tracing::info!("request shed: too many in flight"),
            Self::Panicked(join_error) => tracing::error!(%join_error, "analysis panicked"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.log();

        (self.status(), axum::Json(self.body())).into_response()
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Jwks(reason) => Self::AuthUnavailable(reason),
            error => Self::Unauthorized(error),
        }
    }
}

impl From<AnalyzeError> for ApiError {
    fn from(error: AnalyzeError) -> Self {
        match error {
            AnalyzeError::UnsupportedFormat => Self::UnsupportedFormat,
            AnalyzeError::UnreadablePdf(reason) => Self::UnreadablePdf(reason),
        }
    }
}

impl From<JoinError> for ApiError {
    fn from(error: JoinError) -> Self {
        Self::Panicked(error)
    }
}

impl From<BytesRejection> for ApiError {
    fn from(rejection: BytesRejection) -> Self {
        match &rejection {
            BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => Self::PayloadTooLarge,
            _ => Self::UnreadableBody(rejection),
        }
    }
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
        (status = 400, description = "The body is empty (`empty_body`) or could not be read (`unreadable_body`).", body = ErrorBody),
        (status = 401, description = "The bearer token is missing or invalid (`unauthorized`).", body = ErrorBody),
        (status = 413, description = "The body exceeds `DOCPIPE_MAX_BODY_MB` (`payload_too_large`).", body = ErrorBody),
        (status = 415, description = "The body is neither a PDF nor a CII/UBL e-invoice XML (`unsupported_format`).", body = ErrorBody),
        (status = 422, description = "The PDF cannot be read (`unreadable_pdf`).", body = ErrorBody),
        (status = 500, description = "The analysis failed (`internal_error`).", body = ErrorBody),
        (status = 503, description = "The issuer's keys cannot be fetched (`auth_unavailable`), more than `DOCPIPE_MAX_CONCURRENT` plus `DOCPIPE_MAX_QUEUED` requests are in flight (`overloaded`), or no analysis slot freed up or the analysis did not finish within `DOCPIPE_TIMEOUT_SECS` (`timeout`).", body = ErrorBody),
    ),
    security(("bearer" = [])),
)]
async fn analyze_document(
    State(state): State<Arc<AppState>>,
    body: Result<Bytes, BytesRejection>,
) -> Result<axum::Json<Analysis>, ApiError> {
    let body = body?;

    if body.is_empty() {
        return Err(ApiError::EmptyBody);
    }

    let timeout = state.config.timeout;
    let analysis = tokio::time::timeout(timeout, analyze(&state, body))
        .await
        .map_err(|_elapsed| ApiError::Timeout(timeout))??;

    Ok(axum::Json(analysis))
}

/// Both phases of an analysis, each on a blocking thread under its slot. A
/// slot moves into its blocking task, so an analysis that outlives its
/// request still holds the slot until it ends.
async fn analyze(state: &AppState, body: Bytes) -> Result<Analysis, ApiError> {
    let native = {
        let slot = Arc::clone(&state.analyses)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        let body = body.clone();

        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            analyze_native(&body)
        })
        .await??
    };

    let report = if let Some(status) = ocr::skipped(native.pages_needing_ocr(), &state.config.ocr) {
        OcrReport::without(status)
    } else {
        // Waiting here holds no CPU slot and ends with the request's timeout.
        let slot = Arc::clone(&state.ocr)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        let pages = native.pages_needing_ocr().to_vec();
        let settings = state.config.ocr.clone();

        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            ocr::recognize(&body, &pages, &settings)
        })
        .await?
    };

    Ok(native.finish(report))
}

#[cfg(test)]
mod tests {
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
            max_queued: 2,
            timeout: Duration::from_secs(30),
        }
    }

    fn app(auth: Auth) -> Router {
        router(config(auth))
    }

    async fn send(app: Router, body: Vec<u8>, token: Option<&str>) -> Response {
        let mut request = Request::post("/v1/analyze");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }

        app.oneshot(request.body(Body::from(body)).unwrap()).await.unwrap()
    }

    async fn post(app: Router, body: Vec<u8>, token: Option<&str>) -> (StatusCode, serde_json::Value) {
        let response = send(app, body, token).await;
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

        let (status, json) = post(jwt(), pdf.clone(), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["error"], "unauthorized");
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

    /// The token is checked before the body is read: an oversized body from
    /// an unauthenticated client is rejected as unauthorized, not as too
    /// large.
    #[tokio::test]
    async fn authorizes_before_reading_the_body() {
        let small = router(Config {
            max_body_bytes: 16,
            ..config(Auth::ApiKey("secret".to_owned()))
        });

        let (status, json) = post(small, vec![b'%'; 64], None).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["error"], "unauthorized");
    }

    #[tokio::test]
    async fn rejects_unsupported_and_broken_documents() {
        let (status, json) = post(app(Auth::Public), b"GIF89a".to_vec(), None).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(json["error"], "unsupported_format");

        let (status, json) = post(app(Auth::Public), b"%PDF-1.7 broken".to_vec(), None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json["error"], "unreadable_pdf");

        let (status, json) = post(app(Auth::Public), Vec::new(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "empty_body");
    }

    #[tokio::test]
    async fn rejects_oversized_bodies_with_json() {
        let small = router(Config {
            max_body_bytes: 16,
            ..config(Auth::Public)
        });

        let (status, json) = post(small, pdf_with_attachments(&[]), None).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(json["error"], "payload_too_large");
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
    async fn sheds_requests_beyond_the_queue() {
        let full = router(Config {
            max_concurrent: 0,
            max_queued: 0,
            ..config(Auth::Public)
        });

        let (status, json) = post(full, pdf_with_attachments(&[]), None).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["error"], "overloaded");
    }

    #[tokio::test]
    async fn answers_with_a_request_id() {
        let generated = send(app(Auth::Public), pdf_with_attachments(&[]), None).await;
        assert!(generated.headers().contains_key("x-request-id"));

        let response = app(Auth::Public)
            .oneshot(
                Request::post("/v1/analyze")
                    .header("x-request-id", "client-chosen")
                    .body(Body::from(pdf_with_attachments(&[])))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["x-request-id"], "client-chosen");
    }

    #[tokio::test]
    async fn serves_its_api_reference_without_a_token() {
        let response = app(Auth::ApiKey("secret".to_owned()))
            .oneshot(Request::get("/docs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&bytes).contains("fetch(\"openapi.json\")"));
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
