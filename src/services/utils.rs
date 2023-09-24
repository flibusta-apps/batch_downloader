use async_stream::stream;
use bytes::{Buf, Bytes};
use minio_rsc::error::Error;
use reqwest::Response;
use smartstring::alias::String as SmartString;
use tempfile::SpooledTempFile;
use translit::{gost779b_ru, CharsMapping, Transliterator};

use std::io::{Read, Seek, SeekFrom, Write};

use crate::structures::{CreateTask, ObjectType};

use super::library_client::{get_author, get_sequence};

pub fn get_key(input_data: CreateTask) -> String {
    let mut data = input_data.clone();
    data.allowed_langs.sort();

    let data_string = serde_json::to_string(&data).unwrap();

    format!("{:x}", md5::compute(data_string))
}

pub async fn response_to_tempfile(res: &mut Response) -> Option<(SpooledTempFile, usize)> {
    let mut tmp_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

    let mut data_size: usize = 0;

    {
        loop {
            let chunk = res.chunk().await;

            let result = match chunk {
                Ok(v) => v,
                Err(_) => return None,
            };

            let data = match result {
                Some(v) => v,
                None => break,
            };

            data_size += data.len();

            match tmp_file.write(data.chunk()) {
                Ok(_) => (),
                Err(_) => return None,
            }
        }

        tmp_file.seek(SeekFrom::Start(0)).unwrap();
    }

    Some((tmp_file, data_size))
}

pub fn get_stream(
    mut temp_file: Box<dyn Read + Send>,
) -> impl futures_core::Stream<Item = Result<Bytes, Error>> {
    stream! {
        let mut buf = [0; 2048];

        while let Ok(count) = temp_file.read(&mut buf) {
            if count == 0 {
                break;
            }

            yield Ok(Bytes::copy_from_slice(&buf[0..count]))
        }
    }
}

pub async fn get_filename(
    object_type: ObjectType,
    object_id: u32,
    file_format: SmartString,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let result_filename = match object_type {
        ObjectType::Sequence => match get_sequence(object_id).await {
            Ok(v) => v.name,
            Err(err) => {
                return Err(err);
            }
        },
        ObjectType::Author | ObjectType::Translator => match get_author(object_id).await {
            Ok(v) => vec![
                v.first_name,
                v.last_name,
                v.middle_name.unwrap_or("".to_string()),
            ]
            .into_iter()
            .filter(|v| !v.is_empty())
            .collect::<Vec<String>>()
            .join("_"),
            Err(err) => {
                return Err(err);
            }
        },
    };

    let result_filename = {
        let postfix = match object_type {
            ObjectType::Sequence => "s",
            ObjectType::Author => "a",
            ObjectType::Translator => "t",
        };

        format!("{result_filename}_{postfix}")
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
        ]
        .to_vec();

        let replace_transliterator = Transliterator::new(replace_char_map);
        let normal_filename = replace_transliterator.convert(&filename_without_type, false);

        let normal_filename = normal_filename.replace(|c: char| !c.is_ascii(), "");

        let right_part = format!(".{file_format}.zip");
        let normal_filename_slice =
            std::cmp::min(64 - right_part.len() - 1, normal_filename.len() - 1);

        let left_part = if normal_filename_slice == normal_filename.len() - 1 {
            &normal_filename
        } else {
            normal_filename
                .get(..normal_filename_slice)
                .unwrap_or_else(|| {
                    panic!(
                        "Can't slice left part: {:?} {:?}",
                        normal_filename, normal_filename_slice
                    )
                })
        };

        format!("{left_part}{right_part}")
    };

    Ok(final_filename)
}
