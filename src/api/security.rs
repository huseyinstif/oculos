//! Request guards that keep other websites and processes from driving the
//! desktop through the local API.
//!
//! - **Host check** (always on): the `Host` header must be an IP literal,
//!   `localhost`, or explicitly allowed. This defeats DNS-rebinding, where a
//!   malicious domain is re-pointed at 127.0.0.1 so the browser treats our
//!   API as same-origin.
//! - **Origin check** (always on): browsers attach `Origin` to cross-origin
//!   requests (including "simple" POSTs that skip CORS preflight and
//!   WebSocket handshakes). It must match the Host (same-origin, i.e. our own
//!   dashboard) or be explicitly allowed.
//! - **Token** (optional): when configured, every protected route needs
//!   `Authorization: Bearer <token>`, `X-OculOS-Token: <token>` or
//!   `?token=<token>` (WebSocket).

use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::{ApiError, ServerConfig};

pub const TOKEN_HEADER: &str = "x-oculos-token";

pub async fn check_host_and_origin(
    State(config): State<Arc<ServerConfig>>,
    req: Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    if let Some(host) = &host {
        if !host_allowed(host, &config.allowed_hosts) {
            return forbidden(format!(
                "Host '{host}' is not allowed. Use 127.0.0.1/localhost or start OculOS with --allow-host."
            ));
        }
    }

    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("null");
        if !origin_allowed(origin, host.as_deref(), &config.allowed_origins) {
            return forbidden(format!(
                "Origin '{origin}' is not allowed. Start OculOS with --allow-origin to permit it."
            ));
        }
    }

    next.run(req).await
}

pub async fn require_token(
    State(config): State<Arc<ServerConfig>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &config.token {
        let presented = token_from_request(req.headers(), req.uri().query());
        let valid = presented.is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()));
        if !valid {
            return ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Missing or invalid API token. Send 'Authorization: Bearer <token>' or 'X-OculOS-Token: <token>'.",
            )
            .into_response();
        }
    }
    next.run(req).await
}

fn forbidden(message: String) -> Response {
    ApiError::new(StatusCode::FORBIDDEN, "forbidden", message).into_response()
}

/// Strip the port from a Host header value ("[::1]:7878" → "::1").
fn host_name(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

pub fn host_allowed(host: &str, extra: &[String]) -> bool {
    let name = host_name(host);
    name.parse::<IpAddr>().is_ok()
        || name.eq_ignore_ascii_case("localhost")
        || extra.iter().any(|h| h.eq_ignore_ascii_case(name))
}

pub fn origin_allowed(origin: &str, host: Option<&str>, extra: &[String]) -> bool {
    let origin = origin.trim().trim_end_matches('/');
    if origin.eq_ignore_ascii_case("null") || origin.is_empty() {
        return false;
    }
    if extra
        .iter()
        .any(|o| o.trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        return true;
    }
    match host {
        Some(h) => {
            let h = h.trim();
            origin.eq_ignore_ascii_case(&format!("http://{h}"))
                || origin.eq_ignore_ascii_case(&format!("https://{h}"))
        }
        None => false,
    }
}

fn token_from_request<'a>(headers: &'a HeaderMap, query: Option<&'a str>) -> Option<&'a str> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            return Some(t.trim());
        }
    }
    if let Some(v) = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
        return Some(v.trim());
    }
    query?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "token")
        .map(|(_, v)| v)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A random 32-hex-character token.
pub fn generate_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert!(host_allowed("127.0.0.1:7878", &[]));
        assert!(host_allowed("localhost:7878", &[]));
        assert!(host_allowed("LOCALHOST", &[]));
        assert!(host_allowed("[::1]:7878", &[]));
        assert!(host_allowed("192.168.1.20:7878", &[]));
        assert!(!host_allowed("evil.example:7878", &[]));
        assert!(!host_allowed("localhost.evil.example", &[]));
        assert!(host_allowed("mybox:7878", &["mybox".into()]));
    }

    #[test]
    fn origins() {
        let host = Some("127.0.0.1:7878");
        assert!(origin_allowed("http://127.0.0.1:7878", host, &[]));
        assert!(!origin_allowed("http://localhost:7878", host, &[]));
        assert!(!origin_allowed("https://evil.example", host, &[]));
        assert!(!origin_allowed("null", host, &[]));
        assert!(origin_allowed(
            "http://localhost:3000/",
            host,
            &["http://localhost:3000".into()]
        ));
    }

    #[test]
    fn tokens() {
        let mut h = HeaderMap::new();
        assert_eq!(token_from_request(&h, Some("a=1&token=abc")), Some("abc"));
        h.insert(TOKEN_HEADER, "xyz".parse().unwrap());
        assert_eq!(token_from_request(&h, None), Some("xyz"));
        h.insert(header::AUTHORIZATION, "Bearer tok".parse().unwrap());
        assert_eq!(token_from_request(&h, None), Some("tok"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert_eq!(generate_token().len(), 32);
    }
}
