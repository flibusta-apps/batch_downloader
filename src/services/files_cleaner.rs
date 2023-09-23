use chrono::{DateTime, Utc, Duration};
use minio_rsc::{client::ListObjectsArgs, datatype::Object};

use super::minio::get_minio;
use crate::config;


pub async fn clean_files() -> Result<(), Box<dyn std::error::Error>> {
    let minio_client = get_minio();

    let objects = minio_client.list_objects(
        &config::CONFIG.minio_bucket,
        ListObjectsArgs::default()
    ).await?;

    let delete_before = Utc::now() - Duration::hours(3);
    for Object { key, last_modified, .. } in objects.contents {
        let last_modified_date: DateTime<Utc> = DateTime::parse_from_rfc3339(&last_modified)?.into();

        if last_modified_date <= delete_before {
            let _ = minio_client.remove_object(&config::CONFIG.minio_bucket, key).await;
        }
    }

    Ok(())
}
