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
use subtle::ConstantTimeEq;
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

/// Maps the internal MD5 dedup key (`utils::get_key`) to the random download
/// token currently representing that request, so identical concurrent
/// requests reuse one task without ever exposing the guessable MD5 as
/// `Task.id`.
pub static DEDUP_INDEX: Lazy<Cache<String, String>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(3 * 60 * 60))
        .max_capacity(2048)
        .build()
});

async fn create_archive_task(
    headers: axum::http::HeaderMap,
    Json(mut data): Json<CreateTask>,
) -> impl IntoResponse {
    // Derive user_id exclusively from X-User-Id header (authoritative source).
    // Never trust user_id from the JSON body — it could allow impersonation.
    // If header is absent → None (anonymous), avoiding dummy sentinel values.
    data.user_id = headers
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());

    let dedup_key = get_key(data.clone());

    let existing_task = match DEDUP_INDEX.get(&dedup_key).await {
        Some(token) => TASK_RESULTS.get(&token).await,
        None => None,
    };

    let result = match existing_task {
        Some(task) if task.status != TaskStatus::Failed => task,
        _ => create_task(data, dedup_key).await,
    };

    Json::<Task>(result).into_response()
}

async fn check_archive_task_status(Path(task_id): Path<String>) -> impl IntoResponse {
    match TASK_RESULTS.get(&task_id).await {
        Some(result) => Json::<Task>(result).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn api_keys_match(provided: &str, expected: &str) -> bool {
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
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

    if !api_keys_match(auth_header, &CONFIG.api_key) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

async fn download(Path(task_id): Path<String>) -> impl IntoResponse {
    let task = match TASK_RESULTS.get(&task_id).await {
        Some(result) => result,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    if task.status != TaskStatus::Complete {
        return StatusCode::NOT_FOUND.into_response();
    }

    let file = match File::open(format!("/tmp/{}", task.id)).await {
        Ok(v) => v,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let stream = ReaderStream::new(file);

    Body::from_stream(stream).into_response()
}

async fn health_check() -> impl IntoResponse {
    StatusCode::OK
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

    let public_router = Router::new()
        .route("/api/download/{task_id}", get(download))
        .route("/health", get(health_check));

    Router::new()
        .merge(public_router)
        .merge(app_router)
        .merge(metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structures::ObjectType;
    use smallvec::smallvec;
    use std::sync::Once;
    use tower::ServiceExt;

    pub(super) fn ensure_test_env() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            std::env::set_var("API_KEY", "test-api-key");
            std::env::set_var("LIBRARY_API_KEY", "test-library-key");
            std::env::set_var("LIBRARY_URL", "http://localhost:0");
            std::env::set_var("CACHE_API_KEY", "test-cache-key");
            std::env::set_var("CACHE_URL", "http://localhost:0");
            std::env::set_var("SENTRY_DSN", "https://public@example.com/1");
        });
    }

    fn sample_task_data(object_id: u32) -> CreateTask {
        CreateTask {
            object_id,
            object_type: ObjectType::Author,
            file_format: "fb2".into(),
            allowed_langs: smallvec!["ru".into()],
            user_id: None,
            normalized: true,
        }
    }

    #[test]
    fn api_keys_match_uses_constant_time_comparison() {
        assert!(api_keys_match("secret-key", "secret-key"));
        assert!(!api_keys_match("secret-key", "wrong-key"));
        assert!(!api_keys_match("short", "much-longer-secret-key"));
    }

    #[tokio::test]
    async fn create_archive_task_reuses_existing_non_failed_task_via_dedup_index() {
        let data = sample_task_data(424242);
        let dedup_key = get_key(data.clone());
        let seeded_token = "seeded-token-reuse-test".to_string();

        TASK_RESULTS
            .insert(
                seeded_token.clone(),
                Task {
                    id: seeded_token.clone(),
                    status: TaskStatus::InProgress,
                    status_description: "Подготовка".into(),
                    error_message: None,
                    result_filename: None,
                    content_size: None,
                },
            )
            .await;
        DEDUP_INDEX
            .insert(dedup_key.clone(), seeded_token.clone())
            .await;

        let response = create_archive_task(axum::http::HeaderMap::new(), Json(data))
            .await
            .into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["id"].as_str().unwrap(), seeded_token);
        assert_ne!(json["id"].as_str().unwrap(), dedup_key);
    }

    #[tokio::test]
    async fn create_archive_task_generates_random_token_not_md5_key() {
        ensure_test_env();
        let data = sample_task_data(434343);
        let dedup_key = get_key(data.clone());

        let response = create_archive_task(axum::http::HeaderMap::new(), Json(data))
            .await
            .into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let id = json["id"].as_str().unwrap();

        assert_ne!(id, dedup_key);
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    async fn test_router() -> Router {
        static ROUTER: tokio::sync::OnceCell<Router> = tokio::sync::OnceCell::const_new();
        ROUTER
            .get_or_init(|| async { get_router().await })
            .await
            .clone()
    }

    #[tokio::test]
    async fn download_returns_404_for_in_progress_task() {
        ensure_test_env();
        let token = "test-download-in-progress".to_string();
        tokio::fs::write(format!("/tmp/{token}"), b"partial-zip")
            .await
            .unwrap();
        TASK_RESULTS
            .insert(
                token.clone(),
                Task {
                    id: token.clone(),
                    status: TaskStatus::InProgress,
                    status_description: "working".into(),
                    error_message: None,
                    result_filename: None,
                    content_size: None,
                },
            )
            .await;

        let app = test_router().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/download/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let _ = tokio::fs::remove_file(format!("/tmp/{token}")).await;
    }

    #[tokio::test]
    async fn download_streams_file_only_when_complete() {
        ensure_test_env();
        let token = "test-download-complete".to_string();
        tokio::fs::write(format!("/tmp/{token}"), b"zip-bytes")
            .await
            .unwrap();
        TASK_RESULTS
            .insert(
                token.clone(),
                Task {
                    id: token.clone(),
                    status: TaskStatus::Complete,
                    status_description: "done".into(),
                    error_message: None,
                    result_filename: Some("archive.zip".into()),
                    content_size: Some(9),
                },
            )
            .await;

        let app = test_router().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/download/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"zip-bytes");

        let _ = tokio::fs::remove_file(format!("/tmp/{token}")).await;
    }
}
