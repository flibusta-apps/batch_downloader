use std::{fmt, io::{Seek, Read}};

use base64::{engine::general_purpose, Engine};
use bytes::Bytes;
use minio_rsc::{provider::StaticProvider, Minio, types::args::{ObjectArgs, PresignedArgs}, errors::MinioError};
use reqwest::StatusCode;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use tempfile::SpooledTempFile;
use translit::{Transliterator, gost779b_ru, CharsMapping};
use zip::write::FileOptions;
use async_stream::stream;

use crate::{structures::{CreateTask, Task, ObjectType}, config, views::TASK_RESULTS};

use super::{library_client::{Book, get_sequence_books, get_author_books, get_translator_books, Page, get_sequence, get_author}, utils::response_to_tempfile};


pub fn get_key(
    input_data: CreateTask
) -> String {
    let mut data = input_data.clone();
    data.allowed_langs.sort();

    let data_string = serde_json::to_string(&data).unwrap();

    format!("{:x}", md5::compute(data_string))
}


pub async fn get_books<Fut>(
    object_id: u32,
    allowed_langs: SmallVec<[SmartString; 3]>,
    books_getter: fn(id: u32, page: u32, allowed_langs: SmallVec<[SmartString; 3]>) -> Fut
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
    };

    Ok(result)
}

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
    file_type: String,
) -> Result<Option<(SpooledTempFile, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let mut response = reqwest::Client::new()
        .get(format!(
            "{}/api/v1/download/{book_id}/{file_type}",
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

    let output_file = match response_to_tempfile(&mut response).await {
        Some(v) => v.0,
        None => return Ok(None),
    };

    Ok(Some((output_file, filename)))
}


fn get_stream(mut temp_file: Box<dyn Read + Send>) -> impl futures_core::Stream<Item = Result<Bytes, MinioError>> {
    stream! {
        let mut buf = [0; 2048];

        loop {
            match temp_file.read(&mut buf) {
                Ok(count) => {
                    if count == 0 {
                        break;
                    }

                    yield Ok(Bytes::copy_from_slice(&buf[0..count]))
                },
                Err(_) => break
            }
        }
    }
}


pub async fn create_archive_task(key: String, data: CreateTask) {
    let books = match data.object_type {
        ObjectType::Sequence => get_books(data.object_id, data.allowed_langs, get_sequence_books).await,
        ObjectType::Author => get_books(data.object_id, data.allowed_langs, get_author_books).await,
        ObjectType::Translator => get_books(data.object_id, data.allowed_langs, get_translator_books).await,
    };

    let books = match books {
        Ok(v) => v,
        Err(err) => {
            return; // log error and task error
        },
    };

    let books: Vec<_> = books
        .iter()
        .filter(|book| book.available_types.contains(&data.file_format))
        .collect();

    if books.is_empty() {
        return; // log error and task error
    }

    let output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
    let mut archive = zip::ZipWriter::new(output_file);

    let options = FileOptions::default()
        .compression_level(Some(9))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    for book in books {
        let (mut tmp_file, filename) = match download(book.id, data.file_format.clone()).await {
            Ok(v) => {
                match v {
                    Some(v) => v,
                    None => {
                        return; // log error and task error
                    },
                }
            },
            Err(err) => {
                return; // log error and task error
            },
        };

        match archive.start_file(filename, options) {
            Ok(_) => (),
            Err(_) => return, // log error and task error
        };

        match std::io::copy(&mut tmp_file, &mut archive) {
            Ok(_) => (),
            Err(_) => return,  // log error and task error
        };
    }

    let mut archive_result = match archive.finish() {
        Ok(v) => v,
        Err(err) => return,  // log error and task error
    };

    archive_result.rewind().unwrap();

    let result_filename = match data.object_type {
        ObjectType::Sequence => {
            match get_sequence(data.object_id).await {
                Ok(v) => v.name,
                Err(err) => {
                    println!("{}", err);
                    return;  // log error and task error
                },
            }
        },
        ObjectType::Author | ObjectType::Translator => {
            match get_author(data.object_id).await {
                Ok(v) => {
                    vec![v.first_name, v.last_name, v.middle_name.unwrap_or("".to_string())]
                        .into_iter()
                        .filter(|v| !v.is_empty())
                        .collect::<Vec<String>>()
                        .join("_")
                },
                Err(err) => {
                    println!("{}", err);
                    return;   // log error and task error
                },
            }
        },
    };

    let final_filename = {
        let transliterator = Transliterator::new(gost779b_ru());

        let mut filename_without_type = transliterator.convert(&result_filename, false);

        "(),….’!\"?»«':".get(..).into_iter().for_each(|char| {
            filename_without_type = filename_without_type.replace(char, "");
        });

        let replace_char_map: CharsMapping = [
            ("—", "-"),
            ("/", "_"),
            ("№", "N"),
            (" ", "_"),
            ("–", "-"),
            ("á", "a"),
            (" ", "_"),
            ("'", ""),
            ("`", ""),
            ("[", ""),
            ("]", ""),
            ("\"", ""),
        ].to_vec();

        let replace_transliterator = Transliterator::new(replace_char_map);
        let normal_filename = replace_transliterator.convert(&filename_without_type, false);

        let normal_filename = normal_filename.replace(|c: char| !c.is_ascii(), "");

        let right_part = format!(".zip");
        let normal_filename_slice = std::cmp::min(64 - right_part.len() - 1, normal_filename.len() - 1);

        let left_part = if normal_filename_slice == normal_filename.len() - 1 {
            &normal_filename
        } else {
            normal_filename.get(..normal_filename_slice).unwrap_or_else(|| panic!("Can't slice left part: {:?} {:?}", normal_filename, normal_filename_slice))
        };

        format!("{left_part}{right_part}")
    };

    let provider = StaticProvider::new(
        &config::CONFIG.minio_access_key,
        &config::CONFIG.minio_secret_key,
        None
    );
    let minio = Minio::builder()
        .host(&config::CONFIG.minio_host)
        .provider(provider)
        .secure(false)
        .build()
        .unwrap();

    let is_bucket_exist = match minio.bucket_exists(&config::CONFIG.minio_bucket).await {
        Ok(v) => v,
        Err(err) => {
            println!("{}", err);
            return;   // log error and task error
        },  // log error and task error
    };

    if !is_bucket_exist {
        minio.make_bucket(&config::CONFIG.minio_bucket, false).await;
    }

    let data_stream = get_stream(Box::new(archive_result));

    if let Err(err) = minio.put_object_stream(
        ObjectArgs::new(&config::CONFIG.minio_bucket, final_filename.clone()),
        Box::pin(data_stream)
    ).await {
        println!("{}", err);
        return;   // log error and task error
    }

    let link = match minio.presigned_get_object(
        PresignedArgs::new(&config::CONFIG.minio_bucket, final_filename)
    ).await {
        Ok(v) => v,
        Err(err) => {
            println!("{}", err);
            return;   // log error and task error
        },    // log error and task error
    };

    println!("{}", link);
}


pub async fn create_task(
    data: CreateTask
) -> Task {
    let key = get_key(data.clone());

    let task = Task {
        id: key.clone(),
        status: crate::structures::TaskStatus::InProgress,
        result_filename: None,
        result_link: None
    };

    TASK_RESULTS.insert(key.clone(), task.clone()).await;

    tokio::spawn(async {
        create_archive_task(key, data).await;
    });

    task
}
