pub mod interact;
pub mod windows;
pub mod ws;

use crate::auth::{self, ApiKey};
use crate::platform::UiBackend;
use crate::types::ApiResponse;
use axum::{
    middleware,
    routing::{get, post},
    Json, Router,
};
use once_cell::sync::Lazy;
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;

static START_TIME: Lazy<Instant> = Lazy::new(Instant::now);

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn UiBackend>,
    pub ws_tx: ws::WsBroadcast,
}

pub fn router(state: AppState, api_key: String) -> Router {
    // Touch the lazy so uptime starts counting from server boot.
    Lazy::force(&START_TIME);

    let key_ext = ApiKey(api_key);

    Router::new()
        // ── Discovery ──────────────────────────────────────────────────────
        .route("/windows", get(windows::list_windows))
        .route("/windows/:pid/tree", get(windows::get_tree))
        .route("/windows/:pid/find", get(windows::find_elements))
        // HWND-based (for apps with multiple windows sharing the same PID)
        .route("/hwnd/:hwnd/tree", get(windows::get_tree_hwnd))
        .route("/hwnd/:hwnd/find", get(windows::find_elements_hwnd))
        // ── Window operations ──────────────────────────────────────────────
        .route("/windows/:pid/focus", post(windows::focus_window))
        .route("/windows/:pid/close", post(windows::close_window))
        // ── Wait / Poll ──────────────────────────────────────────────────
        .route("/windows/:pid/wait", get(windows::wait_for_element))
        // ── Screenshot ───────────────────────────────────────────────────
        .route("/windows/:pid/screenshot", get(windows::screenshot_window))
        // ── Element interactions ───────────────────────────────────────────
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
        // ── Batch ─────────────────────────────────────────────────────────
        .route("/interact/batch", post(interact::batch))
        // ── Health ─────────────────────────────────────────────────────────
        .route("/health", get(health))
        // ── WebSocket ─────────────────────────────────────────────────────
        .route("/ws", get(ws::ws_handler))
        // ── Auth middleware (applies to all routes above) ─────────────────
        .layer(middleware::from_fn(auth::auth_middleware))
        .layer(axum::Extension(key_ext))
        .with_state(state)
}

#[derive(Serialize)]
struct HealthInfo {
    status: &'static str,
    version: &'static str,
    platform: &'static str,
    arch: &'static str,
    uptime_secs: u64,
}

async fn health() -> Json<ApiResponse<HealthInfo>> {
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
    }))
}
