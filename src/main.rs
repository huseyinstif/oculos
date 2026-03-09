mod api;
mod auth;
mod mcp;
mod platform;
mod types;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use axum::Router;
use clap::Parser;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
    trace::TraceLayer,
};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use api::AppState;
use platform::{PlatformBackend, UiBackend};

/// OculOS — "If it's on the screen, it's an API."
#[derive(Parser, Debug)]
#[command(name = "oculos", version, about = "Universal UI automation API server")]
struct Args {
    /// Address to bind the API server on.
    #[arg(short, long, default_value = "127.0.0.1:7878")]
    bind: SocketAddr,

    /// Path to the static dashboard files.
    #[arg(long, default_value = "static")]
    static_dir: String,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    log: String,

    /// Run as an MCP server over stdin/stdout instead of an HTTP server.
    /// Add this binary to your MCP host config (Claude, Cursor, Windsurf…).
    #[arg(long)]
    mcp: bool,

    /// API key for authenticating HTTP requests. If not provided, a random key
    /// is generated at startup. Can also be set via OCULOS_API_KEY env variable.
    #[arg(long, env = "OCULOS_API_KEY")]
    api_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // ── Logging ───────────────────────────────────────────────────────────────
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| args.log.as_str().into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // ── Platform backend ──────────────────────────────────────────────────────
    info!("Initialising platform UI backend…");
    let backend: Arc<dyn UiBackend> =
        Arc::new(PlatformBackend::new().expect("Failed to initialise UI Automation backend"));
    info!("Backend ready.");

    // ── MCP mode ──────────────────────────────────────────────────────────────
    if args.mcp {
        info!("OculOS MCP server starting on stdio (protocol version 2024-11-05)");
        tokio::task::spawn_blocking(move || mcp::run_mcp(backend)).await??;
        return Ok(());
    }

    // ── API key ───────────────────────────────────────────────────────────────
    let api_key = auth::resolve_api_key(args.api_key);

    let ws_tx = api::ws::create_broadcast();
    let state = AppState { backend, ws_tx };

    // ── CORS ──────────────────────────────────────────────────────────────────
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // ── Router ────────────────────────────────────────────────────────────────
    let api_routes = api::router(state, api_key.clone());

    let app = Router::new()
        // API routes under /api namespace (also available at root for simplicity)
        .merge(api_routes)
        // Dashboard — served at / (no auth required for static files)
        .nest_service("/", ServeDir::new(&args.static_dir))
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    // ── Serve ─────────────────────────────────────────────────────────────────
    info!("");
    info!("╔══════════════════════════════════════════════════╗");
    info!("║          OculOS is running                       ║");
    info!("║  \"If it's on the screen, it's an API.\"          ║");
    info!("╠══════════════════════════════════════════════════╣");
    info!("║  Dashboard →  http://{}            ║", args.bind);
    info!("║  API        →  http://{}/windows   ║", args.bind);
    info!("╚══════════════════════════════════════════════════╝");
    info!("");

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
