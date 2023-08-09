use std::fmt;

use base64::{engine::general_purpose, Engine};
use reqwest::StatusCode;
use tempfile::SpooledTempFile;
use smartstring::alias::String as SmartString;

use crate::config;

use super::utils::response_to_tempfile;


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
) -> Result<(SpooledTempFile, String), Box<dyn std::error::Error + Send + Sync>> {
    let mut response = reqwest::Client::new()
        .get(format!(
            "{}/api/v1/download/{book_id}/{file_type}/",
            &config::CONFIG.cache_url
        ))
        .header("Authorization", &config::CONFIG.cache_api_key)
        .send()
        .await?
        .error_for_status()?;

    if response.status() != StatusCode::OK {
        return Err(Box::new(DownloadError {
            status_code: response.status(),
        }));
    };

    let headers = response.headers();

    let base64_encoder = general_purpose::STANDARD;

    let filename = std::str::from_utf8(
        &base64_encoder
            .decode(headers.get("x-filename-b64").unwrap())
            .unwrap(),
    )
    .unwrap()
    .to_string();

    let output_file = response_to_tempfile(&mut response).await.unwrap();

    Ok((output_file.0, filename))
}

