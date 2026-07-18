use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
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
///
/// Two conventions hold everywhere in this file, not just locally within
/// one method -- written down here once so they can't drift apart:
///
/// - **Expiration boundary:** `expires_at <= now` means "expired,"
///   everywhere -- recall filtering, `purge_expired()` eligibility, and
///   `reconcile_vector_index()` all use this exact boundary. Never `<` in
///   one place and `<=` in another.
/// - **Compensation policy:** when a SQLite write succeeds but the paired
///   vector-store write/delete fails, the engine attempts best-effort
///   reconciliation (via `reconcile_vector_index`) but *always* surfaces
///   the *original* error as `Err`. A successful reconciliation never
///   silently upgrades a real failure into `Ok` -- applies uniformly to
///   `store()`, `forget()`, and `purge_expired()`.
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
    pub async fn store(
        &self,
        content: &str,
        memory_type: MemoryType,
        importance: f32,
    ) -> Result<String> {
        if content.trim().is_empty() {
            return Err(MemoliteError::InvalidArgument(
                "content must not be empty".into(),
            ));
        }
        if !(0.0..=1.0).contains(&importance) {
            return Err(MemoliteError::InvalidArgument(
                "importance must be in [0.0, 1.0]".into(),
            ));
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

    /// Retrieves memories relevant to the supplied query text using the
    /// default `RecallQuery` (limit `DEFAULT_RECALL_LIMIT`, no filters,
    /// superseded/expired excluded).
    ///
    /// As of M4 this is a thin wrapper around [`Self::recall_query`] --
    /// there is exactly one bump-and-refetch code path in the crate,
    /// defined once in `recall_query()`, not two that can drift apart.
    pub async fn recall(&self, query: &str) -> Result<Vec<Memory>> {
        let result = self
            .recall_query(crate::recall::RecallQuery::new(query))
            .await?;
        Ok(result.items.into_iter().map(|i| i.memory).collect())
    }

    /// The full recall implementation: embed the query, search the live
    /// vector index for nearest neighbors, batch-load and filter the
    /// matching SQLite rows, score and rank them, truncate to
    /// `query.limit`, then bump access stats for exactly that truncated set
    /// and batch-refetch so the returned `Memory` values reflect the bump
    /// this very call made -- never pre-increment values.
    ///
    /// A candidate the vector index still has but that SQLite no longer has
    /// (e.g. a partially-failed concurrent `forget()`) is silently dropped
    /// from the result, never an error -- same convention as M3's
    /// `recall()`.
    ///
    /// `confidence_weight` is stubbed at `1.0` here; M6 replaces that one
    /// line with `memory.confidence.weight()` once the `confidence` column
    /// and field exist. Nothing else in this function changes at that
    /// point -- this is the single bump-and-refetch implementation the
    /// crate will ever have, edited in place from here on, never
    /// duplicated.
    pub async fn recall_query(
        &self,
        query: crate::recall::RecallQuery,
    ) -> Result<crate::recall::RecallResult> {
        use crate::recall::{RecallItem, RecallResult};

        if query.query_text.trim().is_empty() {
            return Err(MemoliteError::InvalidArgument(
                "query_text must not be empty".into(),
            ));
        }
        if query.limit == 0 {
            return Err(MemoliteError::InvalidArgument(
                "limit must be > 0".into(),
            ));
        }
        if !query.min_importance.is_finite() {
            return Err(MemoliteError::InvalidArgument(
                "min_importance must be finite".into(),
            ));
        }

        // Embed the query. Lock the embedder only for the synchronous
        // embed() call and drop the guard before anything else -- same
        // discipline as store()'s use of the embedder.
        let query_vector = {
            let mut embedder = self
                .embedder
                .lock()
                .map_err(|_| MemoliteError::Internal("embedder mutex poisoned".into()))?;
            embedder.embed(&query.query_text)?
        };

        // Clone the vector-store handle, drop the read guard, THEN await.
        let store = {
            let guard = self
                .vector_store
                .read()
                .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            Arc::clone(&*guard)
        };

        let pool_size = crate::recall::candidate_pool_size(query.limit);
        let hits = store.search(&query_vector, pool_size).await?;

        if hits.is_empty() {
            return Ok(RecallResult { items: Vec::new() });
        }

        let hit_ids: Vec<Uuid> = hits.iter().map(|h| h.id).collect();
        let now = Utc::now();

        // One unfiltered batch query for every candidate; RecallQuery's
        // filters (type, importance, metadata, superseded/expired
        // inclusion) are all applied in Rust below, uniformly, rather than
        // being split across a fixed SQL WHERE clause and ad-hoc Rust
        // checks.
        let by_id = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
            fetch_memories_by_ids(&conn, &hit_ids)?
        }; // conn guard dropped here, before anything below

        let mut scored: Vec<RecallItem> = Vec::with_capacity(hits.len());
        for hit in &hits {
            // A candidate the vector index has but SQLite no longer has
            // (e.g. a partially-failed concurrent forget()) is silently
            // dropped, never an error.
            let Some(memory) = by_id.get(&hit.id) else {
                continue;
            };

            if memory.importance < query.min_importance {
                continue;
            }
            if let Some(types) = &query.memory_types {
                if !types.contains(&memory.memory_type) {
                    continue;
                }
            }
            if memory.superseded_by.is_some() && !query.include_superseded {
                continue;
            }
            // Same expiration boundary as everywhere else in this file:
            // expires_at <= now is expired.
            if memory.expires_at.map(|e| e <= now).unwrap_or(false) && !query.include_expired {
                continue;
            }
            if !query
                .metadata_equals
                .iter()
                .all(|(k, v)| memory.metadata.get(k) == Some(v))
            {
                continue;
            }

            let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
            let recency = crate::ranking::recency_factor(days_since_access, memory.memory_type);
            let reinforcement = crate::ranking::reinforcement_factor(memory.access_count);
            let confidence_weight = 1.0_f32; // M6 replaces this one line
            let score = crate::ranking::final_score(
                hit.score,
                memory.importance,
                recency,
                reinforcement,
                confidence_weight,
            );

            scored.push(RecallItem {
                memory: memory.clone(),
                similarity: hit.score,
                score,
            });
        }

        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.memory.id.cmp(&b.memory.id))
        });
        scored.truncate(query.limit);

        if scored.is_empty() {
            return Ok(RecallResult { items: Vec::new() });
        }

        // Bump access stats for exactly the truncated set, in one
        // transaction -- same batching pattern M3's recall() used, not a
        // per-item loop.
        let bumped_ids: Vec<Uuid> = scored.iter().map(|i| i.memory.id).collect();
        {
            let mut conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
            let tx = conn.transaction()?;
            let now_ts = now.timestamp();

            let id_placeholders = (0..bumped_ids.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 \
                 WHERE id IN ({id_placeholders})"
            );

            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now_ts)];
            for id in &bumped_ids {
                params_vec.push(Box::new(id.to_string()));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|p| p.as_ref()).collect();
            tx.execute(&sql, params_refs.as_slice())?;
            tx.commit()?;
        } // conn guard dropped here, before the refetch below

        // Refetch so returned Memory values reflect the bump this call just
        // made. A row that vanished between bump and refetch (concurrent
        // forget()) is simply left with its pre-bump snapshot rather than
        // dropped -- the item already survived truncation and was
        // legitimately part of this call's result set.
        let refetched = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
            fetch_memories_by_ids(&conn, &bumped_ids)?
        };
        for item in &mut scored {
            if let Some(m) = refetched.get(&item.memory.id) {
                item.memory = m.clone();
            }
        }

        Ok(RecallResult { items: scored })
    }

    /// Fetches a memory by its ID.
    ///
    /// The id is parsed as a UUID *before* touching SQLite -- a malformed
    /// id is rejected with zero database work, same discipline as
    /// `forget()`.
    pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
        let uuid = Uuid::parse_str(id)?; // MemoliteError::InvalidUuid via #[from]

        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

        let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE id = ?1");
        let memory = conn
            .query_row(&sql, params![uuid.to_string()], row_to_memory)
            .optional()?;

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
        self.embedder
            .lock()
            .expect("embedder mutex poisoned")
            .dimension()
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
    ///
    /// Returns the number of deleted rows.
    ///
    /// Boundary convention: `expires_at <= now` counts as expired, same as
    /// every other expiration check in this file (recall filtering,
    /// reconciliation).
    ///
    /// Error convention: identical to `forget()` -- if a vector-store
    /// delete fails, this makes a best-effort attempt to bring the index
    /// back in sync via `reconcile_vector_index`, but the *original*
    /// vector-store error is always what gets returned (never silently
    /// upgraded to `Ok` just because reconciliation happened to succeed).
    pub async fn purge_expired(&self) -> Result<usize> {
        let now = Utc::now().timestamp();

        let deleted_ids: Vec<String> = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

            let mut stmt = conn.prepare(
                "DELETE FROM memories
                 WHERE expires_at IS NOT NULL AND expires_at <= ?1
                 RETURNING id",
            )?;

            stmt.query_map(params![now], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        if deleted_ids.is_empty() {
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

        for id in &deleted_ids {
            if let Ok(uuid) = Uuid::parse_str(id) {
                if let Err(error) = store.delete(uuid).await {
                    vector_errors.push(error.to_string());
                }
            }
        }

        if !vector_errors.is_empty() {
            let combined = vector_errors.join("; ");
            if let Err(reconcile_error) =
                reconcile_vector_index(&self.conn, &store, BackfillPolicy::ReplaceAll).await
            {
                return Err(MemoliteError::CompensationFailed {
                    operation: combined,
                    compensation: reconcile_error.to_string(),
                });
            }
            // Reconciliation repaired the index, but per the compensation
            // convention the original failure is still surfaced -- a
            // successful best-effort repair never turns a real error into
            // `Ok`.
            return Err(MemoliteError::VectorStore(combined));
        }

        Ok(deleted_ids.len())
    }
}

/// Batch-loads every memory in `ids` from SQLite in one query, keyed by id,
/// with **no** active/expired/superseded filtering applied -- the caller
/// decides what "eligible" means for its own use case.
///
/// This is the general-purpose fetch `recall_query()` needs: unlike M3's
/// `fetch_active_memories` (replaced as of M4), `RecallQuery::include_superseded`
/// / `include_expired` mean the filter itself is a per-call decision made in
/// Rust alongside the rest of `RecallQuery`'s filters, not a fixed SQL WHERE
/// clause baked into the fetch. An id present in `ids` but absent from the
/// returned map simply doesn't exist in SQLite -- never an error; the caller
/// decides what "missing" means for its own step.
fn fetch_memories_by_ids(conn: &Connection, ids: &[Uuid]) -> Result<HashMap<Uuid, Memory>> {
    let mut result = HashMap::with_capacity(ids.len());
    if ids.is_empty() {
        return Ok(result);
    }

    let id_placeholders = (0..ids.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE id IN ({id_placeholders})");

    let params_vec: Vec<Box<dyn rusqlite::ToSql>> = ids
        .iter()
        .map(|id| Box::new(id.to_string()) as Box<dyn rusqlite::ToSql>)
        .collect();
    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), row_to_memory)?;
    for row in rows {
        let memory = row?;
        result.insert(memory.id, memory);
    }

    Ok(result)
}

/// Reads every *active* memory row -- not superseded, not expired -- and,
/// via a LEFT JOIN, every embedding row that should exist for it. A NULL on
/// the embedding side means a memory row exists with no embedding -- since
/// `store()` always writes both in one SQLite transaction, this is only
/// reachable via external corruption of the file, and is reported as such
/// rather than silently dropped from the index. The active-only filter
/// narrows *which* rows are considered; it never changes that corruption
/// detection outcome for whatever rows do match.
///
/// Expiration boundary: `expires_at <= now` is expired, same convention as
/// `purge_expired()` and recall filtering.
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

    let now = Utc::now().timestamp();

    let entries: Vec<VectorEntry> = {
        let conn = conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector, e.dimension, m.metadata
             FROM memories m LEFT JOIN embeddings e ON e.memory_id = m.id
             WHERE m.superseded_by IS NULL
               AND (m.expires_at IS NULL OR m.expires_at > ?1)",
        )?;
        let rows = stmt.query_map(params![now], |row| {
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

            let metadata: HashMap<String, serde_json::Value> =
                serde_json::from_str(&metadata_json)?;
            out.push(VectorEntry {
                id,
                vector,
                metadata,
            });
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
    let expires_at = expires_at_ts
        .map(|ts| timestamp_to_datetime(ts, 7))
        .transpose()?;

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
    Utc.timestamp_opt(ts, 0).single().ok_or_else(|| {
        to_sql_conversion_err(
            col,
            MemoliteError::Other(anyhow::anyhow!("timestamp out of range")),
        )
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::RecallQuery;
    use crate::vector_store::VectorHit;
    use async_trait::async_trait;

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
            .expect("get_embedding should succeed")
            .expect("embedding should exist");

        assert_eq!(vector.len(), engine.dimension());
        assert!(
            vector.iter().any(|v| *v != 0.0),
            "embedding should not be all zeros"
        );
    }

    #[tokio::test]
    async fn store_rejects_empty_content() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let result = engine.store("   ", MemoryType::Working, 0.5).await;
        assert!(result.is_err(), "storing empty content should fail");
    }

    #[tokio::test]
    async fn store_rejects_out_of_range_importance() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let result = engine
            .store("valid content", MemoryType::Working, 1.5)
            .await;
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn store_populates_the_live_vector_index() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
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
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
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
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
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
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let result = engine.forget(&Uuid::new_v4().to_string()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn reopening_reconstructs_the_vector_index() {
        let path =
            std::env::temp_dir().join(format!("memolite-step0-restart-{}.db", Uuid::new_v4()));

        let id = {
            let engine = MemoryEngine::open(&path)
                .await
                .expect("first open should succeed");
            engine
                .store("user prefers dark mode", MemoryType::Semantic, 0.8)
                .await
                .expect("store should succeed")
            // engine (and its in-RAM vector index) dropped here
        };

        let engine = MemoryEngine::open(&path)
            .await
            .expect("second open should succeed");

        let store = {
            let guard = engine.vector_store.read().unwrap();
            Arc::clone(&*guard)
        };
        let uuid = Uuid::parse_str(&id).unwrap();
        assert!(
            store.contains(uuid).await.unwrap(),
            "reopening must repopulate the vector index from SQLite"
        );

        // Explicitly drop engine before removing file
        drop(engine);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }

    #[tokio::test]
    async fn corrupt_memory_row_with_no_embedding_fails_loudly_on_reopen() {
        let path =
            std::env::temp_dir().join(format!("memolite-step0-corrupt-{}.db", Uuid::new_v4()));

        let id = {
            let engine = MemoryEngine::open(&path)
                .await
                .expect("first open should succeed");
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
            // Explicitly drop the raw connection before reopening
            drop(conn);
        }

        let result = MemoryEngine::open(&path).await;
        assert!(
            matches!(result, Err(MemoliteError::Corruption(_))),
            "a memory row with no embedding must surface as Corruption, not be silently skipped"
        );

        // If open succeeded, we'd have an engine to drop, but it failed so we just
        // need to clean up the file. The failed engine's connection was dropped
        // when it went out of scope.
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }

    /// A `VectorStore` test double whose `insert` always fails, used to
    /// prove `store()`'s compensation path actually deletes the orphaned
    /// SQLite rows rather than leaving them behind.
    struct AlwaysFailsInsert;

    #[async_trait]
    impl VectorStore for AlwaysFailsInsert {
        async fn insert(
            &self,
            _id: Uuid,
            _vector: &[f32],
            _metadata: HashMap<String, serde_json::Value>,
        ) -> Result<()> {
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
        let path =
            std::env::temp_dir().join(format!("memolite-step0-compensate-{}.db", Uuid::new_v4()));

        let engine = MemoryEngine::open_with_store_internal(
            &path,
            Some(Arc::new(AlwaysFailsInsert) as Arc<dyn VectorStore>),
            BackfillPolicy::ExistingOnly,
        )
        .await
        .expect("engine should open even though its store will fail later");

        let result = engine
            .store("this insert will fail downstream", MemoryType::Working, 0.5)
            .await;
        assert!(matches!(
            result,
            Err(MemoliteError::CompensationFailed { .. }) | Err(MemoliteError::VectorStore(_))
        ));

        // Explicitly drop engine before opening a raw connection to avoid
        // file locking issues on Windows
        drop(engine);

        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "a failed vector-store insert must not leave an orphaned memories row"
        );

        // Explicitly drop the connection before removing the file
        drop(conn);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }
 /// A `VectorStore` test double whose `delete` always fails, used to
    /// prove `forget()`'s error-surfacing convention (Step 17/41): the
    /// *original* vector-store error is returned even after a successful
    /// best-effort `reconcile_vector_index` repair.
    struct AlwaysFailsDelete;
 
    #[async_trait]
    impl VectorStore for AlwaysFailsDelete {
        async fn insert(
            &self,
            _id: Uuid,
            _vector: &[f32],
            _metadata: HashMap<String, serde_json::Value>,
        ) -> Result<()> {
            Ok(())
        }
        async fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<VectorHit>> {
            Ok(vec![])
        }
        async fn delete(&self, _id: Uuid) -> Result<()> {
            Err(MemoliteError::VectorStore("simulated delete failure".into()))
        }
        async fn contains(&self, _id: Uuid) -> Result<bool> {
            Ok(false)
        }
        async fn replace_all(&self, _entries: Vec<VectorEntry>) -> Result<()> {
            // Reconciliation itself succeeds trivially here, so the test
            // below proves forget() still surfaces the *original* delete
            // error rather than upgrading a successful repair into `Ok`.
            Ok(())
        }
        fn dimension(&self) -> usize {
            384 // matches Embedder::dimension()
        }
    }
 
    #[tokio::test]
    async fn forget_surfaces_the_original_error_when_vector_delete_fails() {
        let path = std::env::temp_dir()
            .join(format!("memolite-step0-forget-compensate-{}.db", Uuid::new_v4()));
 
        let engine = MemoryEngine::open_with_store_internal(
            &path,
            Some(Arc::new(AlwaysFailsDelete) as Arc<dyn VectorStore>),
            BackfillPolicy::ExistingOnly,
        )
        .await
        .expect("engine should open even though its store's delete will fail later");
 
        let id = engine
            .store("will fail to delete from the vector store", MemoryType::Working, 0.5)
            .await
            .expect("store should succeed");
 
        let result = engine.forget(&id).await;
        assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
 
        // forget() deletes from SQLite first, regardless of what happens to
        // the vector store afterward -- the SQLite row must be gone even
        // though the overall call returned Err.
        assert!(engine.get(&id).await.unwrap().is_none());
 
        drop(engine);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }
 
    #[tokio::test]
    async fn open_with_store_rejects_a_dimension_mismatched_backend() {
        let path = std::env::temp_dir()
            .join(format!("memolite-step0-dim-mismatch-{}.db", Uuid::new_v4()));
 
        // The real embedder produces 384-dimensional vectors; this store is
        // deliberately the wrong size.
        let wrong_dim_store = Arc::new(InMemoryVectorStore::new(7)) as Arc<dyn VectorStore>;
 
        let result = MemoryEngine::open_with_store_internal(
            &path,
            Some(wrong_dim_store),
            BackfillPolicy::ExistingOnly,
        )
        .await;
 
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));
 
        // open() must reject the mismatch before doing any reconciliation
        // work, so no database file contents should have been touched in a
        // way that matters -- just clean up whatever SQLite created.
        let _ = std::fs::remove_file(&path);
    }

    // ---- M4: recall_query() tests ----

    #[tokio::test]
    async fn recall_query_excludes_a_filtered_out_memory_type() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        engine
            .store("the user's favorite editor is Zed", MemoryType::Semantic, 0.8)
            .await
            .unwrap();
        engine
            .store("debugged the login flow yesterday", MemoryType::Episodic, 0.8)
            .await
            .unwrap();

        let result = engine
            .recall_query(
                RecallQuery::new("editor preference").memory_types(vec![MemoryType::Episodic]),
            )
            .await
            .unwrap();

        assert!(result
            .items
            .iter()
            .all(|i| i.memory.memory_type == MemoryType::Episodic));
    }

    #[tokio::test]
    async fn recall_query_limit_zero_is_an_error() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let result = engine
            .recall_query(RecallQuery::new("anything").limit(0))
            .await;
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn recall_query_nan_min_importance_is_an_error() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let result = engine
            .recall_query(RecallQuery::new("anything").min_importance(f32::NAN))
            .await;
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn recall_query_bumps_access_stats_before_returning() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        engine
            .store("a fact worth recalling", MemoryType::Semantic, 0.8)
            .await
            .unwrap();

        let result = engine
            .recall_query(RecallQuery::new("a fact worth recalling"))
            .await
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].memory.access_count, 1,
            "the returned Memory must already reflect this call's own bump"
        );
    }

    #[tokio::test]
    async fn recall_and_recall_query_agree_on_a_plain_query() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        engine
            .store("plain fact one", MemoryType::Semantic, 0.7)
            .await
            .unwrap();
        engine
            .store("plain fact two", MemoryType::Semantic, 0.6)
            .await
            .unwrap();

        let via_recall = engine.recall("plain fact").await.unwrap();
        let via_query = engine
            .recall_query(RecallQuery::new("plain fact"))
            .await
            .unwrap();

        let query_ids: Vec<_> = via_query.items.iter().map(|i| i.memory.id).collect();
        let recall_ids: Vec<_> = via_recall.iter().map(|m| m.id).collect();
        assert_eq!(recall_ids, query_ids);
    }

    #[tokio::test]
    async fn recall_query_metadata_equals_filters_candidates() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let id = engine
            .store("tagged memory", MemoryType::Semantic, 0.8)
            .await
            .unwrap();

        // metadata is empty ("{}") on every M3/M4 store() call today, so an
        // equals-filter on a key that's never set must exclude everything.
        let result = engine
            .recall_query(
                RecallQuery::new("tagged memory")
                    .metadata_equals("project", serde_json::json!("memolite")),
            )
            .await
            .unwrap();

        assert!(result.items.iter().all(|i| i.memory.id != Uuid::parse_str(&id).unwrap()));
    }

    #[tokio::test]
    async fn recall_query_excludes_expired_by_default_and_includes_when_asked() {
        let path = std::env::temp_dir()
            .join(format!("memolite-m4-expired-{}.db", Uuid::new_v4()));
        let engine = MemoryEngine::open(&path).await.expect("engine should open");

        let id = engine
            .store("soon to expire", MemoryType::Working, 0.9)
            .await
            .unwrap();

        // Backdate expires_at into the past via raw test-only SQL -- same
        // trick used by the corruption test above.
        {
            let conn = engine.conn.lock().unwrap();
            let past = (Utc::now() - chrono::Duration::days(1)).timestamp();
            conn.execute(
                "UPDATE memories SET expires_at = ?1 WHERE id = ?2",
                params![past, id],
            )
            .unwrap();
        }

        let default_result = engine
            .recall_query(RecallQuery::new("soon to expire"))
            .await
            .unwrap();
        assert!(default_result.items.is_empty());

        let included_result = engine
            .recall_query(RecallQuery::new("soon to expire").include_expired(true))
            .await
            .unwrap();
        assert!(included_result
            .items
            .iter()
            .any(|i| i.memory.id == Uuid::parse_str(&id).unwrap()));

        drop(engine);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }
}