//! The web dashboard, embedded in the binary so release builds and containers
//! don't depend on a `static/` directory next to the executable.

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderName, StatusCode},
    response::IntoResponse,
};

use super::AppState;

const INDEX_HTML: &str = include_str!("../../static/index.html");
const TOKEN_META_EMPTY: &str = r#"<meta name="oculos-token" content="">"#;

/// GET / — the dashboard page.
///
/// For clients on this machine (loopback peer) the API token, if any, is
/// filled in so the dashboard just works; the Host/Origin guard stops other
/// websites from reading the page. Remote clients never get the token from
/// the server — they open `/?token=<token>` and the page picks it up.
pub async fn index(
    State(state): State<AppState>,
    peer: Option<ConnectInfo<SocketAddr>>,
) -> impl IntoResponse {
    let html = match &state.config.static_dir {
        Some(dir) => tokio::fs::read_to_string(dir.join("index.html"))
            .await
            .unwrap_or_else(|_| INDEX_HTML.to_string()),
        None => INDEX_HTML.to_string(),
    };
    let local = peer.is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback());
    let token = match &state.config.token {
        Some(t) if local => t.as_str(),
        _ => "",
    };

    const HEADERS: [(HeaderName, &str); 5] = [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
    ];
    (StatusCode::OK, HEADERS, inject_token(&html, token))
}

fn inject_token(html: &str, token: &str) -> String {
    let meta = format!(
        r#"<meta name="oculos-token" content="{}">"#,
        html_escape(token)
    );
    if html.contains(TOKEN_META_EMPTY) {
        html.replacen(TOKEN_META_EMPTY, &meta, 1)
    } else if let Some(pos) = html.find("<head>") {
        let mut out = html.to_string();
        out.insert_str(pos + "<head>".len(), &meta);
        out
    } else {
        html.to_string()
    }
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_injected_once_and_escaped() {
        let html = format!("<html><head>{TOKEN_META_EMPTY}</head></html>");
        let out = inject_token(&html, "a\"b");
        assert!(out.contains(r#"<meta name="oculos-token" content="a&quot;b">"#));
        let out = inject_token("<html><head></head></html>", "t");
        assert!(out.contains(r#"<head><meta name="oculos-token" content="t">"#));
    }
}
