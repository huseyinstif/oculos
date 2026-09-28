use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::api::AppState;

/// Shared broadcast channel for pushing events to all connected WebSocket clients.
pub type WsBroadcast = Arc<broadcast::Sender<WsEvent>>;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum WsEvent {
    /// Fired after every interaction (single or batch), successful or not.
    #[serde(rename = "action")]
    Action {
        action: String,
        element_id: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Fired after the window list was fetched.
    #[serde(rename = "windows")]
    Windows { count: usize },
    /// Fired after a UI tree was built.
    #[serde(rename = "tree_loaded")]
    TreeLoaded {
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hwnd: Option<usize>,
        nodes: usize,
    },
}

impl WsEvent {
    pub fn action(action: &str, element_id: &str, result: &Result<(), String>) -> Self {
        WsEvent::Action {
            action: action.to_string(),
            element_id: element_id.to_string(),
            success: result.is_ok(),
            error: result.as_ref().err().cloned(),
        }
    }
}

pub fn create_broadcast() -> WsBroadcast {
    let (tx, _) = broadcast::channel(256);
    Arc::new(tx)
}

/// GET /ws — upgrade to WebSocket
pub async fn ws_handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, s))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.ws_tx.subscribe();

    let welcome = serde_json::json!({
        "type": "connected",
        "data": { "message": "OculOS WebSocket connected" }
    });
    if socket
        .send(Message::Text(welcome.to_string()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            event = rx.recv() => {
                let json = match event {
                    Ok(event) => match serde_json::to_string(&event) {
                        Ok(json) => json,
                        Err(_) => continue,
                    },
                    // A slow client missed events: tell it, keep streaming.
                    Err(RecvError::Lagged(skipped)) => serde_json::json!({
                        "type": "lagged",
                        "data": { "skipped": skipped }
                    })
                    .to_string(),
                    Err(RecvError::Closed) => break,
                };
                if socket.send(Message::Text(json)).await.is_err() {
                    break; // client disconnected
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Ping(d))) => {
                        let _ = socket.send(Message::Pong(d)).await;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {} // ignore text/binary from client
                }
            }
        }
    }
}
