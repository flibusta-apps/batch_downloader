use std::{fs::File, io::Seek};

use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use tracing::log;
use zip::write::FileOptions;

use crate::{
    services::{downloader::download, utils::get_filename},
    structures::{CreateTask, ObjectType, Task},
    views::TASK_RESULTS,
};

use super::{
    library_client::{get_author_books, get_sequence_books, get_translator_books, Book, Page},
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
        content_size: None,
    };

    TASK_RESULTS.insert(key, task.clone()).await;
}

pub async fn create_archive(
    key: String,
    books: Vec<Book>,
    file_format: SmartString,
) -> Result<(File, u64), Box<dyn std::error::Error + Send + Sync>> {
    let output_file = File::create(format!("/tmp/{}", key))?;
    let mut archive = zip::ZipWriter::new(output_file);

    let options: FileOptions<_> = FileOptions::default()
        .compression_level(Some(9))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let books_count = books.len();
    let mut bytes_count: u64 = 0;

    let mut filenames: Vec<String> = vec![];

    for (index, book) in books.iter().enumerate() {
        let (mut tmp_file, filename) = match download(book.id, file_format.clone()).await {
            Ok(v) => v,
            Err(_) => continue,
        };

        if filenames.contains(&filename) {
            continue;
        }

        match archive.start_file::<std::string::String, ()>(filename.clone(), options) {
            Ok(_) => (),
            Err(err) => return Err(Box::new(err)),
        };

        match std::io::copy(&mut tmp_file, &mut archive) {
            Ok(file_bytes_count) => bytes_count += file_bytes_count,
            Err(err) => return Err(Box::new(err)),
        };

        filenames.push(filename);

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

    let task = Task {
        id: key.clone(),
        status: crate::structures::TaskStatus::Complete,
        status_description: "Архив готов! Ожидайте файл".to_string(),
        error_message: None,
        result_filename: Some(final_filename),
        content_size: Some(archive_result.metadata().unwrap().len()),
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
        content_size: None,
    };

    TASK_RESULTS.insert(key.clone(), task.clone()).await;

    tokio::spawn(create_archive_task(key, data));

    task
}
