use std::fmt;

use base64::{engine::general_purpose, Engine};
use reqwest::StatusCode;
use smartstring::alias::String as SmartString;

use super::{cache_client, utils::response_to_tempfile};

#[derive(Debug, Clone)]
struct DownloadError {
    status_code: StatusCode,
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Status code is {0}", self.status_code)
    }
}

impl std::error::Error for DownloadError {}

/// Extracts and decodes the `x-filename-b64` header into a UTF-8 filename.
///
/// Returns a descriptive error if the header is missing, contains invalid
/// base64, or decodes to bytes that aren't valid UTF-8.
fn extract_filename(
    headers: &reqwest::header::HeaderMap,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let header_value = headers
        .get("x-filename-b64")
        .ok_or("missing x-filename-b64 header")?;

    let base64_encoder = general_purpose::STANDARD;
    let decoded = base64_encoder
        .decode(header_value)
        .map_err(|err| format!("invalid base64 in x-filename-b64 header: {err}"))?;

    let filename = std::str::from_utf8(&decoded)
        .map_err(|err| format!("x-filename-b64 header decoded to invalid UTF-8: {err}"))?;

    Ok(sanitize_filename(filename))
}

/// Reduces an untrusted filename to a safe basename: no path components
/// (zip-slip), no control characters, never empty / `.` / `..`.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_string();

    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// True when the error is a (retries exhausted) TFCS rate limit.
pub fn is_rate_limited(err: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        err.downcast_ref::<cache_client::CacheClientError>(),
        Some(cache_client::CacheClientError::RateLimited(_))
    )
}

pub async fn download(
    book_id: u64,
    file_type: SmartString,
    user_id: Option<i64>,
    normalized: bool,
) -> Result<(std::fs::File, String), Box<dyn std::error::Error + Send + Sync>> {
    let request = cache_client::cache_download_request(book_id, &file_type, user_id, normalized)?;
    download_request(request).await
}

async fn download_request(
    request: reqwest::Request,
) -> Result<(std::fs::File, String), Box<dyn std::error::Error + Send + Sync>> {
    for attempt in 1..=cache_client::MAX_RETRIES {
        let response = cache_client::call_with_retry(
            request
                .try_clone()
                .ok_or(cache_client::CacheClientError::CannotCloneRequest)?,
        )
        .await?;

        match response.status() {
            StatusCode::OK => {}
            // Missing books are terminal skips, never retried.
            StatusCode::NOT_FOUND | StatusCode::NO_CONTENT => {
                return Err(Box::new(DownloadError {
                    status_code: response.status(),
                }));
            }
            // 429 is handled by cache_client::cache_download returning
            // CacheClientError::RateLimited which propagates up as-is.
            status => {
                return Err(Box::new(DownloadError {
                    status_code: status,
                }));
            }
        };

        let mut response = response;
        let filename = extract_filename(response.headers())?;

        let output_file = match response_to_tempfile(&mut response).await {
            Ok(v) => v,
            Err(err) => {
                if attempt < cache_client::MAX_RETRIES
                    && err
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(cache_client::is_transient_error)
                {
                    // The failed anonymous file is dropped by response_to_tempfile;
                    // retry a complete GET into a fresh file, never append bytes.
                    tokio::time::sleep(std::time::Duration::from_secs(cache_client::backoff_secs(
                        1, attempt,
                    )))
                    .await;
                    continue;
                }
                return Err(err);
            }
        };

        return Ok((output_file.0, filename));
    }
    unreachable!("bounded retry loop always returns on the last attempt")
}

#[cfg(test)]
mod tests {
    use super::{extract_filename, sanitize_filename};
    use base64::{engine::general_purpose, Engine};
    use reqwest::header::{HeaderMap, HeaderValue};

    async fn serve_responses(
        responses: Vec<&'static str>,
    ) -> (reqwest::Request, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        crate::config::test_support::ensure_test_env();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/book", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0; 4096];
                let mut received = 0;
                while !buf[..received].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut buf[received..]).await.unwrap();
                    assert!(count > 0, "request ended before headers");
                    received += count;
                }
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        (
            super::cache_client::CLIENT.get(url).build().unwrap(),
            server,
        )
    }

    #[tokio::test]
    async fn truncated_body_retries_into_fresh_file() {
        use std::io::Read;
        let (request, server) = serve_responses(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nx-filename-b64: Ym9vay5mYjI=\r\nConnection: close\r\n\r\nbad",
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nx-filename-b64: Ym9vay5mYjI=\r\nConnection: close\r\n\r\ngood",
        ]).await;
        let (mut file, filename) = super::download_request(request).await.unwrap();
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        assert_eq!(body, "good");
        assert_eq!(filename, "book.fb2");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_body_retries_are_bounded() {
        let (request, server) = serve_responses(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nx-filename-b64: Ym9vay5mYjI=\r\nConnection: close\r\n\r\nbad";
            super::cache_client::MAX_RETRIES as usize
        ]).await;
        let error = super::download_request(request).await.unwrap_err();
        assert!(error.downcast_ref::<reqwest::Error>().is_some());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_books_are_not_retried() {
        for response in [
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
        ] {
            let (request, server) = serve_responses(vec![response]).await;
            let error = super::download_request(request).await.unwrap_err();
            assert!(error.downcast_ref::<super::DownloadError>().is_some());
            server.await.unwrap();
        }
    }

    #[test]
    fn missing_header_is_err() {
        let headers = HeaderMap::new();
        let result = extract_filename(&headers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing"));
    }

    #[test]
    fn invalid_base64_is_err() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-filename-b64",
            HeaderValue::from_static("not-valid-base64!!!"),
        );
        let result = extract_filename(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn valid_base64_utf8_filename_is_ok() {
        let filename = "book_title_s.fb2.zip";
        let encoded = general_purpose::STANDARD.encode(filename);
        let mut headers = HeaderMap::new();
        headers.insert("x-filename-b64", HeaderValue::from_str(&encoded).unwrap());

        let result = extract_filename(&headers);
        assert_eq!(result.unwrap(), filename);
    }

    #[test]
    fn valid_base64_non_utf8_is_err() {
        // 0xFF, 0xFE is not valid UTF-8.
        let invalid_utf8_bytes: &[u8] = &[0xFF, 0xFE];
        let encoded = general_purpose::STANDARD.encode(invalid_utf8_bytes);
        let mut headers = HeaderMap::new();
        headers.insert("x-filename-b64", HeaderValue::from_str(&encoded).unwrap());

        let result = extract_filename(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn sanitize_strips_path_components_and_control_chars() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\evil.fb2"), "evil.fb2");
        assert_eq!(sanitize_filename("/abs/path/book.fb2"), "book.fb2");
        assert_eq!(sanitize_filename("bo\u{0}o\nk.fb2"), "book.fb2");
        assert_eq!(sanitize_filename(".."), "file");
        assert_eq!(sanitize_filename("a/"), "file");
        assert_eq!(sanitize_filename(".hidden.fb2"), "hidden.fb2");
    }
}
