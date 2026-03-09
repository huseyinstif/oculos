use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::api::AppState;

/// Shared broadcast channel for pushing events to all connected WebSocket clients.
pub type WsBroadcast = Arc<broadcast::Sender<WsEvent>>;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum WsEvent {
    /// Fired after any interaction endpoint is called.
    #[serde(rename = "action")]
    Action {
        action: String,
        element_id: String,
        success: bool,
    },
    /// Periodic window list snapshot.
    #[serde(rename = "windows")]
    Windows { count: usize },
    /// Signals that a tree was loaded for a PID.
    #[serde(rename = "tree_loaded")]
    TreeLoaded { pid: u32 },
}

pub fn create_broadcast() -> WsBroadcast {
    let (tx, _) = broadcast::channel(256);
    Arc::new(tx)
}

/// GET /ws — upgrade to WebSocket.
/// Auth is handled by the auth middleware layer (supports both
/// `Authorization: Bearer` header and `?token=` query parameter).
pub async fn ws_handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, s))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.ws_tx.subscribe();

    // Send a welcome message
    let welcome = serde_json::json!({
        "type": "connected",
        "data": { "message": "OculOS WebSocket connected" }
    });
    let _ = socket.send(Message::Text(welcome.to_string())).await;

    // Forward broadcast events to this client
    loop {
        tokio::select! {
            // Event from broadcast channel → send to client
            Ok(event) = rx.recv() => {
                if let Ok(json) = serde_json::to_string(&event) {
                    if socket.send(Message::Text(json)).await.is_err() {
                        break; // client disconnected
                    }
                }
            }
            // Message from client (we accept pings/pongs, ignore text)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Ping(d))) => {
                        let _ = socket.send(Message::Pong(d)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore text/binary from client
                }
            }
        }
    }
}
