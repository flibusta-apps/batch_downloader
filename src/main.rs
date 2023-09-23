pub mod views;
pub mod config;
pub mod services;
pub mod structures;

use std::{net::SocketAddr, str::FromStr};
use sentry::{ClientOptions, types::Dsn, integrations::debug_images::DebugImagesIntegration};
use tokio_cron_scheduler::{JobScheduler, Job};
use tracing::info;

use crate::{views::get_router, services::files_cleaner::clean_files};


async fn start_app() {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router().await;

    info!("Start webserver...");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
    info!("Webserver shutdown...");
}


async fn start_job_scheduler() {
    let job_scheduler = JobScheduler::new().await.unwrap();

    let clean_files_job = match Job::new_async("0 */5 * * * *", |_uuid, _l| Box::pin(async {
        match clean_files(config::CONFIG.minio_bucket.clone()).await {
            Ok(_) => info!("Archive files cleaned!"),
            Err(err) => info!("Clean archive files err: {:?}", err),
        };

        match clean_files(config::CONFIG.minio_share_books_bucket.clone()).await {
            Ok(_) => info!("Share files cleaned!"),
            Err(err) => info!("Clean share files err: {:?}", err),
        };
    })) {
        Ok(v) => v,
        Err(err) => panic!("{:?}", err),
    };

    job_scheduler.add(clean_files_job).await.unwrap();

    info!("Scheduler start...");
    match job_scheduler.start().await {
        Ok(v) => v,
        Err(err) => panic!("{:?}", err),
    };
}


#[tokio::main]
async fn main() {
    let options = ClientOptions {
        dsn: Some(Dsn::from_str(&config::CONFIG.sentry_dsn).unwrap()),
        default_integrations: false,
        ..Default::default()
    }
    .add_integration(DebugImagesIntegration::new());

    let _guard = sentry::init(options);

    tokio::join![
        start_app(),
        start_job_scheduler()
    ];
}
