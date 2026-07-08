use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use uuid::Uuid;

use crate::memory::{Memory, MemoryType};

pub struct MemoryEngine {
    conn: Connection,
}

impl MemoryEngine {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id              TEXT PRIMARY KEY,
                content         TEXT NOT NULL,
                type            TEXT NOT NULL CHECK(type IN ('semantic','episodic','procedural','working')),
                importance      REAL NOT NULL DEFAULT 0.5 CHECK(importance BETWEEN 0.0 AND 1.0),
                access_count    INTEGER NOT NULL DEFAULT 0,
                created_at      INTEGER NOT NULL,
                last_accessed   INTEGER NOT NULL,
                expires_at      INTEGER,
                superseded_by   TEXT REFERENCES memories(id),
                metadata        TEXT DEFAULT '{}'
            );
            "#,
        )?;

        Ok(Self { conn })
    }

    pub async fn store(
        &self,
        content: &str,
        memory_type: MemoryType,
        importance: f32,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();

        self.conn.execute(
            r#"
            INSERT INTO memories (
                id,
                content,
                type,
                importance,
                access_count,
                created_at,
                last_accessed,
                expires_at,
                superseded_by,
                metadata
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            params![
                id,
                content,
                memory_type.as_str(),
                importance,
                0i64,
                now,
                now,
                Option::<i64>::None,
                Option::<String>::None,
                "{}",
            ],
        )?;

        Ok(id)
    }

    pub async fn recall(&self, query: &str) -> Result<Vec<Memory>> {
        let _ = query;
        todo!()
    }

    pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
        let _ = id;
        todo!()
    }
}

impl MemoryType {
    fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Semantic => "semantic",
            MemoryType::Episodic => "episodic",
            MemoryType::Procedural => "procedural",
            MemoryType::Working => "working",
        }
    }
}