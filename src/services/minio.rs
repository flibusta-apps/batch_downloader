use minio_rsc::{provider::StaticProvider, Minio};

use crate::config;


pub fn get_minio() -> Minio {
    let provider = StaticProvider::new(
        &config::CONFIG.minio_access_key,
        &config::CONFIG.minio_secret_key,
        None
    );

    Minio::builder()
        .host(&config::CONFIG.minio_host)
        .provider(provider)
        .secure(false)
        .build()
        .unwrap()
}
