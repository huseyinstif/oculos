mod api;
mod error;
mod keys;
mod mcp;
mod ops;
mod platform;
mod registry;
mod types;

#[cfg(test)]
mod tests;

use std::{io::IsTerminal, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use api::{AppState, ServerConfig};
use platform::{PlatformBackend, UiBackend};

/// OculOS — "If it's on the screen, it's an API."
#[derive(Parser, Debug)]
#[command(name = "oculos", version, about = "Universal UI automation API server")]
struct Args {
    /// Address to bind the API server on.
    #[arg(short, long, default_value = "127.0.0.1:7878")]
    bind: SocketAddr,

    /// Require this API token (header `Authorization: Bearer <token>` or
    /// `X-OculOS-Token`). A random token is generated automatically when
    /// binding to a non-loopback address without one.
    #[arg(long, env = "OCULOS_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Allow a browser origin (e.g. http://localhost:3000) to call the API.
    /// Repeatable. Enables CORS for exactly these origins.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    allow_origins: Vec<String>,

    /// Accept an additional Host header name (IP literals and `localhost`
    /// are always accepted). Repeatable.
    #[arg(long = "allow-host", value_name = "HOST")]
    allow_hosts: Vec<String>,

    /// Serve the dashboard from this directory instead of the copy embedded
    /// in the binary (useful while editing static/index.html).
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error). Logs go to stderr.
    #[arg(long, default_value = "info")]
    log: String,

    /// Run as an MCP server over stdin/stdout instead of an HTTP server.
    /// Add this binary to your MCP host config (Claude, Cursor, Windsurf…).
    #[arg(long)]
    mcp: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // ── Logging ───────────────────────────────────────────────────────────────
    // Always stderr: in MCP mode stdout carries the JSON-RPC stream.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| args.log.as_str().into()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal()),
        )
        .init();

    // ── Platform backend ──────────────────────────────────────────────────────
    info!("Initialising platform UI backend…");
    let backend: Arc<dyn UiBackend> =
        Arc::new(PlatformBackend::new().context("Failed to initialise the UI automation backend")?);
    info!("Backend ready.");

    // ── MCP mode ──────────────────────────────────────────────────────────────
    if args.mcp {
        info!("OculOS MCP server running on stdio");
        tokio::task::spawn_blocking(move || mcp::run_mcp(backend)).await??;
        return Ok(());
    }

    // ── Security ──────────────────────────────────────────────────────────────
    let mut token = args.token.filter(|t| !t.trim().is_empty());
    if token.is_none() && !args.bind.ip().is_loopback() {
        let generated = api::security::generate_token();
        warn!(
            "Binding to non-loopback address {} — an API token is required. Generated token: {}",
            args.bind, generated
        );
        token = Some(generated);
    }

    let config = ServerConfig {
        token,
        allowed_origins: args.allow_origins,
        allowed_hosts: args.allow_hosts,
        static_dir: args.static_dir,
    };
    let auth = config.token.is_some();
    let app = api::build_app(AppState::new(backend, config));

    // ── Serve ─────────────────────────────────────────────────────────────────
    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("Failed to bind {}", args.bind))?;

    info!("OculOS {} is running", env!("CARGO_PKG_VERSION"));
    info!("  Dashboard → http://{}/", args.bind);
    info!("  API       → http://{}/windows", args.bind);
    info!(
        "  Auth      → {}",
        if auth {
            "token required"
        } else {
            "loopback only (no token)"
        }
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;

    Ok(())
}
