use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The category of a memory.
///
/// Different memory types have different default lifetimes and decay
/// characteristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    Semantic,
    Episodic,
    Procedural,
    Working,
}

impl MemoryType {
    /// Converts a `MemoryType` into the string form stored in SQLite's
    /// `type` column (see the `CHECK(type IN (...))` constraint in the schema).
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Semantic => "semantic",
            MemoryType::Episodic => "episodic",
            MemoryType::Procedural => "procedural",
            MemoryType::Working => "working",
        }
    }

    /// Returns how long a memory of this type lives before it's eligible for
    /// `purge_expired()` to delete it.
    ///
    /// These are the per-type defaults from the design doc. Nothing fancy —
    /// just a lookup table. `Working` memories are deliberately short-lived
    /// scratch context; `Procedural` memories (skills/habits) are the
    /// longest-lived since they represent durable patterns.
    pub fn default_ttl(&self) -> chrono::Duration {
        match self {
            MemoryType::Semantic => chrono::Duration::days(365),
            MemoryType::Episodic => chrono::Duration::days(30),
            MemoryType::Procedural => chrono::Duration::days(730),
            MemoryType::Working => chrono::Duration::hours(4),
        }
    }

    /// Parses a `MemoryType` back out of the string stored in SQLite.
    ///
    /// This is the inverse of [`MemoryType::as_str`]. Returns
    /// `MemoliteError::InvalidMemoryType` instead of panicking if the row
    /// somehow contains a value outside the `CHECK` constraint.
    ///
    /// Named `parse_str` rather than `from_str` to avoid Clippy's
    /// `should_implement_trait` lint — a method literally named `from_str`
    /// looks like it's meant to implement `std::str::FromStr`, which this
    /// doesn't (no `.parse::<MemoryType>()` support here, just a plain
    /// inherent method).
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
