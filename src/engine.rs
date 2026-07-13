


// use std::collections::HashMap;
// use std::path::Path;
// use std::sync::Mutex;

// use chrono::{DateTime, TimeZone, Utc};
// use rusqlite::{params, Connection, OptionalExtension, Row};
// use uuid::Uuid;

// use crate::embedder::Embedder;
// use crate::error::{MemoliteError, Result};
// use crate::memory::{Memory, MemoryType};

// /// SQLite-backed memory engine.
// ///
// /// This is responsible for persistence *and* for producing the embedding
// /// that backs semantic search. Ranking, decay, and consolidation are
// /// implemented elsewhere.
// pub struct MemoryEngine {
//     conn: Connection,
//     // Wrapped in a `Mutex` so that `embed()` (which needs `&mut Embedder`)
//     // can be called from methods that only take `&self`. `store()` is the
//     // only caller today, but every future caller benefits from not having
//     // to take `&mut self` (and therefore `&mut MemoryEngine` everywhere the
//     // engine is shared, e.g. behind an `Arc`).
//     embedder: Mutex<Embedder>,
// }

// impl MemoryEngine {
//     /// Opens (or creates) the SQLite database, ensures the required schema
//     /// exists, and loads the local embedding model.
//     ///
//     /// The embedder is constructed exactly once here and stored on the
//     /// engine, since loading the model is expensive and every `store()`
//     /// call reuses it rather than reloading it.
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

//             CREATE TABLE IF NOT EXISTS embeddings (
//                 memory_id   TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
//                 vector      BLOB NOT NULL,
//                 dimension   INTEGER NOT NULL
//             );
//             "#,
//         )?;

//         let embedder = Embedder::new()?;

//         Ok(Self {
//             conn,
//             embedder: Mutex::new(embedder),
//         })
//     }

//     /// Stores a new memory, generates its embedding, and persists both.
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
//     ///
//     /// As of this milestone, every call to `store()` also silently produces
//     /// an embedding for `content` and persists it in the `embeddings` table,
//     /// keyed by the same memory id. There is no vector *search* yet (that's
//     /// milestone 3) — this only guarantees the embedding exists once a
//     /// memory has been stored.
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

//         // Embed the content and persist the vector alongside the memory
//         // row. If embedding fails (e.g. empty content), the memory row
//         // itself is already committed — we surface the error to the caller
//         // rather than silently leaving the memory without a vector, since a
//         // memory with no embedding would be invisible to future semantic
//         // search.
//         //
//         // Locking here rather than storing `Embedder` directly is what lets
//         // this method stay `&self` instead of `&mut self`.
//         let vector = {
//             let mut embedder = self
//                 .embedder
//                 .lock()
//                 .map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
//             embedder.embed(content)?
//         };
//         let dimension = vector.len();

//         let encoded_vector = bincode::serialize(&vector)
//             .map_err(|e| MemoliteError::EmbeddingEncode(e.to_string()))?;

//         self.conn.execute(
//             r#"
//             INSERT INTO embeddings (memory_id, vector, dimension)
//             VALUES (?1, ?2, ?3)
//             "#,
//             params![id, encoded_vector, dimension as i64],
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

//     /// Fetches the raw embedding vector stored for a given memory id, if any.
//     ///
//     /// This is primarily useful for tests and debugging right now; the
//     /// actual `VectorStore`-backed search path lands in milestone 3.
//     pub async fn get_embedding(&self, memory_id: &str) -> Result<Option<Vec<f32>>> {
//         let row: Option<(Vec<u8>, i64)> = self
//             .conn
//             .query_row(
//                 "SELECT vector, dimension FROM embeddings WHERE memory_id = ?1",
//                 params![memory_id],
//                 |row| Ok((row.get(0)?, row.get(1)?)),
//             )
//             .optional()?;

//         let Some((blob, _dimension)) = row else {
//             return Ok(None);
//         };

//         let vector: Vec<f32> = bincode::deserialize(&blob)
//             .map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;

//         Ok(Some(vector))
//     }

//     /// Returns the dimension of vectors produced by this engine's embedder.
//     pub fn dimension(&self) -> usize {
//         self.embedder
//             .lock()
//             .expect("embedder mutex poisoned")
//             .dimension()
//     }

//     /// Permanently deletes a memory.
//     ///
//     /// This performs a hard delete. Calling it for an ID that does not exist
//     /// is considered successful and simply affects zero rows. The
//     /// corresponding `embeddings` row is removed automatically via
//     /// `ON DELETE CASCADE`.
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
//         MemoryType::parse_str(&type_str).map_err(|e| to_sql_conversion_err(2, e))?;
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
//         serde_json::from_str(&metadata_str).map_err(|e| to_sql_conversion_err(9, e))?;

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
//     rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(err))
// }

// /// Converts a Unix timestamp stored in SQLite into a `DateTime<Utc>`.
// ///
// /// Returns a `rusqlite` conversion error instead of panicking if the stored
// /// timestamp falls outside Chrono's supported range.
// fn timestamp_to_datetime(ts: i64, col: usize) -> rusqlite::Result<DateTime<Utc>> {
//     Utc.timestamp_opt(ts, 0)
//         .single()
//         .ok_or_else(|| to_sql_conversion_err(col, MemoliteError::InvalidTimestamp(ts)))
// }

// #[cfg(test)]
// mod tests {
//     use super::*;

//     /// Step 34: store a memory, then directly query the `embeddings` table,
//     /// and assert a blob exists with the right dimension.
//     #[tokio::test]
//     async fn store_persists_an_embedding_of_the_right_dimension() {
//         let engine = MemoryEngine::open(":memory:")
//             .await
//             .expect("engine should open");

//         let id = engine
//             .store("user prefers dark mode", MemoryType::Semantic, 0.8)
//             .await
//             .expect("store should succeed");

//         let vector = engine
//             .get_embedding(&id)
//             .await
//             .expect("query should succeed")
//             .expect("embedding should exist for a stored memory");

//         assert_eq!(vector.len(), engine.dimension());
//         assert!(vector.iter().any(|v| *v != 0.0), "embedding should not be all zeros");
//     }

//     /// Step 37: embedding failures (e.g. empty content) should surface as a
//     /// proper error, not a panic, and should not silently store a memory
//     /// with no embedding.
//     #[tokio::test]
//     async fn store_rejects_empty_content() {
//         let engine = MemoryEngine::open(":memory:")
//             .await
//             .expect("engine should open");

//         let result = engine.store("   ", MemoryType::Working, 0.5).await;

//         assert!(result.is_err(), "storing empty content should fail");
//     }
// }


use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use uuid::Uuid;

use crate::embedder::Embedder;
use crate::error::{MemoliteError, Result};
use crate::memory::{Memory, MemoryType};
use crate::vector_store::{InMemoryVectorStore, VectorEntry, VectorStore};

/// Column order shared by every `SELECT ... FROM memories` in this file and
/// consumed by `row_to_memory`. Centralized so a future column addition
/// (e.g. `confidence` in M6) is a one-line edit, not a hunt through every
/// query string.
const MEMORY_COLUMNS: &str = "id, content, type, importance, access_count, \
    created_at, last_accessed, expires_at, superseded_by, metadata";

/// Controls what `open_with_store_internal` does to a caller-supplied
/// backend's *existing* remote contents at open time. `open()` itself
/// always uses `ReplaceAll`, which is safe there specifically because the
/// in-memory store it constructs is private to that one engine instance --
/// nothing else could ever be sharing it. A future public
/// `open_with_store()` (M11) will require the caller to choose explicitly,
/// since a remote backend may not be exclusively owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillPolicy {
    /// Do not touch the remote store's contents, and do not even read
    /// local rows to validate them. The cheapest possible open. Safe
    /// default for a backend shared with other data; local rows with no
    /// matching remote vector simply won't be found by recall until an
    /// explicit rebuild is requested later.
    ExistingOnly,
    /// Upsert every local row into the store via `insert`, but never
    /// delete anything the store already has that SQLite doesn't know
    /// about. Safe for a shared backend.
    UpsertLocal,
    /// Make the store's contents exactly this database's rows via
    /// `replace_all`. Only correct when the backend/collection is
    /// dedicated to this one database.
    ReplaceAll,
}

/// SQLite-backed memory engine.
///
/// This is responsible for persistence, embedding generation, and keeping
/// an in-process vector index in sync with SQLite (the durable source of
/// truth). Ranking, decay, and consolidation are implemented elsewhere.
pub struct MemoryEngine {
    // Wrapped in a `Mutex` (rather than a bare `Connection`) so a shared
    // `&self` can be used from every method instead of `&mut self`, which
    // matters once the engine is held behind an `Arc` and called from
    // multiple concurrent tasks. Every method acquires this lock, does its
    // SQLite work, and drops the guard *before* awaiting anything else --
    // holding a std Mutex across an `.await` risks deadlocking another
    // task on the same thread.
    conn: Mutex<Connection>,
    embedder: Mutex<Embedder>,
    vector_store: RwLock<Arc<dyn VectorStore>>,
    // Reserved for the maintenance controller (a later milestone). Declared
    // now, alongside every other field, so this struct's shape never has
    // to change again after Step 0 -- no milestone after this one edits
    // MemoryEngine's field list.
    #[allow(dead_code)]
    maintenance_running: Arc<AtomicBool>,
}

impl MemoryEngine {
    /// Opens (or creates) the SQLite database, backed by the default
    /// in-memory vector store, and loads the local embedding model.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_store_internal(path, None, BackfillPolicy::ReplaceAll).await
    }

    /// Shared constructor.
    ///
    /// - `store_override: None` constructs a fresh `InMemoryVectorStore`
    ///   sized to the embedder's dimension (what `open()` does).
    /// - `store_override: Some(s)` uses a caller-supplied backend `s`
    ///   instead. Not exercised by any public API yet -- reserved for a
    ///   future public `open_with_store()`. `pub(crate)` rather than
    ///   private so that future constructor lives in a different module
    ///   without needing engine.rs to change.
    pub(crate) async fn open_with_store_internal(
        path: impl AsRef<Path>,
        store_override: Option<Arc<dyn VectorStore>>,
        backfill: BackfillPolicy,
    ) -> Result<Self> {
        let mut raw_conn = Connection::open(path)?;
        crate::migrations::run_migrations(&mut raw_conn)?;

        let embedder = Embedder::new()?;
        let dim = embedder.dimension();

        let vector_store: Arc<dyn VectorStore> = match store_override {
            Some(store) => {
                if store.dimension() != dim {
                    return Err(MemoliteError::InvalidArgument(format!(
                        "supplied vector store has dimension {} but the embedder produces {}",
                        store.dimension(),
                        dim
                    )));
                }
                store
            }
            None => Arc::new(InMemoryVectorStore::new(dim)),
        };

        let conn = Mutex::new(raw_conn);
        reconcile_vector_index(&conn, &vector_store, backfill).await?;

        Ok(Self {
            conn,
            embedder: Mutex::new(embedder),
            vector_store: RwLock::new(vector_store),
            maintenance_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Stores a new memory, generates its embedding, and persists both --
    /// then inserts into the live vector index too (not just at
    /// restart-reconcile time). If the vector-store insert fails, the
    /// SQLite rows are compensated away rather than left as an orphaned
    /// memory no `recall()` will ever find.
    pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String> {
        if content.trim().is_empty() {
            return Err(MemoliteError::InvalidArgument("content must not be empty".into()));
        }
        if !(0.0..=1.0).contains(&importance) {
            return Err(MemoliteError::InvalidArgument("importance must be in [0.0, 1.0]".into()));
        }

        let id = Uuid::new_v4();
        let id_str = id.to_string();
        let created_at = Utc::now();
        let ttl = memory_type.default_ttl();
        let expires_at = created_at + ttl;

        // Embed *before* touching SQLite: model work is slower and can
        // fail, and there's no reason to hold a DB lock across it.
        let vector = {
            let mut embedder = self
                .embedder
                .lock()
                .map_err(|_| MemoliteError::Internal("embedder mutex poisoned".into()))?;
            embedder.embed(content)?
        };
        let dimension = vector.len();
        let encoded_vector = bincode::serialize(&vector)
            .map_err(|e| MemoliteError::EmbeddingEncode(e.to_string()))?;

        {
            let mut conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

            // Both rows are written in ONE transaction. reconcile_vector_index
            // (and every future reader that joins memories<->embeddings)
            // depends on "a memories row always has a matching embeddings
            // row" being a real invariant, not just usually true.
            let tx = conn.transaction()?;
            tx.execute(
                r#"
                INSERT INTO memories (
                    id, content, type, importance, access_count,
                    created_at, last_accessed, expires_at, superseded_by, metadata
                )
                VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, NULL, ?7)
                "#,
                params![
                    id_str,
                    content,
                    memory_type.as_str(),
                    importance,
                    created_at.timestamp(),
                    Some(expires_at.timestamp()),
                    "{}",
                ],
            )?;
            tx.execute(
                "INSERT INTO embeddings (memory_id, vector, dimension) VALUES (?1, ?2, ?3)",
                params![id_str, encoded_vector, dimension as i64],
            )?;
            tx.commit()?;
        } // conn guard dropped here, before the vector-store .await below

        let store = {
            let guard = self
                .vector_store
                .read()
                .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            Arc::clone(&*guard)
        };

        if let Err(e) = store.insert(id, &vector, HashMap::new()).await {
            // The SQLite rows are now orphaned relative to the vector
            // index -- a memory recall() can never find. Delete them
            // rather than leave that inconsistency behind.
            let compensation = {
                let conn = self
                    .conn
                    .lock()
                    .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
                conn.and_then(|c| {
                    c.execute("DELETE FROM memories WHERE id = ?1", params![id_str])
                        .map_err(Into::into)
                })
                .err()
            };
            if let Some(compensation_err) = compensation {
                return Err(MemoliteError::CompensationFailed {
                    operation: e.to_string(),
                    compensation: compensation_err.to_string(),
                });
            }
            return Err(e);
        }

        Ok(id_str)
    }

    /// Retrieves memories relevant to the supplied query.
    ///
    /// Semantic retrieval is not implemented yet -- lands in M4's
    /// `recall_query()`, with `recall()` becoming a thin wrapper around it.
    /// Left as a stub here on purpose: wiring this to types that don't
    /// exist yet (`RecallQuery`, `recall_query()`) would break the Step 0
    /// checkpoint.
    pub async fn recall(&self, query: &str) -> Result<Vec<Memory>> {
        let _ = query;
        todo!("wired to recall_query() in M4")
    }

    /// Fetches a memory by its ID.
    pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

        let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE id = ?1");
        let memory = conn.query_row(&sql, params![id], row_to_memory).optional()?;

        Ok(memory)
    }

    /// Fetches the raw embedding vector stored for a given memory id, if
    /// any.
    pub async fn get_embedding(&self, memory_id: &str) -> Result<Option<Vec<f32>>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

        let row: Option<(Vec<u8>, i64)> = conn
            .query_row(
                "SELECT vector, dimension FROM embeddings WHERE memory_id = ?1",
                params![memory_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        drop(conn);

        let Some((blob, stored_dim)) = row else {
            return Ok(None);
        };

        let vector: Vec<f32> = bincode::deserialize(&blob)
            .map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;

        if vector.len() != stored_dim as usize {
            return Err(MemoliteError::Corruption(format!(
                "embedding for {memory_id} has {} floats but its row says dimension {stored_dim}",
                vector.len()
            )));
        }

        Ok(Some(vector))
    }

    /// Returns the dimension of vectors produced by this engine's embedder.
    pub fn dimension(&self) -> usize {
        self.embedder.lock().expect("embedder mutex poisoned").dimension()
    }

    /// Permanently deletes a memory (hard delete). Deleting a nonexistent
    /// but well-formed ID is not an error. A syntactically invalid ID is
    /// rejected *before* any mutation happens, not after.
    pub async fn forget(&self, id: &str) -> Result<()> {
        let uuid = Uuid::parse_str(id)?; // MemoliteError::InvalidUuid via #[from]; zero side effects on failure

        {
            let conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
            conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        }

        let store = {
            let guard = self
                .vector_store
                .read()
                .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            Arc::clone(&*guard)
        };

        if let Err(e) = store.delete(uuid).await {
            // The SQLite row is already gone. Bring the vector index back
            // in sync with SQLite rather than leaving a stale entry.
            if let Err(reconcile_err) =
                reconcile_vector_index(&self.conn, &store, BackfillPolicy::ReplaceAll).await
            {
                return Err(MemoliteError::CompensationFailed {
                    operation: e.to_string(),
                    compensation: reconcile_err.to_string(),
                });
            }
            return Err(e);
        }

        Ok(())
    }

    /// Removes every expired memory from SQLite and the live vector index.
    /// Returns the number of deleted rows.
    pub async fn purge_expired(&self) -> Result<usize> {
        let now = Utc::now().timestamp();

        let expired_ids: Vec<String> = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

            let mut stmt =
                conn.prepare("SELECT id FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1")?;
            let ids = stmt
                .query_map(params![now], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);

            conn.execute(
                "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
                params![now],
            )?;

            ids
        };

        if expired_ids.is_empty() {
            return Ok(0);
        }

        let store = {
            let guard = self
                .vector_store
                .read()
                .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            Arc::clone(&*guard)
        };

        let mut vector_errors = Vec::new();
        for id in &expired_ids {
            if let Ok(uuid) = Uuid::parse_str(id) {
                if let Err(e) = store.delete(uuid).await {
                    vector_errors.push(e.to_string());
                }
            }
        }

        if !vector_errors.is_empty() {
            if let Err(reconcile_err) =
                reconcile_vector_index(&self.conn, &store, BackfillPolicy::ReplaceAll).await
            {
                return Err(MemoliteError::CompensationFailed {
                    operation: vector_errors.join("; "),
                    compensation: reconcile_err.to_string(),
                });
            }
        }

        Ok(expired_ids.len())
    }
}

/// Reads every memory row and, via a LEFT JOIN, every embedding row that
/// should exist for it. A NULL on the embedding side means a memory row
/// exists with no embedding -- since `store()` always writes both in one
/// SQLite transaction, this is only reachable via external corruption of
/// the file, and is reported as such rather than silently dropped from the
/// index.
///
/// `BackfillPolicy::ExistingOnly` returns immediately, before reading
/// anything: "do not touch the remote store" means zero local work too,
/// not "validate everything and then no-op."
///
/// The read phase (when it runs) is fully synchronous and collects into a
/// `Vec` before any `.await`, so no `MutexGuard` is ever held across an
/// await point.
async fn reconcile_vector_index(
    conn: &Mutex<Connection>,
    store: &Arc<dyn VectorStore>,
    policy: BackfillPolicy,
) -> Result<()> {
    if policy == BackfillPolicy::ExistingOnly {
        return Ok(());
    }

    let entries: Vec<VectorEntry> = {
        let conn = conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector, e.dimension, m.metadata
             FROM memories m LEFT JOIN embeddings e ON e.memory_id = m.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id_str, bytes, stored_dim, metadata_json) = row?;
            let id = Uuid::parse_str(&id_str)?;

            let (Some(bytes), Some(stored_dim)) = (bytes, stored_dim) else {
                return Err(MemoliteError::Corruption(format!(
                    "memory {id} has no matching embedding row"
                )));
            };

            let vector: Vec<f32> = bincode::deserialize(&bytes)
                .map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
            if vector.len() != stored_dim as usize {
                return Err(MemoliteError::Corruption(format!(
                    "stored vector for {id} has dimension {} but its row says {}",
                    vector.len(),
                    stored_dim
                )));
            }

            let metadata: HashMap<String, serde_json::Value> = serde_json::from_str(&metadata_json)?;
            out.push(VectorEntry { id, vector, metadata });
        }
        out
    }; // conn guard dropped here -- before any await below

    match policy {
        BackfillPolicy::ExistingOnly => unreachable!("handled by the early return above"),
        BackfillPolicy::UpsertLocal => {
            for e in entries {
                store.insert(e.id, &e.vector, e.metadata).await?;
            }
            Ok(())
        }
        BackfillPolicy::ReplaceAll => store.replace_all(entries).await,
    }
}

/// Converts a SQLite row from the `memories` table into a [`Memory`].
fn row_to_memory(row: &Row) -> rusqlite::Result<Memory> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| to_sql_conversion_err(0, e))?;

    let content: String = row.get(1)?;

    let type_str: String = row.get(2)?;
    let memory_type = MemoryType::parse_str(&type_str).map_err(|e| to_sql_conversion_err(2, e))?;

    let importance: f32 = row.get(3)?;

    let access_count: i64 = row.get(4)?;
    let access_count = access_count as u32;

    let created_at_ts: i64 = row.get(5)?;
    let created_at = timestamp_to_datetime(created_at_ts, 5)?;

    let last_accessed_ts: i64 = row.get(6)?;
    let last_accessed = timestamp_to_datetime(last_accessed_ts, 6)?;

    let expires_at_ts: Option<i64> = row.get(7)?;
    let expires_at = expires_at_ts.map(|ts| timestamp_to_datetime(ts, 7)).transpose()?;

    let superseded_by_str: Option<String> = row.get(8)?;
    let superseded_by = superseded_by_str
        .map(|s| Uuid::parse_str(&s))
        .transpose()
        .map_err(|e| to_sql_conversion_err(8, e))?;

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
        superseded_by,
        metadata,
    })
}

fn to_sql_conversion_err<E>(col: usize, err: E) -> rusqlite::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(err))
}

fn timestamp_to_datetime(ts: i64, col: usize) -> rusqlite::Result<DateTime<Utc>> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .ok_or_else(|| to_sql_conversion_err(col, MemoliteError::Other(anyhow::anyhow!("timestamp out of range"))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crate::vector_store::VectorHit;

    #[tokio::test]
    async fn store_persists_an_embedding_of_the_right_dimension() {
        let engine = MemoryEngine::open(":memory:").await.expect("engine should open");

        let id = engine
            .store("user prefers dark mode", MemoryType::Semantic, 0.8)
            .await
            .expect("store should succeed");

        let vector = engine
            .get_embedding(&id)
            .await
            .expect("get_embedding should succeed")
            .expect("embedding should exist");

        assert_eq!(vector.len(), engine.dimension());
        assert!(vector.iter().any(|v| *v != 0.0), "embedding should not be all zeros");
    }

    #[tokio::test]
    async fn store_rejects_empty_content() {
        let engine = MemoryEngine::open(":memory:").await.expect("engine should open");
        let result = engine.store("   ", MemoryType::Working, 0.5).await;
        assert!(result.is_err(), "storing empty content should fail");
    }

    #[tokio::test]
    async fn store_rejects_out_of_range_importance() {
        let engine = MemoryEngine::open(":memory:").await.expect("engine should open");
        let result = engine.store("valid content", MemoryType::Working, 1.5).await;
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn store_populates_the_live_vector_index() {
        let engine = MemoryEngine::open(":memory:").await.expect("engine should open");
        let id = engine
            .store("user prefers dark mode", MemoryType::Semantic, 0.8)
            .await
            .expect("store should succeed");

        // Clone the Arc, then drop the read guard, THEN await -- never hold
        // a lock guard across an .await point.
        let store = {
            let guard = engine.vector_store.read().unwrap();
            Arc::clone(&*guard)
        };
        let uuid = Uuid::parse_str(&id).unwrap();
        assert!(store.contains(uuid).await.unwrap());
    }

    #[tokio::test]
    async fn forget_removes_from_the_live_vector_index() {
        let engine = MemoryEngine::open(":memory:").await.expect("engine should open");
        let id = engine
            .store("temporary note", MemoryType::Working, 0.2)
            .await
            .expect("store should succeed");

        engine.forget(&id).await.expect("forget should succeed");

        let store = {
            let guard = engine.vector_store.read().unwrap();
            Arc::clone(&*guard)
        };
        let uuid = Uuid::parse_str(&id).unwrap();
        assert!(!store.contains(uuid).await.unwrap());
    }

    #[tokio::test]
    async fn forget_rejects_malformed_id_without_side_effects() {
        let engine = MemoryEngine::open(":memory:").await.expect("engine should open");
        let id = engine
            .store("should survive", MemoryType::Working, 0.5)
            .await
            .expect("store should succeed");

        let result = engine.forget("not-a-uuid").await;
        assert!(matches!(result, Err(MemoliteError::InvalidUuid(_))));

        // The malformed call must have zero side effects: the real memory
        // is untouched in both SQLite and the vector index.
        assert!(engine.get(&id).await.unwrap().is_some());
        let store = {
            let guard = engine.vector_store.read().unwrap();
            Arc::clone(&*guard)
        };
        let uuid = Uuid::parse_str(&id).unwrap();
        assert!(store.contains(uuid).await.unwrap());
    }

    #[tokio::test]
    async fn forget_on_a_wellformed_but_nonexistent_id_is_a_noop() {
        let engine = MemoryEngine::open(":memory:").await.expect("engine should open");
        let result = engine.forget(&Uuid::new_v4().to_string()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn reopening_reconstructs_the_vector_index() {
        let path = std::env::temp_dir().join(format!("memolite-step0-restart-{}.db", Uuid::new_v4()));

        let id = {
            let engine = MemoryEngine::open(&path).await.expect("first open should succeed");
            engine
                .store("user prefers dark mode", MemoryType::Semantic, 0.8)
                .await
                .expect("store should succeed")
            // engine (and its in-RAM vector index) dropped here
        };

        let engine = MemoryEngine::open(&path).await.expect("second open should succeed");

        let store = {
            let guard = engine.vector_store.read().unwrap();
            Arc::clone(&*guard)
        };
        let uuid = Uuid::parse_str(&id).unwrap();
        assert!(
            store.contains(uuid).await.unwrap(),
            "reopening must repopulate the vector index from SQLite"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn corrupt_memory_row_with_no_embedding_fails_loudly_on_reopen() {
        let path = std::env::temp_dir().join(format!("memolite-step0-corrupt-{}.db", Uuid::new_v4()));

        let id = {
            let engine = MemoryEngine::open(&path).await.expect("first open should succeed");
            engine
                .store("will be corrupted", MemoryType::Working, 0.5)
                .await
                .expect("store should succeed")
        };

        {
            // Simulate corruption: delete only the embeddings row, leaving
            // an orphan memories row. Deleting a child row never trips the
            // FK constraint (that only guards deleting/violating a parent).
            let conn = Connection::open(&path).unwrap();
            conn.execute("DELETE FROM embeddings WHERE memory_id = ?1", params![id])
                .unwrap();
        }

        let result = MemoryEngine::open(&path).await;
        assert!(
            matches!(result, Err(MemoliteError::Corruption(_))),
            "a memory row with no embedding must surface as Corruption, not be silently skipped"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A `VectorStore` test double whose `insert` always fails, used to
    /// prove `store()`'s compensation path actually deletes the orphaned
    /// SQLite rows rather than leaving them behind.
    struct AlwaysFailsInsert;

    #[async_trait]
    impl VectorStore for AlwaysFailsInsert {
        async fn insert(&self, _id: Uuid, _vector: &[f32], _metadata: HashMap<String, serde_json::Value>) -> Result<()> {
            Err(MemoliteError::VectorStore("simulated failure".into()))
        }
        async fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<VectorHit>> {
            Ok(vec![])
        }
        async fn delete(&self, _id: Uuid) -> Result<()> {
            Ok(())
        }
        async fn contains(&self, _id: Uuid) -> Result<bool> {
            Ok(false)
        }
        async fn replace_all(&self, _entries: Vec<VectorEntry>) -> Result<()> {
            Ok(())
        }
        fn dimension(&self) -> usize {
            384 // matches Embedder::dimension()
        }
    }

    #[tokio::test]
    async fn store_rolls_back_orphaned_sqlite_rows_if_the_vector_store_insert_fails() {
        let path = std::env::temp_dir().join(format!("memolite-step0-compensate-{}.db", Uuid::new_v4()));

        let engine = MemoryEngine::open_with_store_internal(
            &path,
            Some(Arc::new(AlwaysFailsInsert) as Arc<dyn VectorStore>),
            BackfillPolicy::ExistingOnly,
        )
        .await
        .expect("engine should open even though its store will fail later");

        let result = engine.store("this insert will fail downstream", MemoryType::Working, 0.5).await;
        assert!(matches!(result, Err(MemoliteError::CompensationFailed { .. }) | Err(MemoliteError::VectorStore(_))));

        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "a failed vector-store insert must not leave an orphaned memories row");

        let _ = std::fs::remove_file(&path);
    }
}