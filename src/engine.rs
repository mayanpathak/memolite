// use std::collections::HashMap;
// use std::path::Path;

// use chrono::{DateTime, TimeZone, Utc};
// use rusqlite::{params, Connection, OptionalExtension, Row};
// use uuid::Uuid;

// use crate::error::{MemoliteError, Result};
// use crate::memory::{Memory, MemoryType};

// /// SQLite-backed memory engine.
// ///
// /// This is responsible only for persistence. Retrieval, ranking,
// /// semantic search, decay, and consolidation are implemented elsewhere.
// pub struct MemoryEngine {
//     conn: Connection,
// }

// impl MemoryEngine {
//     /// Opens (or creates) the SQLite database and ensures the required schema
//     /// exists.
//     pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
//         let conn = Connection::open(path)?;

//         conn.execute_batch(
//             r#"
//             CREATE TABLE IF NOT EXISTS memories (
//                 id              TEXT PRIMARY KEY,
//                 content         TEXT NOT NULL,
//                 type            TEXT NOT NULL CHECK(type IN ('semantic','episodic','procedural','working')),
//                 importance      REAL NOT NULL DEFAULT 0.5 CHECK(importance BETWEEN 0.0 AND 1.0),
//                 access_count    INTEGER NOT NULL DEFAULT 0,
//                 created_at      INTEGER NOT NULL,
//                 last_accessed   INTEGER NOT NULL,
//                 expires_at      INTEGER,
//                 superseded_by   TEXT REFERENCES memories(id),
//                 metadata        TEXT DEFAULT '{}'
//             );
//             "#,
//         )?;

//         Ok(Self { conn })
//     }

//     /// Stores a new memory and returns its generated UUID.
//     ///
//     /// The expiration timestamp is derived automatically from the memory
//     /// type's default TTL:
//     ///
//     /// - Semantic: 365 days
//     /// - Episodic: 30 days
//     /// - Procedural: 730 days
//     /// - Working: 4 hours
//     ///
//     /// Callers do not specify an expiry themselves yet. If custom TTLs are
//     /// added later, they'll likely be exposed through a
//     /// `store_with_options()` API instead.
//     pub async fn store(
//         &self,
//         content: &str,
//         memory_type: MemoryType,
//         importance: f32,
//     ) -> Result<String> {
//         let id = Uuid::new_v4().to_string();
//         let created_at = Utc::now();

//         // Compute the expiration timestamp immediately when the memory is
//         // created so cleanup becomes a simple SQL DELETE later.
//         let ttl = memory_type.default_ttl();
//         let expires_at = created_at + ttl;

//         self.conn.execute(
//             r#"
//             INSERT INTO memories (
//                 id,
//                 content,
//                 type,
//                 importance,
//                 access_count,
//                 created_at,
//                 last_accessed,
//                 expires_at,
//                 superseded_by,
//                 metadata
//             )
//             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
//             "#,
//             params![
//                 id,
//                 content,
//                 memory_type.as_str(),
//                 importance,
//                 0i64,
//                 created_at.timestamp(),
//                 created_at.timestamp(),
//                 Some(expires_at.timestamp()),
//                 Option::<String>::None,
//                 "{}",
//             ],
//         )?;

//         Ok(id)
//     }

//     /// Retrieves memories relevant to the supplied query.
//     ///
//     /// Semantic retrieval has not been implemented yet.
//     pub async fn recall(&self, query: &str) -> Result<Vec<Memory>> {
//         let _ = query;
//         todo!()
//     }

//     /// Fetches a memory by its ID.
//     ///
//     /// Returns:
//     ///
//     /// - `Ok(Some(memory))` if the memory exists.
//     /// - `Ok(None)` if no memory exists with that ID.
//     /// - `Err(...)` if the database row is malformed or another database
//     ///   operation fails.
//     pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
//         let memory = self
//             .conn
//             .query_row(
//                 r#"
//                 SELECT
//                     id,
//                     content,
//                     type,
//                     importance,
//                     access_count,
//                     created_at,
//                     last_accessed,
//                     expires_at,
//                     superseded_by,
//                     metadata
//                 FROM memories
//                 WHERE id = ?1
//                 "#,
//                 params![id],
//                 row_to_memory,
//             )
//             .optional()?;

//         Ok(memory)
//     }

//     /// Permanently deletes a memory.
//     ///
//     /// This performs a hard delete. Calling it for an ID that does not exist
//     /// is considered successful and simply affects zero rows.
//     pub async fn forget(&self, id: &str) -> Result<()> {
//         self.conn
//             .execute("DELETE FROM memories WHERE id = ?1", params![id])?;

//         Ok(())
//     }

//     /// Removes every expired memory from the database.
//     ///
//     /// Only rows whose `expires_at` timestamp is earlier than the current
//     /// time are removed. Rows with `expires_at IS NULL` are treated as
//     /// permanent memories and are left untouched.
//     ///
//     /// Returns the number of deleted rows.
//     pub async fn purge_expired(&self) -> Result<usize> {
//         let now = Utc::now().timestamp();

//         let deleted = self.conn.execute(
//             "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
//             params![now],
//         )?;

//         Ok(deleted)
//     }
// }

// /// Converts a SQLite row from the `memories` table into a [`Memory`].
// ///
// /// This function reverses every conversion performed by
// /// [`MemoryEngine::store`]:
// ///
// /// - `TEXT` → `Uuid`
// /// - `TEXT` → `MemoryType`
// /// - Unix timestamps → `DateTime<Utc>`
// /// - JSON text → `HashMap<String, serde_json::Value>`
// ///
// /// This intentionally returns `rusqlite::Result<Memory>` because
// /// `query_row()` and `query_map()` require that exact signature.
// fn row_to_memory(row: &Row) -> rusqlite::Result<Memory> {
//     // ---------------------------------------------------------------------
//     // id: TEXT -> Uuid
//     // ---------------------------------------------------------------------
//     let id_str: String = row.get(0)?;
//     let id = Uuid::parse_str(&id_str).map_err(|e| to_sql_conversion_err(0, e))?;

//     // ---------------------------------------------------------------------
//     // content
//     // ---------------------------------------------------------------------
//     let content: String = row.get(1)?;

//     // ---------------------------------------------------------------------
//     // type: TEXT -> MemoryType
//     // ---------------------------------------------------------------------
//     let type_str: String = row.get(2)?;
//     let memory_type =
//     MemoryType::parse_str(&type_str).map_err(|e| to_sql_conversion_err(2, e))?;
//     // ---------------------------------------------------------------------
//     // importance
//     // ---------------------------------------------------------------------
//     let importance: f32 = row.get(3)?;

//     // ---------------------------------------------------------------------
//     // access_count: INTEGER -> u32
//     // ---------------------------------------------------------------------
//     let access_count: i64 = row.get(4)?;
//     let access_count = access_count as u32;

//     // ---------------------------------------------------------------------
//     // created_at
//     // ---------------------------------------------------------------------
//     let created_at_ts: i64 = row.get(5)?;
//     let created_at = timestamp_to_datetime(created_at_ts, 5)?;

//     // ---------------------------------------------------------------------
//     // last_accessed
//     // ---------------------------------------------------------------------
//     let last_accessed_ts: i64 = row.get(6)?;
//     let last_accessed = timestamp_to_datetime(last_accessed_ts, 6)?;

//     // ---------------------------------------------------------------------
//     // expires_at: nullable INTEGER -> Option<DateTime<Utc>>
//     // ---------------------------------------------------------------------
//     let expires_at_ts: Option<i64> = row.get(7)?;
//     let expires_at = expires_at_ts
//         .map(|ts| timestamp_to_datetime(ts, 7))
//         .transpose()?;

//     // ---------------------------------------------------------------------
//     // superseded_by: nullable TEXT -> Option<Uuid>
//     // ---------------------------------------------------------------------
//     let superseded_by_str: Option<String> = row.get(8)?;
//     let superseded_by = superseded_by_str
//         .map(|s| Uuid::parse_str(&s))
//         .transpose()
//         .map_err(|e| to_sql_conversion_err(8, e))?;

//     // ---------------------------------------------------------------------
//     // metadata: JSON TEXT -> HashMap
//     // ---------------------------------------------------------------------
//     let metadata_str: String = row.get(9)?;
//     let metadata: HashMap<String, serde_json::Value> =
//         serde_json::from_str(&metadata_str)
//             .map_err(|e| to_sql_conversion_err(9, e))?;

//     Ok(Memory {
//         id,
//         content,
//         memory_type,
//         importance,
//         access_count,
//         created_at,
//         last_accessed,
//         expires_at,
//         metadata,
//         superseded_by,
//     })
// }

// /// Wraps an arbitrary conversion error inside
// /// `rusqlite::Error::FromSqlConversionFailure`.
// ///
// /// `rusqlite` requires row conversion failures to be expressed using its own
// /// error type. This helper avoids repeating the same boilerplate throughout
// /// `row_to_memory()`.
// fn to_sql_conversion_err<E>(col: usize, err: E) -> rusqlite::Error
// where
//     E: std::error::Error + Send + Sync + 'static,
// {
//     rusqlite::Error::FromSqlConversionFailure(
//         col,
//         rusqlite::types::Type::Text,
//         Box::new(err),
//     )
// }

// /// Converts a Unix timestamp stored in SQLite into a `DateTime<Utc>`.
// ///
// /// Returns a `rusqlite` conversion error instead of panicking if the stored
// /// timestamp falls outside Chrono's supported range.
// fn timestamp_to_datetime(
//     ts: i64,
//     col: usize,
// ) -> rusqlite::Result<DateTime<Utc>> {
//     Utc.timestamp_opt(ts, 0)
//         .single()
//         .ok_or_else(|| to_sql_conversion_err(col, MemoliteError::InvalidTimestamp(ts)))
// }




use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use uuid::Uuid;

use crate::embedder::Embedder;
use crate::error::{MemoliteError, Result};
use crate::memory::{Memory, MemoryType};

/// SQLite-backed memory engine.
///
/// This is responsible for persistence *and* for producing the embedding
/// that backs semantic search. Ranking, decay, and consolidation are
/// implemented elsewhere.
pub struct MemoryEngine {
    conn: Connection,
    // Wrapped in a `Mutex` so that `embed()` (which needs `&mut Embedder`)
    // can be called from methods that only take `&self`. `store()` is the
    // only caller today, but every future caller benefits from not having
    // to take `&mut self` (and therefore `&mut MemoryEngine` everywhere the
    // engine is shared, e.g. behind an `Arc`).
    embedder: Mutex<Embedder>,
}

impl MemoryEngine {
    /// Opens (or creates) the SQLite database, ensures the required schema
    /// exists, and loads the local embedding model.
    ///
    /// The embedder is constructed exactly once here and stored on the
    /// engine, since loading the model is expensive and every `store()`
    /// call reuses it rather than reloading it.
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

            CREATE TABLE IF NOT EXISTS embeddings (
                memory_id   TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
                vector      BLOB NOT NULL,
                dimension   INTEGER NOT NULL
            );
            "#,
        )?;

        let embedder = Embedder::new()?;

        Ok(Self {
            conn,
            embedder: Mutex::new(embedder),
        })
    }

    /// Stores a new memory, generates its embedding, and persists both.
    ///
    /// The expiration timestamp is derived automatically from the memory
    /// type's default TTL:
    ///
    /// - Semantic: 365 days
    /// - Episodic: 30 days
    /// - Procedural: 730 days
    /// - Working: 4 hours
    ///
    /// Callers do not specify an expiry themselves yet. If custom TTLs are
    /// added later, they'll likely be exposed through a
    /// `store_with_options()` API instead.
    ///
    /// As of this milestone, every call to `store()` also silently produces
    /// an embedding for `content` and persists it in the `embeddings` table,
    /// keyed by the same memory id. There is no vector *search* yet (that's
    /// milestone 3) — this only guarantees the embedding exists once a
    /// memory has been stored.
    pub async fn store(
        &self,
        content: &str,
        memory_type: MemoryType,
        importance: f32,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now();

        // Compute the expiration timestamp immediately when the memory is
        // created so cleanup becomes a simple SQL DELETE later.
        let ttl = memory_type.default_ttl();
        let expires_at = created_at + ttl;

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
                created_at.timestamp(),
                created_at.timestamp(),
                Some(expires_at.timestamp()),
                Option::<String>::None,
                "{}",
            ],
        )?;

        // Embed the content and persist the vector alongside the memory
        // row. If embedding fails (e.g. empty content), the memory row
        // itself is already committed — we surface the error to the caller
        // rather than silently leaving the memory without a vector, since a
        // memory with no embedding would be invisible to future semantic
        // search.
        //
        // Locking here rather than storing `Embedder` directly is what lets
        // this method stay `&self` instead of `&mut self`.
        let vector = {
            let mut embedder = self
                .embedder
                .lock()
                .map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
            embedder.embed(content)?
        };
        let dimension = vector.len();

        let encoded_vector = bincode::serialize(&vector)
            .map_err(|e| MemoliteError::EmbeddingEncode(e.to_string()))?;

        self.conn.execute(
            r#"
            INSERT INTO embeddings (memory_id, vector, dimension)
            VALUES (?1, ?2, ?3)
            "#,
            params![id, encoded_vector, dimension as i64],
        )?;

        Ok(id)
    }

    /// Retrieves memories relevant to the supplied query.
    ///
    /// Semantic retrieval has not been implemented yet.
    pub async fn recall(&self, query: &str) -> Result<Vec<Memory>> {
        let _ = query;
        todo!()
    }

    /// Fetches a memory by its ID.
    ///
    /// Returns:
    ///
    /// - `Ok(Some(memory))` if the memory exists.
    /// - `Ok(None)` if no memory exists with that ID.
    /// - `Err(...)` if the database row is malformed or another database
    ///   operation fails.
    pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
        let memory = self
            .conn
            .query_row(
                r#"
                SELECT
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
                FROM memories
                WHERE id = ?1
                "#,
                params![id],
                row_to_memory,
            )
            .optional()?;

        Ok(memory)
    }

    /// Fetches the raw embedding vector stored for a given memory id, if any.
    ///
    /// This is primarily useful for tests and debugging right now; the
    /// actual `VectorStore`-backed search path lands in milestone 3.
    pub async fn get_embedding(&self, memory_id: &str) -> Result<Option<Vec<f32>>> {
        let row: Option<(Vec<u8>, i64)> = self
            .conn
            .query_row(
                "SELECT vector, dimension FROM embeddings WHERE memory_id = ?1",
                params![memory_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let Some((blob, _dimension)) = row else {
            return Ok(None);
        };

        let vector: Vec<f32> = bincode::deserialize(&blob)
            .map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;

        Ok(Some(vector))
    }

    /// Returns the dimension of vectors produced by this engine's embedder.
    pub fn dimension(&self) -> usize {
        self.embedder
            .lock()
            .expect("embedder mutex poisoned")
            .dimension()
    }

    /// Permanently deletes a memory.
    ///
    /// This performs a hard delete. Calling it for an ID that does not exist
    /// is considered successful and simply affects zero rows. The
    /// corresponding `embeddings` row is removed automatically via
    /// `ON DELETE CASCADE`.
    pub async fn forget(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;

        Ok(())
    }

    /// Removes every expired memory from the database.
    ///
    /// Only rows whose `expires_at` timestamp is earlier than the current
    /// time are removed. Rows with `expires_at IS NULL` are treated as
    /// permanent memories and are left untouched.
    ///
    /// Returns the number of deleted rows.
    pub async fn purge_expired(&self) -> Result<usize> {
        let now = Utc::now().timestamp();

        let deleted = self.conn.execute(
            "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
            params![now],
        )?;

        Ok(deleted)
    }
}

/// Converts a SQLite row from the `memories` table into a [`Memory`].
///
/// This function reverses every conversion performed by
/// [`MemoryEngine::store`]:
///
/// - `TEXT` → `Uuid`
/// - `TEXT` → `MemoryType`
/// - Unix timestamps → `DateTime<Utc>`
/// - JSON text → `HashMap<String, serde_json::Value>`
///
/// This intentionally returns `rusqlite::Result<Memory>` because
/// `query_row()` and `query_map()` require that exact signature.
fn row_to_memory(row: &Row) -> rusqlite::Result<Memory> {
    // ---------------------------------------------------------------------
    // id: TEXT -> Uuid
    // ---------------------------------------------------------------------
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| to_sql_conversion_err(0, e))?;

    // ---------------------------------------------------------------------
    // content
    // ---------------------------------------------------------------------
    let content: String = row.get(1)?;

    // ---------------------------------------------------------------------
    // type: TEXT -> MemoryType
    // ---------------------------------------------------------------------
    let type_str: String = row.get(2)?;
    let memory_type =
        MemoryType::parse_str(&type_str).map_err(|e| to_sql_conversion_err(2, e))?;
    // ---------------------------------------------------------------------
    // importance
    // ---------------------------------------------------------------------
    let importance: f32 = row.get(3)?;

    // ---------------------------------------------------------------------
    // access_count: INTEGER -> u32
    // ---------------------------------------------------------------------
    let access_count: i64 = row.get(4)?;
    let access_count = access_count as u32;

    // ---------------------------------------------------------------------
    // created_at
    // ---------------------------------------------------------------------
    let created_at_ts: i64 = row.get(5)?;
    let created_at = timestamp_to_datetime(created_at_ts, 5)?;

    // ---------------------------------------------------------------------
    // last_accessed
    // ---------------------------------------------------------------------
    let last_accessed_ts: i64 = row.get(6)?;
    let last_accessed = timestamp_to_datetime(last_accessed_ts, 6)?;

    // ---------------------------------------------------------------------
    // expires_at: nullable INTEGER -> Option<DateTime<Utc>>
    // ---------------------------------------------------------------------
    let expires_at_ts: Option<i64> = row.get(7)?;
    let expires_at = expires_at_ts
        .map(|ts| timestamp_to_datetime(ts, 7))
        .transpose()?;

    // ---------------------------------------------------------------------
    // superseded_by: nullable TEXT -> Option<Uuid>
    // ---------------------------------------------------------------------
    let superseded_by_str: Option<String> = row.get(8)?;
    let superseded_by = superseded_by_str
        .map(|s| Uuid::parse_str(&s))
        .transpose()
        .map_err(|e| to_sql_conversion_err(8, e))?;

    // ---------------------------------------------------------------------
    // metadata: JSON TEXT -> HashMap
    // ---------------------------------------------------------------------
    let metadata_str: String = row.get(9)?;
    let metadata: HashMap<String, serde_json::Value> =
        serde_json::from_str(&metadata_str).map_err(|e| to_sql_conversion_err(9, e))?;

    Ok(Memory {
        id,
        content,
        memory_type,
        importance,
        access_count,
        created_at,
        last_accessed,
        expires_at,
        metadata,
        superseded_by,
    })
}

/// Wraps an arbitrary conversion error inside
/// `rusqlite::Error::FromSqlConversionFailure`.
///
/// `rusqlite` requires row conversion failures to be expressed using its own
/// error type. This helper avoids repeating the same boilerplate throughout
/// `row_to_memory()`.
fn to_sql_conversion_err<E>(col: usize, err: E) -> rusqlite::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(err))
}

/// Converts a Unix timestamp stored in SQLite into a `DateTime<Utc>`.
///
/// Returns a `rusqlite` conversion error instead of panicking if the stored
/// timestamp falls outside Chrono's supported range.
fn timestamp_to_datetime(ts: i64, col: usize) -> rusqlite::Result<DateTime<Utc>> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .ok_or_else(|| to_sql_conversion_err(col, MemoliteError::InvalidTimestamp(ts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 34: store a memory, then directly query the `embeddings` table,
    /// and assert a blob exists with the right dimension.
    #[tokio::test]
    async fn store_persists_an_embedding_of_the_right_dimension() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let id = engine
            .store("user prefers dark mode", MemoryType::Semantic, 0.8)
            .await
            .expect("store should succeed");

        let vector = engine
            .get_embedding(&id)
            .await
            .expect("query should succeed")
            .expect("embedding should exist for a stored memory");

        assert_eq!(vector.len(), engine.dimension());
        assert!(vector.iter().any(|v| *v != 0.0), "embedding should not be all zeros");
    }

    /// Step 37: embedding failures (e.g. empty content) should surface as a
    /// proper error, not a panic, and should not silently store a memory
    /// with no embedding.
    #[tokio::test]
    async fn store_rejects_empty_content() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let result = engine.store("   ", MemoryType::Working, 0.5).await;

        assert!(result.is_err(), "storing empty content should fail");
    }
}