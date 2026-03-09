//! Bearer token authentication middleware for OculOS HTTP API.

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use tracing::info;

/// Resolve the API key: use the provided value, or generate a random one.
pub fn resolve_api_key(provided: Option<String>) -> String {
    match provided {
        Some(key) if !key.is_empty() => {
            info!("[OculOS] Using provided API key.");
            key
        }
        _ => {
            let key = generate_key();
            info!("[OculOS] No API key provided. Generated key: {}", key);
            info!("[OculOS] Pass it with --api-key or set OCULOS_API_KEY env variable.");
            key
        }
    }
}

/// Generate a cryptographically random API key: `oculos_<32 bytes hex>`.
fn generate_key() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes);
    format!("oculos_{}", hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Axum middleware that validates the API key on every request.
///
/// Checks in order:
/// 1. `Authorization: Bearer <KEY>` header
/// 2. `?token=<KEY>` query parameter (needed for browser WebSocket connections)
pub async fn auth_middleware(request: Request, next: Next) -> Response {
    let expected = request
        .extensions()
        .get::<ApiKey>()
        .map(|k| k.0.clone());

    let expected = match expected {
        Some(k) => k,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Internal Server Error", "message": "Auth not configured"}))).into_response(),
    };

    // 1. Check Authorization header
    let header_key = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));

    if let Some(key) = header_key {
        if constant_time_eq(key, &expected) {
            return next.run(request).await;
        }
    }

    // 2. Check ?token= query parameter (for WebSocket from browsers)
    if let Some(query) = request.uri().query() {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("token=") {
                let decoded = urlencoding_decode(value);
                if constant_time_eq(&decoded, &expected) {
                    return next.run(request).await;
                }
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "Unauthorized",
            "message": "Invalid or missing API key"
        })),
    )
        .into_response()
}

/// Minimal percent-decoding for the token query parameter.
fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(0);
            let lo = chars.next().unwrap_or(0);
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                result.push((h << 4 | l) as char);
            }
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Constant-time string comparison to prevent timing attacks.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() { return false; }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Newtype wrapper so we can insert the key into request extensions.
#[derive(Clone)]
pub struct ApiKey(pub String);
