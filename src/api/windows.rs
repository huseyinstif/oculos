use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;

use crate::{
    api::{ok, truthy, ws::WsEvent, ApiError, ApiPath, ApiQuery, ApiResult, AppState},
    ops::{self, FindParams, Target, WaitUntil},
    types::{UiElement, WindowInfo},
};

// ── Discovery endpoints ───────────────────────────────────────────────────────

/// GET /windows
pub async fn list_windows(State(state): State<AppState>) -> ApiResult<Vec<WindowInfo>> {
    let windows = state.blocking(|b| b.list_windows()).await?;
    state.emit(WsEvent::Windows {
        count: windows.len(),
    });
    ok(windows)
}

/// GET /windows/:pid/tree
pub async fn get_tree(
    State(state): State<AppState>,
    ApiPath(pid): ApiPath<u32>,
) -> ApiResult<UiElement> {
    tree(state, Target::Pid(pid)).await
}

/// GET /hwnd/:hwnd/tree
pub async fn get_tree_hwnd(
    State(state): State<AppState>,
    ApiPath(hwnd): ApiPath<usize>,
) -> ApiResult<UiElement> {
    tree(state, Target::Hwnd(hwnd)).await
}

async fn tree(state: AppState, target: Target) -> ApiResult<UiElement> {
    let root = state.blocking(move |b| ops::tree(b, target)).await?;
    let (pid, hwnd) = match target {
        Target::Pid(p) => (Some(p), None),
        Target::Hwnd(h) => (None, Some(h)),
    };
    state.emit(WsEvent::TreeLoaded {
        pid,
        hwnd,
        nodes: count_nodes(&root),
    });
    ok(root)
}

fn count_nodes(e: &UiElement) -> usize {
    1 + e.children.iter().map(count_nodes).sum::<usize>()
}

/// Query parameters for find:
///   q           — case-insensitive substring of label or automation_id
///   type        — element type filter, e.g. "Button" (case-insensitive)
///   interactive — "true" to return only elements with at least one action
#[derive(Deserialize)]
pub struct FindQuery {
    pub q: Option<String>,
    #[serde(rename = "type")]
    pub element_type: Option<String>,
    pub interactive: Option<String>,
}

impl FindQuery {
    fn params(self) -> Result<FindParams, ApiError> {
        Ok(FindParams::parse(
            self.q,
            self.element_type.as_deref(),
            truthy(self.interactive.as_deref()),
        )?)
    }
}

/// GET /windows/:pid/find
pub async fn find_elements(
    State(state): State<AppState>,
    ApiPath(pid): ApiPath<u32>,
    ApiQuery(q): ApiQuery<FindQuery>,
) -> ApiResult<Vec<UiElement>> {
    find(state, Target::Pid(pid), q.params()?).await
}

/// GET /hwnd/:hwnd/find
pub async fn find_elements_hwnd(
    State(state): State<AppState>,
    ApiPath(hwnd): ApiPath<usize>,
    ApiQuery(q): ApiQuery<FindQuery>,
) -> ApiResult<Vec<UiElement>> {
    find(state, Target::Hwnd(hwnd), q.params()?).await
}

async fn find(state: AppState, target: Target, params: FindParams) -> ApiResult<Vec<UiElement>> {
    ok(state
        .blocking(move |b| ops::find(b, target, &params))
        .await?)
}

// ── Wait / Poll ──────────────────────────────────────────────────────────────

/// GET /windows/:pid/wait?q=Submit&type=Button&timeout=5000&until=appears|gone
///
/// Polls every 250ms until an element matches (or, with `until=gone`, until
/// nothing matches any more). Default timeout 5000ms, max 30000ms.
#[derive(Deserialize)]
pub struct WaitQuery {
    pub q: Option<String>,
    #[serde(rename = "type")]
    pub element_type: Option<String>,
    pub interactive: Option<String>,
    pub timeout: Option<u64>,
    pub until: Option<String>,
}

pub async fn wait_for_element(
    State(state): State<AppState>,
    ApiPath(pid): ApiPath<u32>,
    ApiQuery(q): ApiQuery<WaitQuery>,
) -> ApiResult<Vec<UiElement>> {
    wait(state, Target::Pid(pid), q).await
}

pub async fn wait_for_element_hwnd(
    State(state): State<AppState>,
    ApiPath(hwnd): ApiPath<usize>,
    ApiQuery(q): ApiQuery<WaitQuery>,
) -> ApiResult<Vec<UiElement>> {
    wait(state, Target::Hwnd(hwnd), q).await
}

async fn wait(state: AppState, target: Target, q: WaitQuery) -> ApiResult<Vec<UiElement>> {
    let until = WaitUntil::parse(q.until.as_deref())?;
    let timeout = q.timeout.unwrap_or(ops::DEFAULT_WAIT_MS);
    let params = FindParams::parse(
        q.q,
        q.element_type.as_deref(),
        truthy(q.interactive.as_deref()),
    )?;
    ok(state
        .blocking(move |b| ops::wait_for(b, target, &params, until, timeout))
        .await?)
}

// ── Window operations ────────────────────────────────────────────────────────

/// POST /windows/:pid/focus
pub async fn focus_window(
    State(state): State<AppState>,
    ApiPath(pid): ApiPath<u32>,
) -> ApiResult<()> {
    ok(state.blocking(move |b| b.focus_window(pid)).await?)
}

/// POST /windows/:pid/close
pub async fn close_window(
    State(state): State<AppState>,
    ApiPath(pid): ApiPath<u32>,
) -> ApiResult<()> {
    ok(state.blocking(move |b| b.close_window(pid)).await?)
}

/// GET /windows/:pid/screenshot — PNG image
pub async fn screenshot_window(
    State(state): State<AppState>,
    ApiPath(pid): ApiPath<u32>,
) -> Result<impl IntoResponse, ApiError> {
    let png = state.blocking(move |b| b.screenshot_window(pid)).await?;
    Ok(png_response(png))
}

pub fn png_response(png: Vec<u8>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        png,
    )
}
