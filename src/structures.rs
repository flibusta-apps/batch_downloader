use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;


#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    InProgress,
    Archiving,
    Complete
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Sequence,
    Author,
    Translator
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CreateTask{
    pub object_id: u32,
    pub object_type: ObjectType,
    pub file_format: String,
    pub allowed_langs: SmallVec<[SmartString; 3]>
}

#[derive(Serialize, Clone)]
pub struct Task {
    pub id: String,
    pub status: TaskStatus,
    pub result_filename: Option<String>,
    pub result_link: Option<String>
}
