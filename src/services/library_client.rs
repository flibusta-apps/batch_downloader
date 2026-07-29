use once_cell::sync::Lazy;
use serde::{de::DeserializeOwned, Deserialize};
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use std::time::Duration;
use tracing::log;

use crate::config;

/// Time allowed to establish a TCP/TLS connection to the library service
/// before giving up. Kept short since it is an internal service.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Total time allowed for a single request (connect + send + receive) to the
/// library service. Bounds worst-case latency per call so upstream calls
/// never hang indefinitely (see Problem 03.1 in
/// docs/specs/03-task-lifecycle-stuck-and-lost-tasks.md). Kept consistent
/// with `cache_client::REQUEST_TIMEOUT`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build reqwest client for library service")
});
const PAGE_SIZE: &str = "50";

fn get_allowed_langs_params(
    allowed_langs: SmallVec<[SmartString; 3]>,
) -> Vec<(&'static str, SmartString)> {
    allowed_langs
        .into_iter()
        .map(|lang| ("allowed_langs", lang))
        .collect()
}

async fn _make_request<T>(
    url: &str,
    params: Vec<(&str, SmartString)>,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    T: DeserializeOwned,
{
    let response = CLIENT
        .get(format!("{}{}", &config::CONFIG.library_url, url))
        .query(&params)
        .header("Authorization", &config::CONFIG.library_api_key)
        .send()
        .await?
        .error_for_status()?;

    match response.json::<T>().await {
        Ok(v) => Ok(v),
        Err(err) => {
            log::error!("Failed serialization: url={:?} err={:?}", url, err);
            Err(Box::new(err))
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct Book {
    pub id: u64,
    pub available_types: SmallVec<[String; 4]>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u32,

    pub page: u32,

    pub size: u32,
    pub pages: u32,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Sequence {
    pub id: u32,
    pub name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Author {
    pub id: u32,
    pub first_name: String,
    pub last_name: String,
    pub middle_name: Option<String>,
}

pub async fn get_author_books(
    id: u32,
    page: u32,
    allowed_langs: SmallVec<[SmartString; 3]>,
) -> Result<Page<Book>, Box<dyn std::error::Error + Send + Sync>> {
    let mut params = get_allowed_langs_params(allowed_langs);

    params.push(("page", page.to_string().into()));
    params.push(("size", PAGE_SIZE.to_string().into()));

    _make_request(format!("/api/v1/authors/{id}/books").as_str(), params).await
}

pub async fn get_translator_books(
    id: u32,
    page: u32,
    allowed_langs: SmallVec<[SmartString; 3]>,
) -> Result<Page<Book>, Box<dyn std::error::Error + Send + Sync>> {
    let mut params = get_allowed_langs_params(allowed_langs);

    params.push(("page", page.to_string().into()));
    params.push(("size", PAGE_SIZE.to_string().into()));

    _make_request(format!("/api/v1/translators/{id}/books").as_str(), params).await
}

pub async fn get_sequence_books(
    id: u32,
    page: u32,
    allowed_langs: SmallVec<[SmartString; 3]>,
) -> Result<Page<Book>, Box<dyn std::error::Error + Send + Sync>> {
    let mut params = get_allowed_langs_params(allowed_langs);

    params.push(("page", page.to_string().into()));
    params.push(("size", PAGE_SIZE.to_string().into()));

    _make_request(format!("/api/v1/sequences/{id}/books").as_str(), params).await
}

pub async fn get_author(id: u32) -> Result<Author, Box<dyn std::error::Error + Send + Sync>> {
    _make_request(&format!("/api/v1/authors/{id}"), vec![]).await
}

pub async fn get_sequence(id: u32) -> Result<Sequence, Box<dyn std::error::Error + Send + Sync>> {
    _make_request(&format!("/api/v1/sequences/{id}"), vec![]).await
}
