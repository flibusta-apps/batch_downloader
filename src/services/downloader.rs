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

pub async fn download(
    book_id: u64,
    file_type: SmartString,
    user_id: Option<i64>,
) -> Result<(SpooledTempFile, String), Box<dyn std::error::Error + Send + Sync>> {
    let response = cache_client::cache_download(book_id, &file_type, user_id).await?;

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
    let headers = response.headers();

    let base64_encoder = general_purpose::STANDARD;

    let filename = std::str::from_utf8(
        &base64_encoder
            .decode(headers.get("x-filename-b64").unwrap())
            .unwrap(),
    )
    .unwrap()
    .to_string();

    let output_file = match response_to_tempfile(&mut response).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("Error: {}", err);
            return Err(err);
        }
    };

    Ok((output_file.0, filename))
}
