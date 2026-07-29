pub mod config;
pub mod services;
pub mod structures;
pub mod views;

use sentry::{integrations::debug_images::DebugImagesIntegration, types::Dsn, ClientOptions};
use sentry_tracing::EventFilter;
use std::{net::SocketAddr, str::FromStr};
use tracing::info;
use tracing_subscriber::{filter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::views::get_router;

async fn start_app() {
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router().await;

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind webserver address 0.0.0.0:8080");
    axum::serve(listener, app).await.expect("axum server error");
    info!("Webserver shutdown...");
}

#[tokio::main]
async fn main() {
    let options = ClientOptions {
        dsn: Some(
            Dsn::from_str(&config::CONFIG.sentry_dsn)
                .expect("SENTRY_DSN must be a valid DSN string"),
        ),
        default_integrations: false,
        ..Default::default()
    }
    .add_integration(DebugImagesIntegration::new());

    let _guard = sentry::init(options);

    let sentry_layer = sentry_tracing::layer().event_filter(|md| match md.level() {
        &tracing::Level::ERROR => EventFilter::Event,
        _ => EventFilter::Ignore,
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(filter::LevelFilter::INFO)
        .with(sentry_layer)
        .init();

    start_app().await
}
