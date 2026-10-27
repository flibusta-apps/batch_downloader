pub mod config;
pub mod services;
pub mod structures;
pub mod views;

use sentry::{types::Dsn, ClientOptions};
use sentry_tracing::EventFilter;
use std::{net::SocketAddr, str::FromStr, time::Duration};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::views::{cleanup_stale_archives, get_router, run_cache_maintenance};

/// Resolves on the first of SIGINT (ctrl_c) or SIGTERM, whichever arrives
/// first, so `axum::serve` can stop accepting new connections and let
/// in-flight requests drain instead of being killed mid-response.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl_c signal handler");
    };

    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received, draining in-flight requests...");
}

async fn start_app() {
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    cleanup_stale_archives().await;

    let app = get_router().await;

    tokio::spawn(async {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            run_cache_maintenance().await;
        }
    });

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind webserver address 0.0.0.0:8080");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("axum server error");
    info!("Webserver shutdown...");
}

#[tokio::main]
async fn main() {
    // Sentry is optional; the default integrations (incl. panic reporting)
    // stay enabled.
    let _guard = config::CONFIG.sentry_dsn.as_deref().map(|dsn| {
        sentry::init(ClientOptions {
            dsn: Some(Dsn::from_str(dsn).expect("SENTRY_DSN must be a valid DSN string")),
            ..Default::default()
        })
    });

    let sentry_layer = sentry_tracing::layer().event_filter(|md| match md.level() {
        &tracing::Level::ERROR => EventFilter::Event,
        _ => EventFilter::Ignore,
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(sentry_layer)
        .init();

    start_app().await
}
