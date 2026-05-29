use once_cell::sync::Lazy;
use reqwest::{Request, Response, StatusCode};
use serde::Deserialize;
use tracing::warn;

use crate::config;

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(reqwest::Client::new);

const MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_AFTER_SECS: u64 = 5;
/// Maximum backoff duration in seconds (cap exponential backoff).
const MAX_BACKOFF_SECS: u64 = 300;

/// Error returned when TFCS responds with 429 Too Many Requests
/// after all retry attempts have been exhausted.
#[derive(Debug, Clone)]
pub struct RateLimitError {
    pub operation: String,
    pub retry_after_secs: u64,
    pub attempts: u32,
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "rate limit exceeded for operation '{}' after {} attempts (retry_after={}s)",
            self.operation, self.attempts, self.retry_after_secs
        )
    }
}

impl std::error::Error for RateLimitError {}

#[derive(Deserialize)]
struct RateLimitBody {
    #[allow(dead_code)]
    error: String,
    #[allow(dead_code)]
    operation: String,
    retry_after_secs: u64,
}

/// Extract retry_after from the response.
/// Priority: `Retry-After` header > `retry_after_secs` from JSON body > default.
///
/// Note: this consumes the response body, so only call this when
/// you no longer need the body for anything else.
async fn extract_retry_after(response: Response) -> u64 {
    // Try Retry-After header first (can be read from headers before consuming body)
    if let Some(header_val) = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        return header_val;
    }

    // Fallback: consume body and parse JSON
    if let Ok(body) = response.text().await {
        if let Ok(parsed) = serde_json::from_str::<RateLimitBody>(&body) {
            return parsed.retry_after_secs;
        }
    }

    DEFAULT_RETRY_AFTER_SECS
}

/// Extract a normalized operation name from a URL path.
/// Strips ID/type segments so logs aggregate by operation type.
/// /api/v1/download/123/epub/ → "cache_hit_download"
/// /api/v1/456/author/?copy=true → "cache_hit_copy"
/// /api/v1/456/author/ → "cache_hit"
fn extract_operation(path: &str) -> &'static str {
    if path.contains("/download/") {
        "cache_hit_download"
    } else if path.contains("copy=") {
        "cache_hit_copy"
    } else if path.contains("/update_cache") {
        "cache_miss"
    } else {
        "cache_hit"
    }
}

/// Build a request to TFCS with required authentication and optional user ID headers.
fn build_request(
    method: reqwest::Method,
    path: &str,
    user_id: Option<i64>,
) -> reqwest::RequestBuilder {
    let url = format!("{}{}", &config::CONFIG.cache_url, path);

    let mut builder = CLIENT
        .request(method, &url)
        .header("Authorization", &config::CONFIG.cache_api_key);

    if let Some(uid) = user_id {
        builder = builder.header("X-User-Id", uid.to_string());
    }

    builder
}

/// Send a request to TFCS with retry on 429 responses.
///
/// Retries up to MAX_RETRIES times with exponential backoff starting from
/// the `Retry-After` value returned by TFCS.
///
/// Returns `Ok(Response)` for non-429 responses.
/// Returns `Err(CacheClientError::RateLimited)` when all retries are exhausted.
pub async fn call_with_retry(request: Request) -> Result<Response, CacheClientError> {
    let mut attempt: u32 = 0;
    let user_id = request
        .headers()
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    loop {
        // Clone the request before sending so we can retry.
        // reqwest consumes the request body on send, so we need try_clone.
        let cloned = request
            .try_clone()
            .ok_or_else(|| CacheClientError::CannotCloneRequest)?;
        let response = CLIENT.execute(cloned).await?;

        if response.status() != StatusCode::TOO_MANY_REQUESTS {
            return Ok(response);
        }

        attempt += 1;

        let retry_after = extract_retry_after(response).await;
        let operation = extract_operation(request.url().path());

        warn!(
            operation,
            retry_after_secs = retry_after,
            attempt,
            user_id = user_id.as_deref().unwrap_or("anonymous"),
            "TFCS rate limit exceeded, retrying after {}s (attempt {}/{})",
            retry_after,
            attempt,
            MAX_RETRIES
        );

        if attempt >= MAX_RETRIES {
            return Err(CacheClientError::RateLimited(RateLimitError {
                operation: operation.to_string(),
                retry_after_secs: retry_after,
                attempts: attempt,
            }));
        }

        // Exponential backoff: retry_after * 2^(attempt-1), capped at MAX_BACKOFF_SECS
        let backoff_secs = (retry_after * 2u64.pow(attempt - 1)).min(MAX_BACKOFF_SECS);
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
    }
}

// ---- Public convenience functions ----

/// GET a resource from TFCS (cache hit / cache hit copy).
/// If `copy` is true, adds `?copy=true` query parameter.
pub async fn cache_get(
    object_id: u64,
    object_type: &str,
    copy: bool,
    user_id: Option<i64>,
) -> Result<Response, CacheClientError> {
    let path = if copy {
        format!("/api/v1/{object_id}/{object_type}/?copy=true")
    } else {
        format!("/api/v1/{object_id}/{object_type}/")
    };

    let builder = build_request(reqwest::Method::GET, &path, user_id);
    let request = builder.build()?;

    call_with_retry(request).await
}

/// GET a download resource from TFCS (cache hit download).
pub async fn cache_download(
    object_id: u64,
    object_type: &str,
    user_id: Option<i64>,
) -> Result<Response, CacheClientError> {
    let path = format!("/api/v1/download/{object_id}/{object_type}/");
    let builder = build_request(reqwest::Method::GET, &path, user_id);
    let request = builder.build()?;

    call_with_retry(request).await
}

/// DELETE a resource from TFCS.
///
/// Note: TFCS does not rate-limit DELETE endpoints per its API contract,
/// so no retry logic is applied here.
pub async fn cache_delete(
    object_id: u64,
    object_type: &str,
    user_id: Option<i64>,
) -> Result<Response, CacheClientError> {
    let path = format!("/api/v1/{object_id}/{object_type}/");
    let builder = build_request(reqwest::Method::DELETE, &path, user_id);
    let request = builder.build()?;
    let response = CLIENT.execute(request).await?;
    Ok(response)
}

/// POST to update_cache.
///
/// Note: TFCS does not rate-limit update_cache per its API contract,
/// so no retry logic is applied here.
pub async fn cache_update_cache(
    body: impl serde::Serialize,
    user_id: Option<i64>,
) -> Result<Response, CacheClientError> {
    let path = "/api/v1/update_cache";
    let builder = build_request(reqwest::Method::POST, path, user_id);
    let request = builder.json(&body).build()?;
    let response = CLIENT.execute(request).await?;
    Ok(response)
}

#[derive(Debug)]
pub enum CacheClientError {
    Reqwest(reqwest::Error),
    RateLimited(RateLimitError),
    CannotCloneRequest,
}

impl std::fmt::Display for CacheClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            CacheClientError::Reqwest(e) => write!(f, "reqwest error: {e}"),
            CacheClientError::RateLimited(e) => write!(f, "rate limited: {e}"),
            CacheClientError::CannotCloneRequest => {
                write!(f, "cannot clone request body for retry")
            }
        }
    }
}

impl std::error::Error for CacheClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CacheClientError::Reqwest(e) => Some(e),
            CacheClientError::RateLimited(e) => Some(e),
            CacheClientError::CannotCloneRequest => None,
        }
    }
}

impl From<reqwest::Error> for CacheClientError {
    fn from(e: reqwest::Error) -> Self {
        CacheClientError::Reqwest(e)
    }
}

impl From<RateLimitError> for CacheClientError {
    fn from(e: RateLimitError) -> Self {
        CacheClientError::RateLimited(e)
    }
}
