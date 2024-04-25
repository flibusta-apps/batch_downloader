use std::io::Seek;

use minio_rsc::client::PresignedArgs;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use tempfile::SpooledTempFile;
use tracing::log;
use zip::write::FileOptions;

use crate::{
    config,
    services::{
        downloader::download,
        minio::get_minio,
        utils::{get_filename, get_stream},
    },
    structures::{CreateTask, ObjectType, Task},
    views::TASK_RESULTS,
};

use super::{
    library_client::{get_author_books, get_sequence_books, get_translator_books, Book, Page},
    minio::get_internal_minio,
    utils::get_key,
};

pub async fn get_books<Fut>(
    object_id: u32,
    allowed_langs: SmallVec<[SmartString; 3]>,
    books_getter: fn(id: u32, page: u32, allowed_langs: SmallVec<[SmartString; 3]>) -> Fut,
    file_format: SmartString,
) -> Result<Vec<Book>, Box<dyn std::error::Error + Send + Sync>>
where
    Fut: std::future::Future<Output = Result<Page<Book>, Box<dyn std::error::Error + Send + Sync>>>,
{
    let mut result: Vec<Book> = vec![];

    let first_page = match books_getter(object_id, 1, allowed_langs.clone()).await {
        Ok(v) => v,
        Err(err) => return Err(err),
    };

    result.extend(first_page.items);

    let mut current_page = 2;
    let page_count = first_page.pages;

    while current_page <= page_count {
        let page = match books_getter(object_id, current_page, allowed_langs.clone()).await {
            Ok(v) => v,
            Err(err) => return Err(err),
        };
        result.extend(page.items);

        current_page += 1;
    }

    let result = result
        .iter()
        .filter(|book| book.available_types.contains(&file_format.to_string()))
        .cloned()
        .collect();

    Ok(result)
}

pub async fn set_task_error(key: String, error_message: String) {
    let task = Task {
        id: key.clone(),
        status: crate::structures::TaskStatus::Failed,
        status_description: "Ошибка!".to_string(),
        error_message: Some(error_message),
        result_filename: None,
        result_link: None,
        content_size: None,
    };

    TASK_RESULTS.insert(key, task.clone()).await;
}

pub async fn set_progress_description(key: String, description: String) {
    let task = Task {
        id: key.clone(),
        status: crate::structures::TaskStatus::InProgress,
        status_description: description,
        error_message: None,
        result_filename: None,
        result_link: None,
        content_size: None,
    };

    TASK_RESULTS.insert(key, task.clone()).await;
}

pub async fn upload_to_minio(
    archive: SpooledTempFile,
    folder_name: String,
    filename: String,
) -> Result<(String, u64), Box<dyn std::error::Error + Send + Sync>> {
    let full_filename = format!("{}/{}", folder_name, filename);

    let internal_minio = get_internal_minio();

    let is_bucket_exist = match internal_minio
        .bucket_exists(&config::CONFIG.minio_bucket)
        .await
    {
        Ok(v) => v,
        Err(err) => return Err(Box::new(err)),
    };

    if !is_bucket_exist {
        let _ = internal_minio
            .make_bucket(&config::CONFIG.minio_bucket, false)
            .await;
    }

    let data_stream = get_stream(Box::new(archive));

    if let Err(err) = internal_minio
        .put_object_stream(
            &config::CONFIG.minio_bucket,
            full_filename.clone(),
            Box::pin(data_stream),
            None,
        )
        .await
    {
        return Err(Box::new(err));
    }

    let minio = get_minio();

    let link = match minio
        .presigned_get_object(PresignedArgs::new(
            &config::CONFIG.minio_bucket,
            full_filename.clone(),
        ))
        .await
    {
        Ok(v) => v,
        Err(err) => {
            return Err(Box::new(err));
        }
    };

    let obj_size = match internal_minio
        .stat_object(&config::CONFIG.minio_bucket, full_filename.clone())
        .await
    {
        Ok(v) => v.unwrap().size().try_into().unwrap(),
        Err(_) => todo!(),
    };

    Ok((link, obj_size))
}

pub async fn create_archive(
    key: String,
    books: Vec<Book>,
    file_format: SmartString,
) -> Result<(SpooledTempFile, u64), Box<dyn std::error::Error + Send + Sync>> {
    let output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
    let mut archive = zip::ZipWriter::new(output_file);

    let options: FileOptions<_> = FileOptions::default()
        .compression_level(Some(9))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let books_count = books.len();
    let mut bytes_count: u64 = 0;

    for (index, book) in books.iter().enumerate() {
        let (mut tmp_file, filename) = match download(book.id, file_format.clone()).await {
            Ok(v) => v,
            Err(_) => continue,
        };

        match archive.start_file::<std::string::String, ()>(filename, options) {
            Ok(_) => (),
            Err(err) => return Err(Box::new(err)),
        };

        match std::io::copy(&mut tmp_file, &mut archive) {
            Ok(file_bytes_count) => bytes_count += file_bytes_count,
            Err(err) => return Err(Box::new(err)),
        };

        set_progress_description(
            key.clone(),
            format!("Загрузка книг: {}/{}", index + 1, books_count),
        )
        .await;
    }

    let mut archive_result = match archive.finish() {
        Ok(v) => v,
        Err(err) => return Err(Box::new(err)),
    };

    archive_result.rewind().unwrap();

    Ok((archive_result, bytes_count))
}

pub async fn create_archive_task(key: String, data: CreateTask) {
    let books = match data.object_type {
        ObjectType::Sequence => {
            get_books(
                data.object_id,
                data.allowed_langs.clone(),
                get_sequence_books,
                data.file_format.clone(),
            )
            .await
        }
        ObjectType::Author => {
            get_books(
                data.object_id,
                data.allowed_langs.clone(),
                get_author_books,
                data.file_format.clone(),
            )
            .await
        }
        ObjectType::Translator => {
            get_books(
                data.object_id,
                data.allowed_langs.clone(),
                get_translator_books,
                data.file_format.clone(),
            )
            .await
        }
    };

    set_progress_description(key.clone(), "Получение списка книг...".to_string()).await;

    let books = match books {
        Ok(v) => v,
        Err(err) => {
            set_task_error(key.clone(), "Failed getting books!".to_string()).await;
            log::error!("{}", err);
            return;
        }
    };

    if books.is_empty() {
        set_task_error(key.clone(), "No books!".to_string()).await;
        return;
    }

    let final_filename =
        match get_filename(data.object_type, data.object_id, data.file_format.clone()).await {
            Ok(v) => v,
            Err(err) => {
                set_task_error(key.clone(), "Can't get archive name!".to_string()).await;
                log::error!("{}", err);
                return;
            }
        };

    set_progress_description(key.clone(), "Сборка архива...".to_string()).await;

    let (archive_result, _inside_content_size) =
        match create_archive(key.clone(), books, data.file_format).await {
            Ok(v) => v,
            Err(err) => {
                set_task_error(key.clone(), "Failed downloading books!".to_string()).await;
                log::error!("{}", err);
                return;
            }
        };

    set_progress_description(key.clone(), "Загрузка архива...".to_string()).await;

    let folder_name = {
        let mut langs = data.allowed_langs.clone();
        langs.sort();
        langs.join("_")
    };

    let (link, content_size) =
        match upload_to_minio(archive_result, folder_name, final_filename.clone()).await {
            Ok(v) => v,
            Err(err) => {
                set_task_error(key.clone(), "Failed uploading archive!".to_string()).await;
                log::error!("{}", err);
                return;
            }
        };

    let task = Task {
        id: key.clone(),
        status: crate::structures::TaskStatus::Complete,
        status_description: "Архив готов! Ожидайте файл".to_string(),
        error_message: None,
        result_filename: Some(final_filename),
        result_link: Some(link),
        content_size: Some(content_size),
    };

    TASK_RESULTS.insert(key.clone(), task.clone()).await;
}

pub async fn create_task(data: CreateTask) -> Task {
    let key = get_key(data.clone());

    let task = Task {
        id: key.clone(),
        status: crate::structures::TaskStatus::InProgress,
        status_description: "Подготовка".to_string(),
        error_message: None,
        result_filename: None,
        result_link: None,
        content_size: None,
    };

    TASK_RESULTS.insert(key.clone(), task.clone()).await;

    tokio::spawn(create_archive_task(key, data));

    task
}
