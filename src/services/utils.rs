use bytes::Buf;
use reqwest::Response;
use smartstring::alias::String as SmartString;
use tempfile::SpooledTempFile;
use translit::{gost779b_ru, CharsMapping, Transliterator};

use std::io::{Seek, SeekFrom, Write};

use crate::structures::{CreateTask, ObjectType};

use super::library_client::{get_author, get_sequence};

pub fn get_key(input_data: CreateTask) -> String {
    let mut data = input_data.clone();
    data.allowed_langs.sort();

    let data_string = serde_json::to_string(&data).unwrap();

    format!("{:x}", md5::compute(data_string))
}

pub async fn response_to_tempfile(
    res: &mut Response,
) -> Result<(SpooledTempFile, usize), Box<dyn std::error::Error + Send + Sync>> {
    let mut tmp_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

    let mut data_size: usize = 0;

    {
        loop {
            let chunk = res.chunk().await;

            let result = match chunk {
                Ok(v) => v,
                Err(err) => return Err(Box::new(err)),
            };

            let data = match result {
                Some(v) => v,
                None => break,
            };

            data_size += data.len();

            tmp_file.write_all(data.chunk())?;
        }

        tmp_file.seek(SeekFrom::Start(0)).unwrap();
    }

    Ok((tmp_file, data_size))
}

/// Maximum size of the `<left>` part of the filename in UTF-8 bytes.
/// 50 leaves room for `.{file_format}.zip` to stay well under 60 bytes
/// (the Telegram Bot API limit, kept for parity with `books_downloader`).
const LEFT_MAX_BYTES: usize = 50;

pub async fn get_filename(
    object_type: ObjectType,
    object_id: u32,
    file_format: SmartString,
    normalized: bool,
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

    Ok(normalize_filename(
        &result_filename,
        normalized,
        &file_format,
    ))
}

/// Normalize a raw filename into the final archive name.
///
/// Pipeline (order matters):
/// 1. Pre-cleanup: `№` → `N`, drop `«`/`»`.
/// 2. If `normalized`: GOST 7.79B transliteration.
/// 3. Drop symbol-level punctuation: `(),.…’!?"':`.
/// 4. Replace char map (em/en dash, slash, space, etc.).
/// 5. If `normalized`: drop any remaining non-ASCII (legacy behaviour).
/// 6. Telegram-unsafe cleanup: `\|:*"<>?!` — applied in both modes.
/// 7. Trim `<left>` to at most `LEFT_MAX_BYTES` UTF-8 bytes via `floor_char_boundary`.
/// 8. Collapse trailing separators (`_`, `-`, `.`, space) in `<left>`.
/// 9. Glue as `{left}.{file_format}.zip`.
pub fn normalize_filename(input: &str, normalized: bool, file_format: &str) -> String {
    // 1. Pre-cleanup (always, before transliteration so that GOST doesn't
    //    turn `№` into `#`).
    let mut s = input.replace('№', "N").replace(['«', '»'], "");

    // 2. Transliteration.
    if normalized {
        let transliterator = Transliterator::new(gost779b_ru());
        s = transliterator.convert(&s, false);
    }

    // 3. Symbol-level cleanup.
    for ch in ['(', ')', ',', '.', '…', '’', '!', '"', '?', '\'', ':'] {
        s = s.replace(ch, "");
    }

    // 4. Char map.
    let replace_char_map: CharsMapping = [
        ("—", "-"),
        ("–", "-"),
        ("/", "_"),
        (" ", "_"),
        ("á", "a"),
        ("[", ""),
        ("]", ""),
        ("`", ""),
        ("\"", ""),
        ("'", ""),
    ]
    .to_vec();

    let replace_transliterator = Transliterator::new(replace_char_map);
    s = replace_transliterator.convert(&s, false);

    // 5. Drop non-ASCII if normalized (legacy behaviour).
    if normalized {
        s = s.replace(|c: char| !c.is_ascii(), "");
    }

    // 6. Telegram-unsafe cleanup (always, in both modes).
    for ch in ['\\', '|', '*', '<', '>'] {
        s = s.replace(ch, "");
    }

    // 7. Trim <left> to LEFT_MAX_BYTES UTF-8 bytes.
    let right_part = format!(".{file_format}.zip");
    let left_max = LEFT_MAX_BYTES.min(s.len());
    let slice_end = s.floor_char_boundary(left_max);
    let mut left_part = &s[..slice_end];

    // 8. Collapse trailing separators in <left>.
    while let Some(last) = left_part.chars().last() {
        if matches!(last, '_' | '-' | '.' | ' ') {
            left_part = &left_part[..left_part.len() - last.len_utf8()];
        } else {
            break;
        }
    }

    // 9. Glue.
    format!("{left_part}{right_part}")
}

#[cfg(test)]
mod tests {
    use super::normalize_filename;

    #[test]
    fn normalized_true_transliterates() {
        // GOST 7.79B: ё → yo
        assert_eq!(
            normalize_filename("Усачёв_Приключения_Кота_s", true, "fb2"),
            "Usachyov_Priklyucheniya_Kota_s.fb2.zip"
        );
    }

    #[test]
    fn normalized_false_keeps_cyrillic() {
        // Short input that fits under the 50-byte <left> cap.
        assert_eq!(
            normalize_filename("Усачёв_Кот_s", false, "fb2"),
            "Усачёв_Кот_s.fb2.zip"
        );
        // Longer input — the trailing `_s` is sliced off by the 50-byte cap
        // and the leftover trailing `_` is collapsed.
        let out = normalize_filename("Усачёв_А_А_Приключения_Кота_s", false, "fb2");
        assert!(out.ends_with(".fb2.zip"));
        let left = out.strip_suffix(".fb2.zip").unwrap();
        assert!(left.ends_with("Кота"), "left was: {left}");
        assert!(left.len() <= 50, "left was {} bytes", left.len());
    }

    #[test]
    fn punctuation_dropped_normalized() {
        assert_eq!(
            normalize_filename("Что? Где? Когда!_s", true, "fb2"),
            "Chto_Gde_Kogda_s.fb2.zip"
        );
    }

    #[test]
    fn quotation_marks_dropped_both_modes() {
        assert_eq!(
            normalize_filename("«Котобой»_s", true, "fb2"),
            "Kotoboj_s.fb2.zip"
        );
        assert_eq!(
            normalize_filename("«Котобой»_s", false, "fb2"),
            "Котобой_s.fb2.zip"
        );
    }

    #[test]
    fn numero_replaced_with_n_both_modes() {
        assert_eq!(normalize_filename("№ 42_s", true, "fb2"), "N_42_s.fb2.zip");
        assert_eq!(normalize_filename("№ 42_s", false, "fb2"), "N_42_s.fb2.zip");
    }

    #[test]
    fn em_dash_to_hyphen() {
        assert_eq!(
            normalize_filename("А — Б_s", true, "fb2"),
            "A_-_B_s.fb2.zip"
        );
    }

    #[test]
    fn slash_to_underscore() {
        assert_eq!(
            normalize_filename("Война/и/мир_s", true, "fb2"),
            "Vojna_i_mir_s.fb2.zip"
        );
    }

    #[test]
    fn file_format_propagates() {
        assert_eq!(
            normalize_filename("Book_s", true, "epub"),
            "Book_s.epub.zip"
        );
    }

    #[test]
    fn telegram_unsafe_dropped_normalized() {
        // Tabs/quotes/brackets/etc. should be gone.
        let out = normalize_filename("A\\|B:C*D\"E<F>G?H!I_s", true, "fb2");
        for ch in ['\\', '|', ':', '*', '"', '<', '>', '?', '!'] {
            assert!(
                !out.contains(ch),
                "expected `{ch}` to be dropped, got: {out}"
            );
        }
        assert!(out.ends_with("_s.fb2.zip"));
    }

    #[test]
    fn telegram_unsafe_dropped_unnormalized() {
        let out = normalize_filename("Кот\\|Бой:C*D\"Е<F>G?H!I_s", false, "fb2");
        for ch in ['\\', '|', ':', '*', '"', '<', '>', '?', '!'] {
            assert!(
                !out.contains(ch),
                "expected `{ch}` to be dropped, got: {out}"
            );
        }
        // Cyrillic survives in non-normalized mode.
        assert!(out.contains("Кот"));
        assert!(out.contains("Бой"));
        assert!(out.ends_with("_s.fb2.zip"));
    }

    #[test]
    fn trailing_separators_collapsed() {
        // `Что?!` becomes `Chto` (the `?!` are dropped) — no trailing sep.
        let out = normalize_filename("Что?!_s", true, "fb2");
        let left = out.strip_suffix(".fb2.zip").unwrap();
        assert!(
            !left.ends_with('_')
                && !left.ends_with('-')
                && !left.ends_with('.')
                && !left.ends_with(' '),
            "trailing separator in left part: {left}"
        );
    }

    #[test]
    fn long_cyrillic_does_not_panic_and_stays_under_60_bytes() {
        let long = "Очень".repeat(40);
        let src = format!("{long}_s");
        let out = normalize_filename(&src, true, "fb2");
        assert!(out.ends_with(".fb2.zip"));
        assert!(
            out.len() <= 60,
            "expected ≤ 60 bytes, got {}: {out}",
            out.len()
        );
        // Valid UTF-8 by construction (we slice on a char boundary).
    }

    #[test]
    fn long_cyrillic_keeps_original_under_60_bytes() {
        let long = "Очень".repeat(40);
        let src = format!("{long}_s");
        let out = normalize_filename(&src, false, "fb2");
        assert!(out.ends_with(".fb2.zip"));
        assert!(
            out.len() <= 60,
            "expected ≤ 60 bytes, got {}: {out}",
            out.len()
        );
    }

    #[test]
    fn left_part_respects_50_byte_cap_normalized() {
        let long = "A".repeat(200);
        let src = format!("{long}_s");
        let out = normalize_filename(&src, true, "fb2");
        let left = out.strip_suffix(".fb2.zip").unwrap();
        assert!(left.len() <= 50, "left part was {} bytes", left.len());
    }

    #[test]
    fn author_postfixes_preserved() {
        assert_eq!(
            normalize_filename("Author_a", true, "fb2"),
            "Author_a.fb2.zip"
        );
        assert_eq!(
            normalize_filename("Translator_t", true, "fb2"),
            "Translator_t.fb2.zip"
        );
        assert_eq!(normalize_filename("Seq_s", true, "fb2"), "Seq_s.fb2.zip");
    }
}
