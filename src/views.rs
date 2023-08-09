use std::time::Duration;

use axum::{Router, routing::{get, post}, middleware::{self, Next}, http::{Request, StatusCode, self}, response::{Response, IntoResponse}, extract::{Path, self}, Json};
use axum_prometheus::PrometheusMetricLayer;
use moka::future::Cache;
use once_cell::sync::Lazy;
use tower_http::trace::{TraceLayer, self};

use tracing::Level;

use crate::{config::CONFIG, structures::{Task, CreateTask, TaskStatus}, services::{task_creator::create_task, utils::get_key}};


pub static TASK_RESULTS: Lazy<Cache<String, Task>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(12 * 60 * 60))
        .max_capacity(2048)
        .build()
});


async fn create_archive_task(
    extract::Json(data): extract::Json<CreateTask>
) -> impl IntoResponse {
    let key = get_key(data.clone());

    let result = match TASK_RESULTS.get(&key) {
        Some(result) => {
            if result.status == TaskStatus::Failled {
                 create_task(data).await
            } else {
                result
            }
        },
        None => create_task(data).await,
    };

    Json::<Task>(result).into_response()
}


async fn check_archive_task_status(
    Path(task_id): Path<String>
) -> impl IntoResponse {
    match TASK_RESULTS.get(&task_id) {
        Some(result) => Json::<Task>(result).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}


async fn auth<B>(req: Request<B>, next: Next<B>) -> Result<Response, StatusCode> {
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


pub async fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = Router::new()
        .route("/api/", post(create_archive_task))
        .route("/api/check_archive/:task_id", get(check_archive_task_status))
        .layer(middleware::from_fn(auth))
        .layer(prometheus_layer);

    let metric_router =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));

    Router::new()
        .nest("/", app_router)
        .nest("/", metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
}
