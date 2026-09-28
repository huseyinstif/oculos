pub mod dashboard;
pub mod interact;
pub mod security;
pub mod windows;
pub mod ws;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    async_trait,
    extract::{
        rejection::{JsonRejection, PathRejection, QueryRejection},
        FromRequest, FromRequestParts, Request, State,
    },
    http::{header, request::Parts, HeaderValue, Method, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use once_cell::sync::Lazy;
use serde::{de::DeserializeOwned, Serialize};
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};

use crate::error::{self, ErrorKind};
use crate::platform::UiBackend;
use crate::types::ApiResponse;

static START_TIME: Lazy<Instant> = Lazy::new(Instant::now);

/// Server-level configuration (everything that isn't the backend itself).
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    /// API token; `None` disables token authentication.
    pub token: Option<String>,
    /// Extra browser origins allowed to call the API (enables CORS for them).
    pub allowed_origins: Vec<String>,
    /// Extra Host header names accepted besides IP literals and `localhost`.
    pub allowed_hosts: Vec<String>,
    /// Serve the dashboard from this directory instead of the embedded copy.
    pub static_dir: Option<PathBuf>,
}

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn UiBackend>,
    pub ws_tx: ws::WsBroadcast,
    pub config: Arc<ServerConfig>,
}

impl AppState {
    pub fn new(backend: Arc<dyn UiBackend>, config: ServerConfig) -> Self {
        Self {
            backend,
            ws_tx: ws::create_broadcast(),
            config: Arc::new(config),
        }
    }

    /// Run a blocking backend operation on the blocking thread pool.
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&dyn UiBackend) -> anyhow::Result<T> + Send + 'static,
    {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || f(backend.as_ref()))
            .await
            .map_err(|e| ApiError::internal(format!("worker task failed: {e}")))?
            .map_err(ApiError::from)
    }

    pub fn emit(&self, event: ws::WsEvent) {
        // No subscribers is not an error.
        let _ = self.ws_tx.send(event);
    }
}

/// Build the complete application: API routes, dashboard, security layers.
pub fn build_app(state: AppState) -> Router {
    Lazy::force(&START_TIME);
    let config = state.config.clone();

    // Routes that require the API token (when one is configured).
    let protected = Router::new()
        // ── Discovery ──────────────────────────────────────────────────────
        .route("/windows", get(windows::list_windows))
        .route("/windows/:pid/tree", get(windows::get_tree))
        .route("/windows/:pid/find", get(windows::find_elements))
        .route("/windows/:pid/wait", get(windows::wait_for_element))
        // HWND-based (for apps with multiple windows sharing the same PID)
        .route("/hwnd/:hwnd/tree", get(windows::get_tree_hwnd))
        .route("/hwnd/:hwnd/find", get(windows::find_elements_hwnd))
        .route("/hwnd/:hwnd/wait", get(windows::wait_for_element_hwnd))
        // ── Window operations ──────────────────────────────────────────────
        .route("/windows/:pid/focus", post(windows::focus_window))
        .route("/windows/:pid/close", post(windows::close_window))
        .route("/windows/:pid/screenshot", get(windows::screenshot_window))
        // ── Element interactions ───────────────────────────────────────────
        .route("/interact/batch", post(interact::batch))
        .route("/interact/:id/click", post(interact::click))
        .route("/interact/:id/set-text", post(interact::set_text))
        .route("/interact/:id/send-keys", post(interact::send_keys))
        .route("/interact/:id/focus", post(interact::focus))
        .route("/interact/:id/toggle", post(interact::toggle))
        .route("/interact/:id/expand", post(interact::expand))
        .route("/interact/:id/collapse", post(interact::collapse))
        .route("/interact/:id/select", post(interact::select))
        .route("/interact/:id/set-range", post(interact::set_range))
        .route("/interact/:id/scroll", post(interact::scroll))
        .route(
            "/interact/:id/scroll-into-view",
            post(interact::scroll_into_view),
        )
        .route("/interact/:id/highlight", post(interact::highlight))
        .route(
            "/interact/:id/screenshot",
            get(interact::screenshot_element),
        )
        // ── WebSocket ──────────────────────────────────────────────────────
        .route("/ws", get(ws::ws_handler))
        .route_layer(middleware::from_fn_with_state(
            config.clone(),
            security::require_token,
        ));

    // Routes that stay reachable without a token.
    let public = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard::index))
        .route("/index.html", get(dashboard::index));

    let mut app = Router::new().merge(protected).merge(public);
    app = match &config.static_dir {
        Some(dir) => app.fallback_service(ServeDir::new(dir)),
        None => app.fallback(not_found_route),
    };

    let mut app = app.with_state(state).layer(middleware::from_fn_with_state(
        config.clone(),
        security::check_host_and_origin,
    ));

    if !config.allowed_origins.is_empty() {
        let origins: Vec<HeaderValue> = config
            .allowed_origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o.trim_end_matches('/')).ok())
            .collect();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::AUTHORIZATION,
                    header::HeaderName::from_static(security::TOKEN_HEADER),
                ]),
        );
    }

    app.layer(TraceLayer::new_for_http())
}

async fn not_found_route() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "No such endpoint")
}

// ── Health ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthInfo {
    status: &'static str,
    version: &'static str,
    platform: &'static str,
    arch: &'static str,
    uptime_secs: u64,
    auth_required: bool,
}

async fn health(State(state): State<AppState>) -> Json<ApiResponse<HealthInfo>> {
    let platform = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };

    Json(ApiResponse::ok(HealthInfo {
        status: "running",
        version: env!("CARGO_PKG_VERSION"),
        platform,
        arch: std::env::consts::ARCH,
        uptime_secs: START_TIME.elapsed().as_secs(),
        auth_required: state.config.token.is_some(),
    }))
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// An error response in the standard envelope, with a status code and a
/// machine-readable `code`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_input", message)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        let status = match error::kind_of(&err) {
            Some(ErrorKind::NotFound) => StatusCode::NOT_FOUND,
            Some(ErrorKind::InvalidInput) | Some(ErrorKind::Unsupported) => StatusCode::BAD_REQUEST,
            Some(ErrorKind::Timeout) => StatusCode::REQUEST_TIMEOUT,
            Some(ErrorKind::PermissionDenied) => StatusCode::FORBIDDEN,
            None => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // `{:#}` keeps the context chain ("while X: cause") in one line.
        Self::new(status, crate::ops::error_code(&err), format!("{err:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ApiResponse::err(self.code, self.message))).into_response()
    }
}

pub type ApiResult<T> = Result<Json<ApiResponse<T>>, ApiError>;

pub fn ok<T: Serialize>(data: T) -> ApiResult<T> {
    Ok(Json(ApiResponse::ok(data)))
}

// ── Extractors that answer with the standard error envelope ──────────────────

/// `Json<T>` whose rejection is an [`ApiError`].
pub struct ApiJson<T>(pub T);

#[async_trait]
impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(rej) => Err(json_rejection(rej)),
        }
    }
}

fn json_rejection(rej: JsonRejection) -> ApiError {
    ApiError::new(rej.status(), "invalid_input", rej.body_text())
}

/// `Query<T>` whose rejection is an [`ApiError`].
pub struct ApiQuery<T>(pub T);

#[async_trait]
impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|q| ApiQuery(q.0))
            .map_err(|rej: QueryRejection| ApiError::bad_request(rej.body_text()))
    }
}

/// `Path<T>` whose rejection is an [`ApiError`].
pub struct ApiPath<T>(pub T);

#[async_trait]
impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|p| ApiPath(p.0))
            .map_err(|rej: PathRejection| ApiError::bad_request(rej.body_text()))
    }
}

/// Interpret common truthy query-string values.
pub fn truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("true") | Some("1") | Some("yes") | Some("on")
    )
}
