use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// The category of a memory.
///
/// Different memory types will eventually have different
/// decay rates and default lifetimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    Semantic,
    Episodic,
    Procedural,
    Working,
}

/// A stored memory.
///
/// This struct contains only the data representation.
/// Retrieval, ranking, decay, and persistence logic are implemented elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    /// Unique identifier (UUID v4).
    pub id: Uuid,

    /// Raw memory content.
    pub content: String,

    /// Category of the memory.
    pub memory_type: MemoryType,

    /// Importance score (0.0–1.0).
    pub importance: f32,

    /// Number of successful recalls.
    pub access_count: u32,

    /// When the memory was created.
    pub created_at: DateTime<Utc>,

    /// Last time this memory was accessed.
    pub last_accessed: DateTime<Utc>,

    /// Optional expiration timestamp.
    pub expires_at: Option<DateTime<Utc>>,

    /// Arbitrary user-defined metadata.
    pub metadata: HashMap<String, Value>,

    /// ID of the memory that supersedes this one, if any.
    pub superseded_by: Option<Uuid>,
}

