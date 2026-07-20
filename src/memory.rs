use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::confidence::ConfidenceLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    Semantic,
    Episodic,
    Procedural,
    Working,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Semantic => "semantic",
            MemoryType::Episodic => "episodic",
            MemoryType::Procedural => "procedural",
            MemoryType::Working => "working",
        }
    }

    pub fn default_ttl(&self) -> chrono::Duration {
        match self {
            MemoryType::Semantic => chrono::Duration::days(365),
            MemoryType::Episodic => chrono::Duration::days(30),
            MemoryType::Procedural => chrono::Duration::days(730),
            MemoryType::Working => chrono::Duration::hours(4),
        }
    }

    pub fn parse_str(s: &str) -> crate::error::Result<Self> {
        match s {
            "semantic" => Ok(MemoryType::Semantic),
            "episodic" => Ok(MemoryType::Episodic),
            "procedural" => Ok(MemoryType::Procedural),
            "working" => Ok(MemoryType::Working),
            other => Err(crate::error::MemoliteError::InvalidMemoryType(
                other.to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: Uuid,

    pub content: String,

    pub memory_type: MemoryType,

    pub importance: f32,

    pub access_count: u32,

    pub created_at: DateTime<Utc>,

    pub last_accessed: DateTime<Utc>,

    pub expires_at: Option<DateTime<Utc>>,

    pub metadata: HashMap<String, Value>,

    pub superseded_by: Option<Uuid>,

    pub confidence: ConfidenceLevel,
}