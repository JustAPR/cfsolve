mod handlers;
mod models;
mod pool;
mod state;

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{EnvFilter, fmt};

use crate::handlers::{
    clear_handler, dashboard_handler, health_handler, index_handler, solve_handler,
    turnstile_handler,
};
use crate::state::AppState;

const HELP_MSG: &str = "\
cfsolve-server: High-performance async REST API server for Cloudflare Turnstile and challenge pages.

USAGE:
    cfsolve-server [OPTIONS]

OPTIONS:
    --host <IP>                Bind address (default: 0.0.0.0, or env HOST)
    --port <PORT>              Bind port (default: 3000, or env PORT)
    --profile-dir <DIR>        Directory with browser profiles (default: profiles, or env PROFILE_DIR)
    --default-profile <NAME>   Default browser profile (default: safari-26-ios, or env DEFAULT_PROFILE)
    -h, --help                 Display this help message
";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    fmt().with_env_filter(filter).init();

    // Parse CLI arguments & environment variables
    let mut host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let mut port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let mut profile_dir =
        PathBuf::from(env::var("PROFILE_DIR").unwrap_or_else(|_| "profiles".to_string()));
    let mut default_profile =
        env::var("DEFAULT_PROFILE").unwrap_or_else(|_| "safari-26-ios".to_string());

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => {
                if let Some(val) = args.next() {
                    host = val;
                }
            }
            "--port" => {
                if let Some(val) = args.next() {
                    port = val;
                }
            }
            "--profile-dir" => {
                if let Some(val) = args.next() {
                    profile_dir = PathBuf::from(val);
                }
            }
            "--default-profile" => {
                if let Some(val) = args.next() {
                    default_profile = val;
                }
            }
            "-h" | "--help" => {
                println!("{HELP_MSG}");
                return Ok(());
            }
            unknown => {
                eprintln!("Unknown argument: {unknown}\n\n{HELP_MSG}");
                std::process::exit(1);
            }
        }
    }

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("Invalid address {host}:{port}"))?;

    tracing::info!("Initializing cfsolve server state...");
    let state = AppState::new(profile_dir, default_profile)
        .await
        .context("Failed to initialize AppState")?;

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/dashboard", get(dashboard_handler))
        .route("/playground", get(dashboard_handler))
        .route("/health", get(health_handler))
        .route("/stats", get(health_handler))
        .route("/turnstile", post(turnstile_handler))
        .route("/clear", post(clear_handler))
        .route("/solve", post(solve_handler))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    tracing::info!("Starting server listening on http://{}", addr);
    tracing::info!("Endpoints available:");
    tracing::info!("  POST http://{}/turnstile  (Solve Turnstile widgets)", addr);
    tracing::info!("  POST http://{}/clear      (Solve Challenge pages / cf_clearance)", addr);
    tracing::info!("  POST http://{}/solve      (Auto-detect and solve)", addr);
    tracing::info!("  GET  http://{}/health     (Server metrics & profile status)", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind to {}", addr))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Server error")?;

    tracing::info!("Server shut down gracefully.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, shutting down...");
        },
        _ = terminate => {
            tracing::info!("Received SIGTERM, shutting down...");
        },
    }
}
