pub mod config;
mod instance_lock;
pub mod services;
pub mod structures;
pub mod views;

use sentry::{types::Dsn, ClientOptions};
use std::{net::SocketAddr, str::FromStr, time::Duration};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use crate::views::{cleanup_stale_archives, get_router, run_cache_maintenance};

fn sentry_layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    sentry_tracing::ErrorLayer::new().with_filter(tracing_subscriber::filter::LevelFilter::ERROR)
}

fn sentry_span_layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    sentry_tracing::SpanLayer::new().with_filter(tracing_subscriber::filter::LevelFilter::INFO)
}

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

    let _instance_lock = instance_lock::acquire(&config::CONFIG.data_dir)
        .expect("DATA_DIR is already in use or cannot be locked");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind webserver address 0.0.0.0:8080");
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
        let mut options = ClientOptions::new();
        options.dsn = Some(Dsn::from_str(dsn).expect("SENTRY_DSN must be a valid DSN string"));
        sentry::init(options)
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(sentry_span_layer())
        .with(sentry_layer())
        .init();

    start_app().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentry_captures_only_errors_on_the_configured_hub() {
        let events = sentry::test::with_captured_events(|| {
            let subscriber = tracing_subscriber::registry()
                .with(sentry_span_layer())
                .with(sentry_layer());
            tracing::subscriber::with_default(subscriber, || {
                tracing::trace!("ignored trace");
                tracing::debug!("ignored debug");
                tracing::info!("ignored info");
                tracing::warn!("ignored warning");
                tracing::error!("captured error");
            });
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, sentry::Level::Error);
        assert_eq!(events[0].message.as_deref(), Some("captured error"));
        assert!(events[0].breadcrumbs.is_empty());
    }

    #[test]
    fn sentry_retains_info_span_context_but_excludes_debug_spans() {
        let events = sentry::test::with_captured_events(|| {
            let subscriber = tracing_subscriber::registry()
                .with(sentry_span_layer())
                .with(sentry_layer());
            tracing::subscriber::with_default(subscriber, || {
                assert!(sentry::configure_scope(|scope| scope.get_span()).is_none());
                let _info = tracing::info_span!("retained info").entered();
                assert!(sentry::configure_scope(|scope| scope.get_span()).is_some());
                tracing::error!("info context");
                {
                    let _debug = tracing::debug_span!("excluded debug").entered();
                    tracing::error!("same info context");
                }
            });
            assert!(sentry::configure_scope(|scope| scope.get_span()).is_none());
        });
        assert_eq!(events.len(), 2);
        let trace = events[0].contexts.get("trace").expect("INFO trace context");
        assert_eq!(Some(trace), events[1].contexts.get("trace"));
        assert!(events.iter().all(|event| event.breadcrumbs.is_empty()));
    }
}
