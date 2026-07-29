use std::fmt;

use base64::{engine::general_purpose, Engine};
use reqwest::StatusCode;
use smartstring::alias::String as SmartString;
use tempfile::SpooledTempFile;
use tracing::log;

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
        .map_err(|err| format!("x-filename-b64 header decoded to invalid UTF-8: {err}"))?
        .to_string();

    Ok(filename)
}

pub async fn download(
    book_id: u64,
    file_type: SmartString,
    user_id: Option<i64>,
    normalized: bool,
) -> Result<(SpooledTempFile, String), Box<dyn std::error::Error + Send + Sync>> {
    let response = cache_client::cache_download(book_id, &file_type, user_id, normalized).await?;

    match response.status() {
        StatusCode::OK => {}
        // 429 is handled by cache_client::cache_download returning CacheClientError::RateLimited
        // which propagates up as-is
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
            log::error!("Error: {}", err);
            return Err(err);
        }
    };

    Ok((output_file.0, filename))
}

#[cfg(test)]
mod tests {
    use super::extract_filename;
    use base64::{engine::general_purpose, Engine};
    use reqwest::header::{HeaderMap, HeaderValue};

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
}
