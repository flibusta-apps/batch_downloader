use std::{collections::HashMap, time::Duration};

use axum::{
    body::Body,
    extract::{MatchedPath, Path},
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
use tokio::{fs::File, sync::Mutex};
use tokio_util::io::ReaderStream;
use tower_http::trace::{self, TraceLayer};
use tracing::Level;

use crate::{
    config::{archive_path, CONFIG},
    services::task_creator::{create_or_reuse_task, run_archive_task, CreateTaskError, TaskCtx},
    structures::{CreateTask, Task, TaskStatus},
};

/// `time_to_idle` alone only bounds how long an *unpolled* task survives —
/// a client (or an attacker) that keeps polling `check_archive`/`download`
/// resets the idle timer indefinitely, so a download link could in theory
/// live forever. `time_to_live` adds a hard upper bound independent of
/// access patterns: 24 hours is comfortably longer than any realistic
/// archive build + download window (builds are minutes, downloads are
/// typically started within an hour of completion), while still giving a
/// concrete, documented expiry for "how long is a download link valid"
/// (see README's Operational notes section).
const TASK_RESULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Finished (complete / failed) tasks. Evictable; evicting a completed task
/// deletes its archive. Running tasks are NOT stored here (see `ACTIVE_TASKS`)
/// so they can never be evicted while still writing their file.
pub static TASK_RESULTS: Lazy<Cache<String, Task>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(3 * 60 * 60))
        .time_to_live(TASK_RESULT_TTL)
        .max_capacity(2048)
        .async_eviction_listener(|_key, value: Task, reason| {
            Box::pin(async move {
                if reason == RemovalCause::Replaced {
                    return;
                }

                if value.status == TaskStatus::Complete {
                    let _ = tokio::fs::remove_file(archive_path(&value.id)).await;
                }
            })
        })
        .build()
});

/// Tasks that are currently queued or running. Never evicted.
static ACTIVE_TASKS: Lazy<Mutex<HashMap<String, Task>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Stores the task state: in-progress tasks go to `ACTIVE_TASKS`, terminal
/// ones move to the evictable `TASK_RESULTS`.
pub async fn put_task(task: Task) {
    let token = task.id.clone();
    if task.status == TaskStatus::InProgress {
        ACTIVE_TASKS.lock().await.insert(token, task);
    } else {
        TASK_RESULTS.insert(token.clone(), task).await;
        ACTIVE_TASKS.lock().await.remove(&token);
    }
}

pub async fn get_task(token: &str) -> Option<Task> {
    if let Some(task) = ACTIVE_TASKS.lock().await.get(token) {
        return Some(task.clone());
    }
    TASK_RESULTS.get(token).await
}

/// Maps the internal MD5 dedup key (`utils::get_key`) to the random download
/// token currently representing that request, so identical concurrent
/// requests reuse one task without ever exposing the guessable MD5 as
/// `Task.id`.
pub static DEDUP_INDEX: Lazy<Cache<String, String>> = Lazy::new(|| {
    Cache::builder()
        .time_to_idle(Duration::from_secs(3 * 60 * 60))
        .time_to_live(TASK_RESULT_TTL)
        .max_capacity(2048)
        .build()
});

/// Runs pending cache maintenance (expiry + eviction listeners) so expired
/// archives are deleted even when the service is idle.
pub async fn run_cache_maintenance() {
    TASK_RESULTS.run_pending_tasks().await;
    DEDUP_INDEX.run_pending_tasks().await;
}

/// Removes orphaned archive files left in the dedicated data directory by a
/// previous process run (task state lives only in memory). Only the
/// application's own directory is touched.
pub async fn cleanup_stale_archives() {
    let dir = &CONFIG.data_dir;

    if let Err(err) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!(
            "cleanup_stale_archives: failed to create {}: {err}",
            dir.display()
        );
        return;
    }

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                "cleanup_stale_archives: failed to read {}: {err}",
                dir.display()
            );
            return;
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(err) => {
                tracing::warn!("cleanup_stale_archives: failed to read entry: {err}");
                continue;
            }
        };

        let is_file = entry
            .file_type()
            .await
            .map(|t| t.is_file())
            .unwrap_or(false);
        if !is_file {
            continue;
        }

        if let Err(err) = tokio::fs::remove_file(entry.path()).await {
            tracing::warn!(
                "cleanup_stale_archives: failed to remove stale archive {}: {err}",
                entry.path().display()
            );
        }
    }
}

async fn create_archive_task(
    headers: axum::http::HeaderMap,
    Json(mut data): Json<CreateTask>,
) -> Response {
    // Derive user_id exclusively from X-User-Id header (authoritative source).
    // Never trust user_id from the JSON body — it could allow impersonation.
    data.user_id = headers
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());

    create_archive_task_with(data, run_archive_task).await
}

async fn create_archive_task_with<F, Fut>(data: CreateTask, runner: F) -> Response
where
    F: FnOnce(TaskCtx, CreateTask) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    match create_or_reuse_task(data, runner).await {
        Ok(task) => Json::<Task>(task).into_response(),
        Err(CreateTaskError::QueueFull) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(http::header::RETRY_AFTER, "30")],
            "Too many tasks in queue",
        )
            .into_response(),
    }
}

async fn check_archive_task_status(Path(task_id): Path<String>) -> impl IntoResponse {
    match get_task(&task_id).await {
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

/// RFC 5987 `filename*` value (UTF-8, percent-encoded).
fn content_disposition(filename: &str) -> String {
    let mut encoded = String::new();
    for b in filename.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }

    let fallback: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();

    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

async fn download(Path(task_id): Path<String>) -> impl IntoResponse {
    let task = match TASK_RESULTS.get(&task_id).await {
        Some(result) => result,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    if task.status != TaskStatus::Complete {
        return StatusCode::NOT_FOUND.into_response();
    }

    let file = match File::open(archive_path(&task.id)).await {
        Ok(v) => v,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let filename = task.result_filename.as_deref().unwrap_or("archive.zip");

    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/zip"),
    );
    headers.insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(size));
    if let Ok(v) = http::HeaderValue::from_str(&content_disposition(filename)) {
        headers.insert(http::header::CONTENT_DISPOSITION, v);
    }

    response
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

    // `/metrics` exposes internal request paths/status codes/latencies.
    // Require the same API key as the rest of the authenticated surface —
    // see README for the operational note this implies for the scrape config.
    let metric_router = Router::new()
        .route("/metrics", get(|| async move { metric_handle.render() }))
        .layer(middleware::from_fn(auth));

    let public_router = Router::new()
        .route("/api/download/{task_id}", get(download))
        .route("/health", get(health_check));

    Router::new()
        .merge(public_router)
        .merge(app_router)
        .merge(metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &Request<Body>| {
                    // Use the route template, never the raw URI: the download
                    // token is part of the path and must not be logged.
                    let route = req
                        .extensions()
                        .get::<MatchedPath>()
                        .map(MatchedPath::as_str)
                        .unwrap_or("unmatched");
                    tracing::info_span!("request", method = %req.method(), route)
                })
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::test_support::ensure_test_env, services::utils::get_key, structures::ObjectType,
    };
    use smallvec::smallvec;
    use tower::ServiceExt;

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

    async fn noop_runner(_ctx: TaskCtx, _data: CreateTask) {}

    async fn body_json(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn api_keys_match_uses_constant_time_comparison() {
        assert!(api_keys_match("secret-key", "secret-key"));
        assert!(!api_keys_match("secret-key", "wrong-key"));
        assert!(!api_keys_match("short", "much-longer-secret-key"));
    }

    #[test]
    fn content_disposition_has_rfc5987_filename() {
        let v = content_disposition("Усачёв_a.fb2.zip");
        assert!(v.starts_with("attachment; filename=\""));
        assert!(v.contains("filename*=UTF-8''%D0%A3"));
        assert!(v.ends_with("_a.fb2.zip"));
    }

    #[tokio::test]
    async fn create_archive_task_reuses_existing_non_failed_task_via_dedup_index() {
        ensure_test_env();
        let data = sample_task_data(424242);
        let dedup_key = get_key(data.clone());
        let seeded_token = "seeded-token-reuse-test".to_string();

        put_task(Task {
            id: seeded_token.clone(),
            status: TaskStatus::InProgress,
            status_description: "Подготовка".into(),
            error_message: None,
            result_filename: None,
            content_size: None,
        })
        .await;
        DEDUP_INDEX
            .insert(dedup_key.clone(), seeded_token.clone())
            .await;

        let response = create_archive_task_with(data, noop_runner).await;
        let json = body_json(response).await;

        assert_eq!(json["id"].as_str().unwrap(), seeded_token);
        assert_ne!(json["id"].as_str().unwrap(), dedup_key);
    }

    #[tokio::test]
    async fn concurrent_identical_requests_create_single_task() {
        ensure_test_env();
        let a = create_archive_task_with(sample_task_data(515151), noop_runner);
        let b = create_archive_task_with(sample_task_data(515151), noop_runner);
        let (a, b) = tokio::join!(a, b);
        assert_eq!(body_json(a).await["id"], body_json(b).await["id"]);
    }

    #[tokio::test]
    async fn create_archive_task_generates_random_token_not_md5_key() {
        ensure_test_env();
        let data = sample_task_data(434343);
        let dedup_key = get_key(data.clone());

        // Mock runner: no network access.
        let response = create_archive_task_with(data, noop_runner).await;
        let json = body_json(response).await;
        let id = json["id"].as_str().unwrap();

        assert_ne!(id, dedup_key);
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn active_task_is_not_evicted_and_terminal_state_replaces_it() {
        ensure_test_env();
        let token = "active-task-test".to_string();
        put_task(Task {
            id: token.clone(),
            status: TaskStatus::InProgress,
            status_description: "x".into(),
            error_message: None,
            result_filename: None,
            content_size: None,
        })
        .await;
        run_cache_maintenance().await;
        assert_eq!(
            get_task(&token).await.unwrap().status,
            TaskStatus::InProgress
        );

        put_task(Task {
            id: token.clone(),
            status: TaskStatus::Failed,
            status_description: "x".into(),
            error_message: None,
            result_filename: None,
            content_size: None,
        })
        .await;
        assert_eq!(get_task(&token).await.unwrap().status, TaskStatus::Failed);
        assert!(!ACTIVE_TASKS.lock().await.contains_key(&token));
    }

    async fn test_router() -> Router {
        static ROUTER: tokio::sync::OnceCell<Router> = tokio::sync::OnceCell::const_new();
        ROUTER
            .get_or_init(|| async { get_router().await })
            .await
            .clone()
    }

    async fn write_archive_file(token: &str, bytes: &[u8]) {
        tokio::fs::create_dir_all(&CONFIG.data_dir).await.unwrap();
        tokio::fs::write(archive_path(token), bytes).await.unwrap();
    }

    #[tokio::test]
    async fn download_returns_404_for_in_progress_task() {
        ensure_test_env();
        let token = "test-download-in-progress".to_string();
        write_archive_file(&token, b"partial-zip").await;
        put_task(Task {
            id: token.clone(),
            status: TaskStatus::InProgress,
            status_description: "working".into(),
            error_message: None,
            result_filename: None,
            content_size: None,
        })
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
    }

    #[tokio::test]
    async fn download_streams_file_only_when_complete() {
        ensure_test_env();
        let token = "test-download-complete".to_string();
        write_archive_file(&token, b"zip-bytes").await;
        put_task(Task {
            id: token.clone(),
            status: TaskStatus::Complete,
            status_description: "done".into(),
            error_message: None,
            result_filename: Some("archive.zip".into()),
            content_size: Some(9),
        })
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
        let h = response.headers();
        assert_eq!(h["content-type"], "application/zip");
        assert_eq!(h["content-length"], "9");
        assert!(h["content-disposition"]
            .to_str()
            .unwrap()
            .contains("filename*=UTF-8''archive.zip"));
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"zip-bytes");
    }

    #[tokio::test]
    async fn metrics_requires_auth() {
        ensure_test_env();
        let app = test_router().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn metrics_accessible_with_valid_api_key() {
        ensure_test_env();
        let app = test_router().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .header("Authorization", "test-api-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
