use serde::{Deserialize, Serialize};

/// Search hit shared by the knowledge daemon and optional file manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitRow {
    pub score: f32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chunk_id: String,
    pub file_path: String,
    pub chunk_index: u32,
    pub chunk_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_score: Option<f32>,
}
