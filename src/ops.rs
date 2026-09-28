//! Operations shared by the HTTP API and the MCP server: action parsing and
//! dispatch, batch execution and wait/poll. Everything here is blocking and is
//! meant to run on a blocking thread.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::error::{self, invalid_input};
use crate::keys::{self, KeyStep};
use crate::platform::UiBackend;
use crate::types::{ElementType, UiElement};

// ── Targets & search parameters ──────────────────────────────────────────────

/// Which window an operation applies to.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    Pid(u32),
    Hwnd(usize),
}

#[derive(Debug, Clone, Default)]
pub struct FindParams {
    pub query: Option<String>,
    pub element_type: Option<ElementType>,
    pub interactive_only: bool,
}

impl FindParams {
    /// Build from raw string inputs, validating the element type.
    pub fn parse(
        query: Option<String>,
        element_type: Option<&str>,
        interactive_only: bool,
    ) -> Result<Self> {
        let element_type = match element_type.map(str::trim) {
            None | Some("") => None,
            Some(t) => Some(t.parse::<ElementType>()?),
        };
        Ok(Self {
            query: query.filter(|q| !q.is_empty()),
            element_type,
            interactive_only,
        })
    }
}

pub fn find(backend: &dyn UiBackend, target: Target, p: &FindParams) -> Result<Vec<UiElement>> {
    let q = p.query.as_deref();
    let t = p.element_type.as_ref();
    match target {
        Target::Pid(pid) => backend.find_elements(pid, q, t, p.interactive_only),
        Target::Hwnd(hwnd) => backend.find_elements_hwnd(hwnd, q, t, p.interactive_only),
    }
}

pub fn tree(backend: &dyn UiBackend, target: Target) -> Result<UiElement> {
    match target {
        Target::Pid(pid) => backend.get_ui_tree(pid),
        Target::Hwnd(hwnd) => backend.get_ui_tree_hwnd(hwnd),
    }
}

// ── Wait / poll ──────────────────────────────────────────────────────────────

pub const DEFAULT_WAIT_MS: u64 = 5_000;
pub const MAX_WAIT_MS: u64 = 30_000;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitUntil {
    /// Return as soon as at least one element matches.
    #[default]
    Appears,
    /// Return once no element matches any more (e.g. a progress dialog closed).
    Disappears,
}

impl WaitUntil {
    pub fn parse(s: Option<&str>) -> Result<Self> {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("appears") | Some("present") | Some("visible") => {
                Ok(WaitUntil::Appears)
            }
            Some("disappears") | Some("gone") | Some("hidden") => Ok(WaitUntil::Disappears),
            Some(other) => Err(invalid_input(format!(
                "Unknown wait condition '{other}'. Use 'appears' or 'gone'."
            ))),
        }
    }
}

/// Poll `find` until the condition holds or `timeout_ms` elapses.
pub fn wait_for(
    backend: &dyn UiBackend,
    target: Target,
    p: &FindParams,
    until: WaitUntil,
    timeout_ms: u64,
) -> Result<Vec<UiElement>> {
    let timeout_ms = timeout_ms.min(MAX_WAIT_MS);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let found = match find(backend, target, p) {
            Ok(found) => found,
            // While waiting for something to disappear, the window itself
            // vanishing counts as success.
            Err(e)
                if until == WaitUntil::Disappears
                    && error::kind_of(&e) == Some(error::ErrorKind::NotFound) =>
            {
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        let done = match until {
            WaitUntil::Appears => !found.is_empty(),
            WaitUntil::Disappears => found.is_empty(),
        };
        if done {
            return Ok(found);
        }
        if Instant::now() >= deadline {
            let what = match until {
                WaitUntil::Appears => "No matching element appeared",
                WaitUntil::Disappears => "Matching elements did not disappear",
            };
            return Err(error::timeout(format!("{what} within {timeout_ms}ms")));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

// ── Actions ──────────────────────────────────────────────────────────────────

/// A validated element action.
#[derive(Debug, Clone)]
pub enum Action {
    Click,
    Focus,
    Toggle,
    Expand,
    Collapse,
    Select,
    ScrollIntoView,
    SetText(String),
    SendKeys(Vec<KeyStep>),
    SetRange(f64),
    Scroll(&'static str),
}

pub const SCROLL_DIRECTIONS: &[&str] = &["up", "down", "left", "right", "page-up", "page-down"];

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::Click => "click",
            Action::Focus => "focus",
            Action::Toggle => "toggle",
            Action::Expand => "expand",
            Action::Collapse => "collapse",
            Action::Select => "select",
            Action::ScrollIntoView => "scroll-into-view",
            Action::SetText(_) => "set-text",
            Action::SendKeys(_) => "send-keys",
            Action::SetRange(_) => "set-range",
            Action::Scroll(_) => "scroll",
        }
    }

    pub fn send_keys(keys: &str) -> Result<Self> {
        let steps = keys::parse(keys).map_err(invalid_input)?;
        Ok(Action::SendKeys(steps))
    }

    pub fn scroll(direction: &str) -> Result<Self> {
        let d = direction.trim().to_ascii_lowercase();
        SCROLL_DIRECTIONS
            .iter()
            .find(|x| **x == d)
            .map(|x| Action::Scroll(x))
            .ok_or_else(|| {
                invalid_input(format!(
                    "Unknown scroll direction '{direction}'. Use one of: {}",
                    SCROLL_DIRECTIONS.join(", ")
                ))
            })
    }

    pub fn set_range(value: f64) -> Result<Self> {
        if !value.is_finite() {
            return Err(invalid_input("'value' must be a finite number"));
        }
        Ok(Action::SetRange(value))
    }

    /// Build an action from its name plus the optional arguments used by
    /// batch requests and MCP. Missing required arguments are an error —
    /// nothing is defaulted silently.
    pub fn from_parts(
        name: &str,
        text: Option<&str>,
        keys: Option<&str>,
        value: Option<f64>,
        direction: Option<&str>,
    ) -> Result<Self> {
        let need = |v: Option<&str>, field: &str| -> Result<String> {
            v.map(String::from)
                .ok_or_else(|| invalid_input(format!("Action '{name}' requires '{field}'")))
        };
        Ok(
            match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                "click" => Action::Click,
                "focus" => Action::Focus,
                "toggle" => Action::Toggle,
                "expand" => Action::Expand,
                "collapse" => Action::Collapse,
                "select" => Action::Select,
                "scroll-into-view" => Action::ScrollIntoView,
                "set-text" => Action::SetText(need(text, "text")?),
                "send-keys" => Action::send_keys(&need(keys, "keys")?)?,
                "set-range" => Action::set_range(
                    value.ok_or_else(|| invalid_input("Action 'set-range' requires 'value'"))?,
                )?,
                "scroll" => Action::scroll(&need(direction, "direction")?)?,
                other => {
                    return Err(invalid_input(format!(
                        "Unknown action '{other}'. Use one of: click, focus, toggle, expand, \
                     collapse, select, scroll-into-view, set-text, send-keys, set-range, scroll"
                    )))
                }
            },
        )
    }
}

/// Execute one action on one element.
pub fn perform(backend: &dyn UiBackend, id: &str, action: &Action) -> Result<()> {
    match action {
        Action::Click => backend.click_element(id),
        Action::Focus => backend.focus_element(id),
        Action::Toggle => backend.toggle_element(id),
        Action::Expand => backend.expand_element(id),
        Action::Collapse => backend.collapse_element(id),
        Action::Select => backend.select_element(id),
        Action::ScrollIntoView => backend.scroll_into_view(id),
        Action::SetText(text) => backend.set_text(id, text),
        Action::SendKeys(steps) => backend.send_keys(id, steps),
        Action::SetRange(v) => backend.set_range(id, *v),
        Action::Scroll(dir) => backend.scroll_element(id, dir),
    }
}

// ── Batch ────────────────────────────────────────────────────────────────────

/// One step of a batch request, as sent by clients.
#[derive(Debug, Clone, Deserialize)]
pub struct BatchActionSpec {
    pub element_id: String,
    pub action: String,
    pub text: Option<String>,
    pub keys: Option<String>,
    pub value: Option<f64>,
    pub direction: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchRequest {
    pub actions: Vec<BatchActionSpec>,
    /// Stop at the first failing action (default: true) so later steps never
    /// run against an unexpected UI state.
    #[serde(default = "default_true")]
    pub stop_on_error: bool,
    /// Pause between actions, in milliseconds (default 0, max 5000).
    #[serde(default)]
    pub delay_ms: u64,
}

fn default_true() -> bool {
    true
}

pub const MAX_BATCH: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct BatchResult {
    pub index: usize,
    pub action: String,
    pub element_id: String,
    pub success: bool,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
}

/// Validate every step up front; if any step is invalid nothing is executed.
pub fn prepare_batch(req: &BatchRequest) -> Result<Vec<(String, Action)>> {
    if req.actions.is_empty() {
        return Err(invalid_input("'actions' must not be empty"));
    }
    if req.actions.len() > MAX_BATCH {
        return Err(invalid_input(format!(
            "A batch may contain at most {MAX_BATCH} actions"
        )));
    }
    req.actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            Action::from_parts(
                &a.action,
                a.text.as_deref(),
                a.keys.as_deref(),
                a.value,
                a.direction.as_deref(),
            )
            .map(|act| (a.element_id.clone(), act))
            .map_err(|e| invalid_input(format!("actions[{i}]: {e}")))
        })
        .collect()
}

pub fn run_batch(
    backend: &dyn UiBackend,
    steps: &[(String, Action)],
    stop_on_error: bool,
    delay_ms: u64,
) -> Vec<BatchResult> {
    let delay = Duration::from_millis(delay_ms.min(5_000));
    let mut results = Vec::with_capacity(steps.len());
    for (index, (id, action)) in steps.iter().enumerate() {
        if index > 0 && !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let res = perform(backend, id, action);
        let failed = res.is_err();
        results.push(BatchResult {
            index,
            action: action.name().to_string(),
            element_id: id.clone(),
            success: !failed,
            code: res.as_ref().err().map(error_code),
            error: res.err().map(|e| e.to_string()),
        });
        if failed && stop_on_error {
            break;
        }
    }
    results
}

/// Machine-readable code for an error (see [`crate::types::ApiResponse::code`]).
pub fn error_code(err: &anyhow::Error) -> &'static str {
    error::kind_of(err).map_or("internal", |k| k.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_arguments_are_rejected() {
        assert!(Action::from_parts("set-text", None, None, None, None).is_err());
        assert!(Action::from_parts("set-range", None, None, None, None).is_err());
        assert!(Action::from_parts("send-keys", None, None, None, None).is_err());
        assert!(Action::from_parts("scroll", None, None, None, None).is_err());
        assert!(Action::from_parts("dance", None, None, None, None).is_err());
    }

    #[test]
    fn action_names_accept_underscores() {
        let a = Action::from_parts("scroll_into_view", None, None, None, None).unwrap();
        assert_eq!(a.name(), "scroll-into-view");
    }

    #[test]
    fn invalid_batch_fails_before_running() {
        let req: BatchRequest = serde_json::from_value(serde_json::json!({
            "actions": [
                { "element_id": "a", "action": "click" },
                { "element_id": "b", "action": "send-keys", "keys": "{NOPE}" }
            ]
        }))
        .unwrap();
        assert!(req.stop_on_error);
        let err = prepare_batch(&req).unwrap_err();
        assert!(err.to_string().contains("actions[1]"));
    }

    #[test]
    fn wait_condition_parsing() {
        assert_eq!(WaitUntil::parse(None).unwrap(), WaitUntil::Appears);
        assert_eq!(
            WaitUntil::parse(Some("gone")).unwrap(),
            WaitUntil::Disappears
        );
        assert!(WaitUntil::parse(Some("later")).is_err());
    }

    #[test]
    fn scroll_direction_validation() {
        assert_eq!(Action::scroll("Page-Down").unwrap().name(), "scroll");
        assert!(Action::scroll("sideways").is_err());
    }
}
