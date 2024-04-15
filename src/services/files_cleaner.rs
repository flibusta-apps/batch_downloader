use chrono::{DateTime, Duration, Utc};
use minio_rsc::{client::ListObjectsArgs, datatype::Object};

use super::minio::get_internal_minio;

pub async fn clean_files(bucket: String) -> Result<(), Box<dyn std::error::Error>> {
    let minio_client = get_internal_minio();

    let objects = minio_client
        .list_objects(&bucket, ListObjectsArgs::default())
        .await?;

    let delete_before = Utc::now() - Duration::hours(3);
    for Object {
        key, last_modified, ..
    } in objects.contents
    {
        let last_modified_date: DateTime<Utc> =
            DateTime::parse_from_rfc3339(&last_modified)?.into();

        if last_modified_date <= delete_before {
            let _ = minio_client.remove_object(&bucket, key).await;
        }
    }

    Ok(())
}
