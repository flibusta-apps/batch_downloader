use std::time::Duration;

use axum::{
    body::Body,
    extract::Path,
    http::{self, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum_prometheus::PrometheusMetricLayer;
use moka::{future::Cache, notification::RemovalCause};
use once_cell::sync::Lazy;
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tower_http::trace::{self, TraceLayer};

use tracing::Level;

use crate::{
    config::CONFIG,
    services::{task_creator::create_task, utils::get_key},
    structures::{CreateTask, Task, TaskStatus},
};

pub static TASK_RESULTS: Lazy<Cache<String, Task>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(3 * 60 * 60))
        .max_capacity(2048)
        .async_eviction_listener(|_key, value: Task, reason| {
            Box::pin(async move {
                if reason == RemovalCause::Replaced {
                    return;
                }

                let _ = tokio::fs::remove_file(format!("/tmp/{}", value.id)).await;
            })
        })
        .build()
});

async fn create_archive_task(Json(data): Json<CreateTask>) -> impl IntoResponse {
    let key = get_key(data.clone());

    let result = match TASK_RESULTS.get(&key).await {
        Some(result) => {
            if result.status == TaskStatus::Failed {
                create_task(data).await
            } else {
                result
            }
        }
        None => create_task(data).await,
    };

    Json::<Task>(result).into_response()
}

async fn check_archive_task_status(Path(task_id): Path<String>) -> impl IntoResponse {
    match TASK_RESULTS.get(&task_id).await {
        Some(result) => Json::<Task>(result).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn auth(req: Request<axum::body::Body>, next: Next) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    let auth_header = if let Some(auth_header) = auth_header {
        auth_header
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if auth_header != CONFIG.api_key {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

async fn download(Path(task_id): Path<String>) -> impl IntoResponse {
    let task = match TASK_RESULTS.get(&task_id).await {
        Some(result) => result,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let file = match File::open(format!("/tmp/{}", task.id)).await {
        Ok(v) => v,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let stream = ReaderStream::new(file);

    Body::from_stream(stream).into_response()
}

pub async fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = Router::new()
        .route("/api/", post(create_archive_task))
        .route(
            "/api/check_archive/{task_id}",
            get(check_archive_task_status),
        )
        .layer(middleware::from_fn(auth))
        .layer(prometheus_layer);

    let metric_router =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));

    Router::new()
        .route("/api/download/{task_id}", get(download))
        .nest("/", app_router)
        .nest("/", metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
}
