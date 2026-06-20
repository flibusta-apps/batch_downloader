use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;

#[derive(Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    InProgress,
    Archiving,
    Complete,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Sequence,
    Author,
    Translator,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CreateTask {
    pub object_id: u32,
    pub object_type: ObjectType,
    pub file_format: SmartString,
    pub allowed_langs: SmallVec<[SmartString; 3]>,

    /// Optional user ID forwarded to TFCS as X-User-Id for per-user rate limiting.
    #[serde(default)]
    pub user_id: Option<i64>,

    /// Normalize the generated archive filename (transliterate via GOST 7.79B).
    /// `true` (default) preserves the legacy behaviour.
    /// `false` keeps the original Cyrillic in the filename.
    #[serde(default = "default_true")]
    pub normalized: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Clone)]
pub struct Task {
    pub id: String,
    pub status: TaskStatus,
    pub status_description: String,
    pub error_message: Option<String>,

    pub result_filename: Option<String>,
    pub content_size: Option<u64>,
}
