use axum::{extract::State, response::IntoResponse, Json};
use serde_json::{json, Value};

use crate::{
    api::{
        ok, windows::png_response, ws::WsEvent, ApiError, ApiJson, ApiPath, ApiResult, AppState,
    },
    ops::{self, Action, BatchRequest, BatchResult},
    types::{HighlightPayload, ScrollPayload, SendKeysPayload, SetRangePayload, SetTextPayload},
};

/// Run one action on the blocking pool, broadcast the outcome, and answer with
/// `{ "action": name, ...extra }`.
async fn act(state: AppState, id: String, action: Action, extra: Value) -> ApiResult<Value> {
    let name = action.name();
    let id2 = id.clone();
    let res = state
        .blocking(move |b| ops::perform(b, &id2, &action))
        .await;
    state.emit(WsEvent::action(
        name,
        &id,
        &res.as_ref().map(|_| ()).map_err(|e| e.message.clone()),
    ));
    res?;

    let mut body = json!({ "action": name });
    if let (Some(obj), Value::Object(extra)) = (body.as_object_mut(), extra) {
        obj.extend(extra);
    }
    ok(body)
}

// ── Simple actions (no body) ─────────────────────────────────────────────────

macro_rules! simple_action {
    ($(#[$doc:meta])* $name:ident => $action:expr) => {
        $(#[$doc])*
        pub async fn $name(
            State(s): State<AppState>,
            ApiPath(id): ApiPath<String>,
        ) -> ApiResult<Value> {
            act(s, id, $action, Value::Null).await
        }
    };
}

simple_action!(
    /// POST /interact/:id/click
    click => Action::Click
);
simple_action!(
    /// POST /interact/:id/focus
    focus => Action::Focus
);
simple_action!(
    /// POST /interact/:id/toggle
    toggle => Action::Toggle
);
simple_action!(
    /// POST /interact/:id/expand
    expand => Action::Expand
);
simple_action!(
    /// POST /interact/:id/collapse
    collapse => Action::Collapse
);
simple_action!(
    /// POST /interact/:id/select
    select => Action::Select
);
simple_action!(
    /// POST /interact/:id/scroll-into-view
    scroll_into_view => Action::ScrollIntoView
);

// ── Actions with a body ──────────────────────────────────────────────────────

/// POST /interact/:id/set-text  body: { "text": "..." }
pub async fn set_text(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(p): ApiJson<SetTextPayload>,
) -> ApiResult<Value> {
    act(s, id, Action::SetText(p.text), Value::Null).await
}

/// POST /interact/:id/send-keys  body: { "keys": "Hello{ENTER}" }
///
/// The key sequence is validated before anything is typed.
pub async fn send_keys(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(p): ApiJson<SendKeysPayload>,
) -> ApiResult<Value> {
    let action = Action::send_keys(&p.keys)?;
    act(s, id, action, Value::Null).await
}

/// POST /interact/:id/set-range  body: { "value": 42.0 }
pub async fn set_range(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(p): ApiJson<SetRangePayload>,
) -> ApiResult<Value> {
    let action = Action::set_range(p.value)?;
    act(s, id, action, json!({ "value": p.value })).await
}

/// POST /interact/:id/scroll  body: { "direction": "down" }
pub async fn scroll(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(p): ApiJson<ScrollPayload>,
) -> ApiResult<Value> {
    let action = Action::scroll(&p.direction)?;
    let direction = match &action {
        Action::Scroll(d) => *d,
        _ => unreachable!("Action::scroll always builds a Scroll action"),
    };
    act(s, id, action, json!({ "direction": direction })).await
}

/// POST /interact/:id/highlight  body (optional): { "duration_ms": 2000 }
pub async fn highlight(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    body: Option<Json<HighlightPayload>>,
) -> ApiResult<Value> {
    let dur = body.map(|b| b.duration_ms).unwrap_or(2000);
    let rect = s.blocking(move |b| b.highlight_element(&id, dur)).await?;
    ok(json!({ "action": "highlight", "rect": rect }))
}

/// GET /interact/:id/screenshot — PNG of the element's on-screen area
pub async fn screenshot_element(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let png = s.blocking(move |b| b.screenshot_element(&id)).await?;
    Ok(png_response(png))
}

// ── Batch operations ──────────────────────────────────────────────────────────

/// POST /interact/batch
///
/// Body: `{ "actions": [ { "element_id": "...", "action": "click" }, ... ],
///          "stop_on_error": true, "delay_ms": 0 }`
///
/// Every step is validated before anything runs (400 if one is invalid).
/// Execution stops at the first failure unless `stop_on_error` is false.
pub async fn batch(
    State(s): State<AppState>,
    ApiJson(req): ApiJson<BatchRequest>,
) -> ApiResult<Vec<BatchResult>> {
    let steps = ops::prepare_batch(&req)?;
    let (stop, delay) = (req.stop_on_error, req.delay_ms);
    let results = s
        .blocking(move |b| Ok(ops::run_batch(b, &steps, stop, delay)))
        .await?;
    for r in &results {
        s.emit(WsEvent::action(
            &r.action,
            &r.element_id,
            &match &r.error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            },
        ));
    }
    ok(results)
}
