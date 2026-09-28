//! End-to-end tests of the HTTP API and the MCP server against a mock backend.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::api::{self, AppState, ServerConfig};
use crate::error;
use crate::keys::KeyStep;
use crate::mcp::McpServer;
use crate::platform::UiBackend;
use crate::types::{ElementType, Rect, UiElement, WindowInfo};

// ── Mock backend ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockBackend {
    calls: Mutex<Vec<String>>,
}

impl MockBackend {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn tree() -> UiElement {
        let mut root = UiElement::new("root0000".into(), ElementType::Window);
        root.label = "Untitled - Notepad".into();

        let mut save = UiElement::new("btn-save".into(), ElementType::Button);
        save.label = "Save".into();
        save.actions = vec!["click".into(), "focus".into()];
        save.rect = Rect {
            x: 10,
            y: 10,
            width: 80,
            height: 24,
        };

        let mut edit = UiElement::new("edit-body".into(), ElementType::Edit);
        edit.label = "Text editor".into();
        edit.automation_id = Some("15".into());
        edit.actions = vec!["set-text".into(), "send-keys".into(), "focus".into()];

        let status = UiElement::new("status".into(), ElementType::StatusBar);

        root.children = vec![save, edit, status];
        root
    }

    fn check(&self, id: &str) -> Result<()> {
        if Self::flatten(&Self::tree())
            .iter()
            .any(|e| e.oculos_id == id)
        {
            Ok(())
        } else {
            Err(error::element_not_found(id))
        }
    }

    fn flatten(e: &UiElement) -> Vec<UiElement> {
        let mut out = vec![UiElement {
            children: vec![],
            ..e.clone()
        }];
        for c in &e.children {
            out.extend(Self::flatten(c));
        }
        out
    }

    fn search(q: Option<&str>, t: Option<&ElementType>, interactive_only: bool) -> Vec<UiElement> {
        Self::flatten(&Self::tree())
            .into_iter()
            .filter(|e| {
                q.is_none_or(|q| e.label.to_lowercase().contains(&q.to_lowercase()))
                    && t.is_none_or(|t| &e.element_type == t)
                    && (!interactive_only || !e.actions.is_empty())
            })
            .collect()
    }

    fn simple(&self, what: &str, id: &str) -> Result<()> {
        self.check(id)?;
        self.record(format!("{what} {id}"));
        Ok(())
    }
}

impl UiBackend for MockBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(vec![WindowInfo {
            pid: 42,
            hwnd: 7,
            title: "Untitled - Notepad".into(),
            exe_name: "notepad.exe".into(),
            rect: Rect::default(),
            visible: true,
        }])
    }

    fn get_ui_tree(&self, pid: u32) -> Result<UiElement> {
        match pid {
            42 => Ok(Self::tree()),
            _ => Err(error::not_found(format!("No window for PID {pid}"))),
        }
    }

    fn get_ui_tree_hwnd(&self, hwnd: usize) -> Result<UiElement> {
        self.get_ui_tree(if hwnd == 7 { 42 } else { 0 })
    }

    fn find_elements(
        &self,
        pid: u32,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        self.get_ui_tree(pid)?;
        Ok(Self::search(query, element_type, interactive_only))
    }

    fn find_elements_hwnd(
        &self,
        hwnd: usize,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        self.get_ui_tree_hwnd(hwnd)?;
        Ok(Self::search(query, element_type, interactive_only))
    }

    fn click_element(&self, id: &str) -> Result<()> {
        self.simple("click", id)
    }

    fn set_text(&self, id: &str, text: &str) -> Result<()> {
        self.check(id)?;
        self.record(format!("set_text {id} {text}"));
        Ok(())
    }

    fn send_keys(&self, id: &str, steps: &[KeyStep]) -> Result<()> {
        self.check(id)?;
        self.record(format!("send_keys {id} {}", steps.len()));
        Ok(())
    }

    fn focus_element(&self, id: &str) -> Result<()> {
        self.simple("focus", id)
    }

    fn toggle_element(&self, id: &str) -> Result<()> {
        self.check(id)?;
        Err(error::unsupported("Element does not support toggling"))
    }

    fn expand_element(&self, id: &str) -> Result<()> {
        self.simple("expand", id)
    }

    fn collapse_element(&self, id: &str) -> Result<()> {
        self.simple("collapse", id)
    }

    fn select_element(&self, id: &str) -> Result<()> {
        self.simple("select", id)
    }

    fn set_range(&self, id: &str, value: f64) -> Result<()> {
        self.check(id)?;
        self.record(format!("set_range {id} {value}"));
        Ok(())
    }

    fn scroll_element(&self, id: &str, direction: &str) -> Result<()> {
        self.check(id)?;
        self.record(format!("scroll {id} {direction}"));
        Ok(())
    }

    fn scroll_into_view(&self, id: &str) -> Result<()> {
        self.simple("scroll_into_view", id)
    }

    fn focus_window(&self, pid: u32) -> Result<()> {
        self.get_ui_tree(pid).map(|_| ())
    }

    fn close_window(&self, pid: u32) -> Result<()> {
        self.get_ui_tree(pid)?;
        self.record(format!("close {pid}"));
        Ok(())
    }

    fn screenshot_window(&self, pid: u32) -> Result<Vec<u8>> {
        self.get_ui_tree(pid)?;
        Ok(b"\x89PNG fake".to_vec())
    }
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

fn app_with(config: ServerConfig) -> (Router, Arc<MockBackend>) {
    let backend = Arc::new(MockBackend::default());
    let app = api::build_app(AppState::new(backend.clone(), config));
    (app, backend)
}

fn app() -> (Router, Arc<MockBackend>) {
    app_with(ServerConfig::default())
}

struct Req {
    method: Method,
    uri: String,
    headers: Vec<(&'static str, String)>,
    body: Option<Value>,
    peer: Option<std::net::SocketAddr>,
}

fn get(uri: &str) -> Req {
    Req {
        method: Method::GET,
        uri: uri.into(),
        headers: vec![("host", "127.0.0.1:7878".into())],
        body: None,
        peer: None,
    }
}

fn post(uri: &str, body: Option<Value>) -> Req {
    Req {
        method: Method::POST,
        body,
        ..get(uri)
    }
}

impl Req {
    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.retain(|(n, _)| *n != name);
        self.headers.push((name, value.into()));
        self
    }

    fn with_peer(mut self, addr: &str) -> Self {
        self.peer = Some(addr.parse().unwrap());
        self
    }

    async fn send(self, app: &Router) -> (StatusCode, Value) {
        let (status, bytes) = self.send_raw(app).await;
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    async fn send_raw(self, app: &Router) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder().method(self.method).uri(self.uri);
        for (k, v) in &self.headers {
            b = b.header(*k, v);
        }
        let body = match self.body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let mut req = b.body(body).unwrap();
        if let Some(peer) = self.peer {
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
        }
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }
}

// ── HTTP: security ────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_is_public_and_reports_auth() {
    let (app, _) = app();
    let (status, body) = get("/health").send(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["auth_required"], false);
}

#[tokio::test]
async fn foreign_host_header_is_rejected() {
    let (app, backend) = app();
    let (status, body) = get("/windows")
        .header("host", "attacker.example:7878")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");
    assert!(backend.calls().is_empty());
}

#[tokio::test]
async fn cross_origin_simple_post_is_rejected_before_acting() {
    let (app, backend) = app();
    let (status, body) = post("/interact/btn-save/click", None)
        .header("origin", "https://evil.example")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");
    assert!(backend.calls().is_empty(), "the click must not happen");
}

#[tokio::test]
async fn same_origin_dashboard_requests_are_allowed() {
    let (app, backend) = app();
    let (status, _) = post("/interact/btn-save/click", None)
        .header("origin", "http://127.0.0.1:7878")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(backend.calls(), vec!["click btn-save"]);
}

#[tokio::test]
async fn no_cors_headers_by_default() {
    let (app, _) = app();
    let mut b = Request::builder()
        .method(Method::GET)
        .uri("/health")
        .header("host", "127.0.0.1:7878");
    b = b.header("origin", "http://127.0.0.1:7878");
    let resp = app.oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn allowed_origin_gets_cors() {
    let (app, _) = app_with(ServerConfig {
        allowed_origins: vec!["http://localhost:3000".into()],
        ..Default::default()
    });
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header("host", "127.0.0.1:7878")
                .header("origin", "http://localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["access-control-allow-origin"],
        "http://localhost:3000"
    );
}

#[tokio::test]
async fn token_is_enforced_when_configured() {
    let (app, _) = app_with(ServerConfig {
        token: Some("s3cret".into()),
        ..Default::default()
    });

    let (status, body) = get("/windows").send(&app).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthorized");

    let (status, _) = get("/windows")
        .header("x-oculos-token", "wrong")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = get("/windows")
        .header("x-oculos-token", "s3cret")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = get("/windows")
        .header("authorization", "Bearer s3cret")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get("/health").send(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["auth_required"], true);

    // The dashboard page is public; only local clients get the token.
    let (status, html) = get("/").with_peer("127.0.0.1:50000").send_raw(&app).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(html).unwrap();
    assert!(html.contains(r#"<meta name="oculos-token" content="s3cret">"#));

    let (_, html) = get("/")
        .header("host", "192.168.1.5:7878")
        .with_peer("192.168.1.9:50000")
        .send_raw(&app)
        .await;
    let html = String::from_utf8(html).unwrap();
    assert!(
        !html.contains("s3cret"),
        "remote clients must not receive the token"
    );
}

#[tokio::test]
async fn dashboard_is_embedded() {
    let (app, _) = app();
    let (status, html) = get("/").send_raw(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(html).unwrap().contains("<html"));
}

// ── HTTP: behaviour ───────────────────────────────────────────────────────────

#[tokio::test]
async fn find_validates_type_case_insensitively() {
    let (app, _) = app();
    let (status, body) = get("/windows/42/find?type=button").send(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["oculos_id"], "btn-save");

    let (status, body) = get("/windows/42/find?type=Buton").send(&app).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_input");
    assert!(body["error"].as_str().unwrap().contains("Valid types"));
}

#[tokio::test]
async fn errors_carry_status_and_code() {
    let (app, _) = app();
    let (status, body) = post("/interact/nope/click", None).send(&app).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["success"], false);
    assert_eq!(body["code"], "not_found");

    let (status, body) = post("/interact/btn-save/toggle", None).send(&app).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "unsupported");

    let (status, body) = get("/windows/abc/tree").send(&app).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_input");

    let (status, body) = get("/no/such/route").send(&app).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn malformed_json_gets_the_standard_envelope() {
    let (app, backend) = app();
    let (status, body) = post("/interact/edit-body/set-text", Some(json!({ "txt": 1 })))
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_input");
    assert!(backend.calls().is_empty());
}

#[tokio::test]
async fn send_keys_is_validated_before_typing() {
    let (app, backend) = app();
    let (status, body) = post(
        "/interact/edit-body/send-keys",
        Some(json!({ "keys": "hello{NOPE}" })),
    )
    .send(&app)
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_input");
    assert!(backend.calls().is_empty(), "nothing may be typed");

    let (status, _) = post(
        "/interact/edit-body/send-keys",
        Some(json!({ "keys": "{CTRL+A}hi{SPACE}{WIN+D}" })),
    )
    .send(&app)
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(backend.calls(), vec!["send_keys edit-body 4"]);
}

#[tokio::test]
async fn scroll_direction_is_validated() {
    let (app, backend) = app();
    let (status, _) = post(
        "/interact/edit-body/scroll",
        Some(json!({ "direction": "sideways" })),
    )
    .send(&app)
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = post(
        "/interact/edit-body/scroll",
        Some(json!({ "direction": "Down" })),
    )
    .send(&app)
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["direction"], "down");
    assert_eq!(backend.calls(), vec!["scroll edit-body down"]);
}

#[tokio::test]
async fn batch_validates_everything_first() {
    let (app, backend) = app();
    let (status, body) = post(
        "/interact/batch",
        Some(json!({ "actions": [
            { "element_id": "btn-save", "action": "click" },
            { "element_id": "edit-body", "action": "set-text" }
        ]})),
    )
    .send(&app)
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("actions[1]"));
    assert!(backend.calls().is_empty());
}

#[tokio::test]
async fn batch_stops_at_first_failure_by_default() {
    let (app, backend) = app();
    let steps = json!([
        { "element_id": "edit-body", "action": "set-text", "text": "hi" },
        { "element_id": "gone", "action": "click" },
        { "element_id": "btn-save", "action": "click" }
    ]);
    let (status, body) = post("/interact/batch", Some(json!({ "actions": steps })))
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["data"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[1]["success"], false);
    assert_eq!(results[1]["code"], "not_found");
    assert_eq!(backend.calls(), vec!["set_text edit-body hi"]);

    let (_, body) = post(
        "/interact/batch",
        Some(json!({ "actions": steps, "stop_on_error": false })),
    )
    .send(&app)
    .await;
    assert_eq!(body["data"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn wait_supports_gone_and_times_out() {
    let (app, _) = app();
    let (status, body) = get("/windows/42/wait?q=nothing-like-this&until=gone")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!([]));

    let (status, body) = get("/windows/42/wait?q=nothing-like-this&timeout=0")
        .send(&app)
        .await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(body["code"], "timeout");

    let (status, body) = get("/hwnd/7/wait?q=save").send(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["label"], "Save");
}

#[tokio::test]
async fn screenshot_is_png() {
    let (app, _) = app();
    let (status, bytes) = get("/windows/42/screenshot").send_raw(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.starts_with(b"\x89PNG"));

    let (status, body) = get("/interact/btn-save/screenshot").send(&app).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "unsupported");
}

// ── MCP ───────────────────────────────────────────────────────────────────────

fn mcp() -> (McpServer, Arc<MockBackend>) {
    let backend = Arc::new(MockBackend::default());
    (McpServer::new(backend.clone()), backend)
}

fn call(server: &McpServer, msg: Value) -> Option<Value> {
    server.handle_message(&msg.to_string())
}

fn tool(server: &McpServer, name: &str, args: Value) -> Value {
    call(
        server,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": args } }),
    )
    .unwrap()["result"]
        .clone()
}

#[test]
fn mcp_initialize_negotiates_version() {
    let (s, _) = mcp();
    let r = call(
        &s,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "capabilities": {} } }),
    )
    .unwrap();
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert!(r["result"]["instructions"].as_str().is_some());

    let r = call(
        &s,
        json!({ "jsonrpc": "2.0", "id": "x", "method": "initialize",
                "params": { "protocolVersion": "1999-01-01" } }),
    )
    .unwrap();
    assert_eq!(r["id"], "x");
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
}

#[test]
fn mcp_notifications_get_no_response() {
    let (s, _) = mcp();
    assert!(call(
        &s,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .is_none());
    assert!(call(
        &s,
        json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": {} })
    )
    .is_none());
}

#[test]
fn mcp_ping_and_unknown_methods() {
    let (s, _) = mcp();
    let r = call(&s, json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" })).unwrap();
    assert_eq!(r["result"], json!({}));
    let r = call(&s, json!({ "jsonrpc": "2.0", "id": 6, "method": "nope" })).unwrap();
    assert_eq!(r["error"]["code"], -32601);
    let r = s.handle_message("{not json").unwrap();
    assert_eq!(r["error"]["code"], -32700);
}

#[test]
fn mcp_tools_have_annotations_and_unique_names() {
    let (s, _) = mcp();
    let r = call(
        &s,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .unwrap();
    let tools = r["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for t in tools {
        assert!(
            t["annotations"]["readOnlyHint"].is_boolean(),
            "{}",
            t["name"]
        );
        assert_eq!(t["inputSchema"]["type"], "object");
    }
    for expected in [
        "wait_for_element",
        "screenshot_window",
        "batch_actions",
        "highlight_element",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), total, "duplicate tool names");
}

#[test]
fn mcp_tool_errors_are_results() {
    let (s, _) = mcp();
    let r = tool(&s, "click_element", json!({ "id": "missing" }));
    assert_eq!(r["isError"], true);
    assert!(r["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("not_found"));

    let r = tool(
        &s,
        "send_keys",
        json!({ "id": "edit-body", "keys": "{NOPE}" }),
    );
    assert_eq!(r["isError"], true);

    let r = call(
        &s,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "no_such_tool", "arguments": {} } }),
    )
    .unwrap();
    assert_eq!(r["error"]["code"], -32602);
}

#[test]
fn mcp_find_output_is_compact() {
    let (s, _) = mcp();
    let r = tool(&s, "find_elements", json!({ "pid": 42, "query": "save" }));
    assert_eq!(r["isError"], false);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("null"), "{text}");
    assert!(!text.contains("\"enabled\""), "{text}");
    assert!(!text.contains('\n'), "should not be pretty-printed");
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed[0]["oculos_id"], "btn-save");
}

#[test]
fn mcp_find_limit_truncates() {
    let (s, _) = mcp();
    let r = tool(&s, "find_elements", json!({ "pid": 42, "limit": 1 }));
    let parsed: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed["elements"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["total"], 4);
}

#[test]
fn mcp_actions_batch_and_images() {
    let (s, backend) = mcp();
    let r = tool(
        &s,
        "set_text",
        json!({ "id": "edit-body", "text": "héllo" }),
    );
    assert_eq!(r["isError"], false);

    let r = tool(
        &s,
        "batch_actions",
        json!({ "actions": [
            { "element_id": "btn-save", "action": "click" },
            { "element_id": "missing", "action": "click" }
        ]}),
    );
    assert_eq!(r["isError"], true);

    let r = tool(&s, "screenshot_window", json!({ "pid": 42 }));
    assert_eq!(r["content"][0]["type"], "image");
    assert_eq!(r["content"][0]["mimeType"], "image/png");

    let r = tool(&s, "get_ui_tree", json!({ "pid": 42, "max_depth": 0 }));
    let parsed: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(parsed.get("children").is_none());

    assert_eq!(
        backend.calls(),
        vec!["set_text edit-body héllo", "click btn-save"]
    );
}
