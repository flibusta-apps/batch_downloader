use once_cell::sync::Lazy;
use reqwest::{Request, Response, StatusCode};
use serde::Deserialize;
use std::time::Duration;
use tracing::warn;

use crate::config;

/// Time allowed to establish a TCP/TLS connection to TFCS before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum silence between two body chunks / the response headers.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Total time allowed for one download request (including the body).
/// Downloads can be large, so this is much longer than a typical API call.
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(180);

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .expect("failed to build reqwest client for TFCS")
});

const MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_AFTER_SECS: u64 = 5;
/// Maximum backoff duration in seconds (cap exponential backoff).
const MAX_BACKOFF_SECS: u64 = 300;
/// Base backoff for transient (5xx / timeout / connect) errors.
const TRANSIENT_BACKOFF_SECS: u64 = 1;

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

/// C2 contract: `{"detail": "...", "retry_after_secs": N}` (extra fields ignored).
#[derive(Deserialize)]
struct RateLimitBody {
    retry_after_secs: u64,
}

/// Extract retry_after from the response.
/// Priority: `Retry-After` header > `retry_after_secs` from JSON body > default.
async fn extract_retry_after(response: Response) -> u64 {
    if let Some(header_val) = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return header_val;
    }

    if let Ok(body) = response.text().await {
        if let Ok(parsed) = serde_json::from_str::<RateLimitBody>(&body) {
            return parsed.retry_after_secs;
        }
    }

    DEFAULT_RETRY_AFTER_SECS
}

/// Exponential backoff `base * 2^(attempt-1)`, saturating and capped.
fn backoff_secs(base: u64, attempt: u32) -> u64 {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    base.saturating_mul(factor).min(MAX_BACKOFF_SECS)
}

fn is_transient_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
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
    let url = format!("{}{}", config::CONFIG.cache_url, path);

    let mut builder = CLIENT
        .request(method, &url)
        .header("Authorization", &config::CONFIG.cache_api_key);

    if let Some(uid) = user_id {
        builder = builder.header("X-User-Id", uid.to_string());
    }

    builder
}

/// Send a request to TFCS with retries.
///
/// * 429: waits `Retry-After` (header or JSON `retry_after_secs`), growing
///   exponentially and capped at `MAX_BACKOFF_SECS`; after `MAX_RETRIES`
///   returns `Err(CacheClientError::RateLimited)`.
/// * 5xx / timeouts / connect errors: retried with a small backoff; after the
///   last attempt the 5xx response (or the error) is returned as is.
pub async fn call_with_retry(request: Request) -> Result<Response, CacheClientError> {
    let mut attempt: u32 = 0;
    let user_id = request
        .headers()
        .get("X-User-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    loop {
        let cloned = request
            .try_clone()
            .ok_or(CacheClientError::CannotCloneRequest)?;
        attempt += 1;

        let response = match CLIENT.execute(cloned).await {
            Ok(r) => r,
            Err(err) if is_transient_error(&err) && attempt < MAX_RETRIES => {
                let wait = backoff_secs(TRANSIENT_BACKOFF_SECS, attempt);
                warn!(attempt, "TFCS transient error: {err}; retrying in {wait}s");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let status = response.status();

        if status.is_server_error() && attempt < MAX_RETRIES {
            let wait = backoff_secs(TRANSIENT_BACKOFF_SECS, attempt);
            warn!(attempt, %status, "TFCS server error; retrying in {wait}s");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }

        if status != StatusCode::TOO_MANY_REQUESTS {
            return Ok(response);
        }

        let retry_after = extract_retry_after(response).await;
        let operation = extract_operation(request.url().path());

        warn!(
            operation,
            retry_after_secs = retry_after,
            attempt,
            user_id = user_id.as_deref().unwrap_or("anonymous"),
            "TFCS rate limit exceeded (attempt {}/{})",
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

        tokio::time::sleep(Duration::from_secs(backoff_secs(retry_after, attempt))).await;
    }
}

/// GET a download resource from TFCS (cache hit download).
///
/// `normalized` is forwarded to TFCS as a `?normalized=` query parameter.
pub async fn cache_download(
    object_id: u64,
    object_type: &str,
    user_id: Option<i64>,
    normalized: bool,
) -> Result<Response, CacheClientError> {
    let path = format!("/api/v1/download/{object_id}/{object_type}/");
    let builder = build_request(reqwest::Method::GET, &path, user_id)
        .query(&[("normalized", normalized)])
        .timeout(DOWNLOAD_TOTAL_TIMEOUT);
    let request = builder.build()?;

    call_with_retry(request).await
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

#[cfg(test)]
mod tests {
    use super::{backoff_secs, build_request, MAX_BACKOFF_SECS};

    /// `build_request` reads `crate::config::CONFIG`, a `once_cell::sync::Lazy`
    /// that panics on first access if the required env vars are unset. Since a
    /// panicked `Lazy` poisons itself for the rest of the process, any other
    /// test in the binary that touches `CONFIG` afterwards would fail too.
    /// Set harmless fake values before first access so this test module never
    /// depends on (or interferes with) the real environment.
    fn ensure_test_env() {
        crate::config::test_support::ensure_test_env();
    }

    #[test]
    fn backoff_is_capped_and_never_overflows() {
        assert_eq!(backoff_secs(5, 1), 5);
        assert_eq!(backoff_secs(5, 2), 10);
        assert_eq!(backoff_secs(5, 30), MAX_BACKOFF_SECS);
        assert_eq!(backoff_secs(u64::MAX, 100), MAX_BACKOFF_SECS);
    }

    /// `cache_download` must always include `?normalized=` so that TFCS
    /// (or any future consumer) can react to the flag. This guards against
    /// accidental removal of `.query(&[("normalized", ...)])`.
    #[test]
    fn cache_download_query_includes_normalized() {
        ensure_test_env();
        for normalized in [true, false] {
            let builder = build_request(reqwest::Method::GET, "/api/v1/download/42/fb2/", None)
                .query(&[("normalized", normalized)]);

            let url = builder.build().unwrap().url().to_string();
            assert!(
                url.contains(&format!("normalized={normalized}")),
                "expected `normalized={normalized}` in URL, got: {url}"
            );
        }
    }
}
