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
use crate::requests::{ExpiryPolicy, MemoryUpdate, StoreRequest}; // M5
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
/// Conventions that hold everywhere in this file, not just locally within
/// one method -- written down here once so they can't drift apart:
///
/// - **Expiration boundary:** `expires_at <= now` means "expired,"
///   everywhere -- recall filtering, `purge_expired()` eligibility,
///   `update()`'s expired-revival guard, and `reconcile_vector_index()`
///   all use this exact boundary. Never `<` in one place and `<=` in
///   another.
/// - **Compensation policy:** when a SQLite write succeeds but a paired
///   follow-up write/delete fails, the engine attempts best-effort
///   reconciliation but *always* surfaces the *original* error as `Err`.
///   A successful reconciliation never silently upgrades a real failure
///   into `Ok` -- applies uniformly to `store_with_options_id()`,
///   `forget()`, `update()`, and `purge_expired()`.
/// - **Single write path (M5):** `store_with_options_id()` is the *only*
///   function that ever inserts a `memories` row and its matching
///   `embeddings` row. `store()`, `store_with_options()`, and `update()`
///   all route through it. This is what makes "a memory row with no
///   embedding row is corruption, never a normal state" a sound
///   invariant for `reconcile_vector_index()` to rely on.
/// - **Supersession is race-safe (M5):** `mark_superseded()`'s `UPDATE`
///   only succeeds against a row that is *currently* un-superseded
///   (`superseded_by IS NULL`). This closes two related gaps that a
///   naive `UPDATE ... WHERE id = ?` would leave open: (1) calling
///   `update()` twice on the same already-superseded memory would
///   otherwise silently overwrite which memory is "the current one" a
///   second time, corrupting the supersession chain; (2) two concurrent
///   `update()` calls racing on the same source id would otherwise let
///   the loser silently overwrite the winner's link with no error,
///   leaving the loser's brand-new memory fully persisted but orphaned
///   from the chain. `update()` also rejects an already-superseded
///   source memory up front (before doing any embedding/storage work) so
///   the common, non-racy case fails fast and cheaply; `mark_superseded`'s
///   conditional `UPDATE` remains the authoritative guard against the
///   genuinely concurrent case, since only SQLite can adjudicate that
///   atomically.
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

    /// Stores a new memory using the crate's original simple signature.
    /// (M5) Thin wrapper around `store_with_options` using
    /// `ExpiryPolicy::TypeDefault` and empty metadata -- the exact
    /// defaults `store()` always used before M5. Kept for source
    /// compatibility; new code that wants expiry/metadata control should
    /// call `store_with_options` directly.
    pub async fn store(
        &self,
        content: &str,
        memory_type: MemoryType,
        importance: f32,
    ) -> Result<String> {
        self.store_with_options(StoreRequest::new(content, memory_type, importance))
            .await
    }

    /// (M5) Stores a new memory described by `request` -- with explicit
    /// control over expiry and metadata -- generates its embedding, and
    /// persists both, then inserts into the live vector index too.
    /// Returns the new memory's id as a string.
    pub async fn store_with_options(&self, request: StoreRequest) -> Result<String> {
        self.store_with_options_id(request)
            .await
            .map(|id| id.to_string())
    }

    /// (M5) The single writer of a memory+embedding pair, always in one
    /// SQLite transaction. `store()`, `store_with_options()`, and
    /// `update()` all route through this one function -- there is exactly
    /// one place a memory row and its embedding row are ever created
    /// together, which is what makes `reconcile_vector_index`'s "missing
    /// embedding = corruption" rule sound.
    async fn store_with_options_id(&self, request: StoreRequest) -> Result<Uuid> {
        if request.content.trim().is_empty() {
            return Err(MemoliteError::InvalidArgument(
                "content must not be empty".into(),
            ));
        }
        if !(0.0..=1.0).contains(&request.importance) {
            return Err(MemoliteError::InvalidArgument(
                "importance must be in [0.0, 1.0]".into(),
            ));
        }
        if let ExpiryPolicy::Custom(d) = request.expiry {
            if d <= chrono::Duration::zero() {
                return Err(MemoliteError::InvalidArgument(
                    "Custom expiry duration must be positive".into(),
                ));
            }
        }

        let id = Uuid::new_v4();
        let id_str = id.to_string();
        let created_at = Utc::now();
        let expires_at: Option<DateTime<Utc>> = match request.expiry {
            ExpiryPolicy::Never => None,
            ExpiryPolicy::Custom(d) => Some(created_at + d),
            ExpiryPolicy::TypeDefault => Some(created_at + request.memory_type.default_ttl()),
        };
        // (M5) Actually persist the request's metadata instead of the
        // hardcoded "{}" the pre-M5 store() used.
        let metadata_json = serde_json::to_string(&request.metadata)?;

        // Embed *before* touching SQLite: model work is slower and can
        // fail, and there's no reason to hold a DB lock across it.
        let vector = {
            let mut embedder = self
                .embedder
                .lock()
                .map_err(|_| MemoliteError::Internal("embedder mutex poisoned".into()))?;
            embedder.embed(&request.content)?
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
                    request.content,
                    request.memory_type.as_str(),
                    request.importance,
                    created_at.timestamp(),
                    expires_at.map(|e| e.timestamp()),
                    metadata_json,
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

        // (M5) Insert the request's actual metadata into the vector store
        // too, instead of an empty HashMap, so the two copies stay
        // consistent.
        if let Err(e) = store.insert(id, &vector, request.metadata.clone()).await {
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

        Ok(id)
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
            return Err(MemoliteError::InvalidArgument("limit must be > 0".into()));
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

    /// (M5) Applies a partial update to an existing memory. This never
    /// mutates the existing row in place: it builds a merged `StoreRequest`
    /// from `old` + `update`, creates a brand-new memory for it via
    /// `store_with_options_id` (the single write path), then marks the
    /// original row's `superseded_by` to point at the new one. Returns the
    /// new memory's id as a string.
    ///
    /// Two guards keep the supersession chain from ever getting corrupted:
    ///
    /// - **Expired-revival guard:** reviving an expired memory requires the
    ///   caller to explicitly supply `new_expiry` -- an update that leaves
    ///   expiry untouched on an already-expired memory is rejected, so a
    ///   memory can never silently come back to life through an unrelated
    ///   field edit.
    /// - **Already-superseded guard:** updating a memory that has already
    ///   been superseded is rejected up front, before any embedding or
    ///   storage work happens. Without this, a second `update()` call
    ///   against the same original id would silently overwrite which
    ///   memory is "the current one," breaking the one-hop chain. The
    ///   *concurrent* version of this same problem (two `update()` calls
    ///   racing on the same id) is closed atomically inside
    ///   `mark_superseded()` itself, since only SQLite can adjudicate that
    ///   race correctly -- this up-front check just makes the common,
    ///   non-racy mistake fail fast and cheaply.
    pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<String> {
        let uuid = Uuid::parse_str(id)?; // MemoliteError::InvalidUuid via #[from]
        let old = self
            .get(id)
            .await?
            .ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;

        // Already-superseded guard (fixes the M5 review finding): reject
        // before doing any embedding/storage work rather than silently
        // re-pointing an already-resolved supersession link.
        if old.superseded_by.is_some() {
            return Err(MemoliteError::InvalidArgument(format!(
                "memory {id} has already been superseded and cannot be updated directly; \
                 update the memory it was superseded by instead"
            )));
        }

        // Snapshot `now` ONCE and reuse it for both the expired check and
        // the remaining-TTL calculation below -- using two separate
        // `Utc::now()` calls here would let embedding latency between them
        // flip which branch is "correct" for a memory expiring right now.
        let now = Utc::now();
        // Same expiration boundary as everywhere else in this file:
        // expires_at <= now is expired.
        let is_expired = old.expires_at.map(|e| e <= now).unwrap_or(false);
        if is_expired && update.new_expiry.is_none() {
            return Err(MemoliteError::InvalidArgument(
                "memory is expired; supply new_expiry explicitly to revive it".into(),
            ));
        }

        let mut request = StoreRequest::new(
            &update.new_content.unwrap_or_else(|| old.content.clone()),
            update.new_memory_type.unwrap_or(old.memory_type),
            update.new_importance.unwrap_or(old.importance),
        );
        // Preserve the old memory's remaining TTL by default (or "never
        // expire" if that's what it had); only overridden when the caller
        // supplies new_expiry explicitly. The `None` + not-expired-but-no-
        // override case below is reachable (e.g. no expires_at at all), so
        // this match covers both "had no expiry" and "still has time left".
        request.expiry = update.new_expiry.unwrap_or(match old.expires_at {
            None => ExpiryPolicy::Never,
            Some(old_expires_at) => ExpiryPolicy::Custom(old_expires_at.signed_duration_since(now)),
        });
        request.metadata = update.new_metadata.unwrap_or_else(|| old.metadata.clone());

        let new_uuid = self.store_with_options_id(request).await?;

        if let Err(e) = self.mark_superseded(&uuid, &new_uuid.to_string()) {
            // The new memory was already committed to both SQLite and the
            // vector index by store_with_options_id -- if linking it back
            // to the old one fails, roll the new memory back rather than
            // leave an unlinked duplicate behind. Same compensation shape
            // as store_with_options_id's own insert-failure path.
            let del_err = {
                let conn = self
                    .conn
                    .lock()
                    .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
                conn.and_then(|c| {
                    c.execute(
                        "DELETE FROM memories WHERE id = ?1",
                        params![new_uuid.to_string()],
                    )
                    .map_err(Into::into)
                })
                .err()
            };
            let store = {
                let guard = self
                    .vector_store
                    .read()
                    .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
                Arc::clone(&*guard)
            };
            let vec_err = store.delete(new_uuid).await.err();

            if del_err.is_some() || vec_err.is_some() {
                return Err(MemoliteError::CompensationFailed {
                    operation: e.to_string(),
                    compensation: format!("{:?} / {:?}", del_err, vec_err),
                });
            }
            return Err(e);
        }

        Ok(new_uuid.to_string())
    }

    /// (M5) Links `old_id` to `new_id` via `superseded_by`.
    ///
    /// The `UPDATE` is conditioned on `superseded_by IS NULL` so this is
    /// safe against races: if two callers both pass `update()`'s up-front
    /// "already superseded" check (because neither had committed yet) and
    /// then race here, exactly one `UPDATE` succeeds -- the other affects
    /// zero rows and gets a clear error back, rather than silently
    /// overwriting the winner's link.
    ///
    /// `affected == 0` is disambiguated into two distinct errors so a
    /// caller (or test) can tell them apart:
    /// - `Err(NotFound)` if `old_id` doesn't exist at all (e.g. a
    ///   concurrent `forget()` raced this `update()` call).
    /// - `Err(InvalidArgument)` if `old_id` exists but was already
    ///   superseded by the time this `UPDATE` ran (the race case above).
    fn mark_superseded(&self, old_id: &Uuid, new_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

        let affected = conn.execute(
            "UPDATE memories SET superseded_by = ?1 WHERE id = ?2 AND superseded_by IS NULL",
            params![new_id, old_id.to_string()],
        )?;

        if affected == 0 {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                params![old_id.to_string()],
                |r| r.get(0),
            )?;

            if exists {
                return Err(MemoliteError::InvalidArgument(format!(
                    "memory {old_id} was already superseded by another update (concurrent update race)"
                )));
            }
            return Err(MemoliteError::NotFound(old_id.to_string()));
        }

        Ok(())
    }

    /// Removes every expired memory from SQLite and the live vector index.
    ///
    /// Returns the number of deleted rows.
    ///
    /// Boundary convention: `expires_at <= now` counts as expired, same as
    /// every other expiration check in this file (recall filtering,
    /// reconciliation, `update()`'s revival guard).
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
/// `store_with_options_id()` always writes both in one SQLite transaction,
/// this is only reachable via external corruption of the file, and is
/// reported as such rather than silently dropped from the index. The
/// active-only filter narrows *which* rows are considered; it never changes
/// that corruption detection outcome for whatever rows do match.
///
/// Expiration boundary: `expires_at <= now` is expired, same convention as
/// `purge_expired()`, recall filtering, and `update()`'s revival guard.
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

    // ---------------------------------------------------------------
    // M3 tests (store/get/forget/restart/corruption/compensation)
    // ---------------------------------------------------------------

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
            std::env::temp_dir().join(format!("memolite-m5-restart-{}.db", Uuid::new_v4()));

        let id = {
            let engine = MemoryEngine::open(&path)
                .await
                .expect("first open should succeed");
            engine
                .store("user prefers dark mode", MemoryType::Semantic, 0.8)
                .await
                .expect("store should succeed")
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

        drop(engine);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }

    #[tokio::test]
    async fn corrupt_memory_row_with_no_embedding_fails_loudly_on_reopen() {
        let path =
            std::env::temp_dir().join(format!("memolite-m5-corrupt-{}.db", Uuid::new_v4()));

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
            let conn = Connection::open(&path).unwrap();
            conn.execute("DELETE FROM embeddings WHERE memory_id = ?1", params![id])
                .unwrap();
            drop(conn);
        }

        let result = MemoryEngine::open(&path).await;
        assert!(
            matches!(result, Err(MemoliteError::Corruption(_))),
            "a memory row with no embedding must surface as Corruption, not be silently skipped"
        );

        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }

    /// A `VectorStore` test double whose `insert` always fails, used to
    /// prove `store_with_options_id()`'s compensation path actually
    /// deletes the orphaned SQLite rows rather than leaving them behind.
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
            384
        }
    }

    #[tokio::test]
    async fn store_rolls_back_orphaned_sqlite_rows_if_the_vector_store_insert_fails() {
        let path =
            std::env::temp_dir().join(format!("memolite-m5-compensate-{}.db", Uuid::new_v4()));

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

        drop(engine);

        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "a failed vector-store insert must not leave an orphaned memories row"
        );

        drop(conn);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }

    /// A `VectorStore` test double whose `delete` always fails, used to
    /// prove `forget()`'s error-surfacing convention: the *original*
    /// vector-store error is returned even after a successful best-effort
    /// `reconcile_vector_index` repair.
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
            Ok(())
        }
        fn dimension(&self) -> usize {
            384
        }
    }

    #[tokio::test]
    async fn forget_surfaces_the_original_error_when_vector_delete_fails() {
        let path = std::env::temp_dir()
            .join(format!("memolite-m5-forget-compensate-{}.db", Uuid::new_v4()));

        let engine = MemoryEngine::open_with_store_internal(
            &path,
            Some(Arc::new(AlwaysFailsDelete) as Arc<dyn VectorStore>),
            BackfillPolicy::ExistingOnly,
        )
        .await
        .expect("engine should open even though its store's delete will fail later");

        let id = engine
            .store(
                "will fail to delete from the vector store",
                MemoryType::Working,
                0.5,
            )
            .await
            .expect("store should succeed");

        let result = engine.forget(&id).await;
        assert!(matches!(result, Err(MemoliteError::VectorStore(_))));

        assert!(engine.get(&id).await.unwrap().is_none());

        drop(engine);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }

    #[tokio::test]
    async fn open_with_store_rejects_a_dimension_mismatched_backend() {
        let path =
            std::env::temp_dir().join(format!("memolite-m5-dim-mismatch-{}.db", Uuid::new_v4()));

        let wrong_dim_store = Arc::new(InMemoryVectorStore::new(7)) as Arc<dyn VectorStore>;

        let result = MemoryEngine::open_with_store_internal(
            &path,
            Some(wrong_dim_store),
            BackfillPolicy::ExistingOnly,
        )
        .await;

        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));

        let _ = std::fs::remove_file(&path);
    }

    // ---------------------------------------------------------------
    // M4 tests (recall_query)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn recall_query_excludes_a_filtered_out_memory_type() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        engine
            .store(
                "the user's favorite editor is Zed",
                MemoryType::Semantic,
                0.8,
            )
            .await
            .unwrap();
        engine
            .store(
                "debugged the login flow yesterday",
                MemoryType::Episodic,
                0.8,
            )
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

        let result = engine
            .recall_query(
                RecallQuery::new("tagged memory")
                    .metadata_equals("project", serde_json::json!("memolite")),
            )
            .await
            .unwrap();

        assert!(result
            .items
            .iter()
            .all(|i| i.memory.id != Uuid::parse_str(&id).unwrap()));
    }

    #[tokio::test]
    async fn recall_query_excludes_expired_by_default_and_includes_when_asked() {
        let path =
            std::env::temp_dir().join(format!("memolite-m5-expired-{}.db", Uuid::new_v4()));
        let engine = MemoryEngine::open(&path).await.expect("engine should open");

        let id = engine
            .store("soon to expire", MemoryType::Working, 0.9)
            .await
            .unwrap();

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

    // ---------------------------------------------------------------
    // M5 tests (store_with_options / ExpiryPolicy / update / supersede)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn store_with_options_persists_metadata_in_both_sqlite_and_vector_store() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let mut metadata = HashMap::new();
        metadata.insert("project".to_string(), serde_json::json!("memolite"));

        let request = StoreRequest::new("has custom metadata", MemoryType::Semantic, 0.7)
            .metadata(metadata.clone());

        let id = engine
            .store_with_options(request)
            .await
            .expect("store_with_options should succeed");

        let stored = engine.get(&id).await.unwrap().expect("memory should exist");
        assert_eq!(stored.metadata, metadata);

        // metadata_equals filter should now match, proving the vector-store
        // side (which recall_query doesn't read metadata from directly, but
        // whose insert call received it) didn't diverge from SQLite's copy.
        let result = engine
            .recall_query(
                RecallQuery::new("has custom metadata")
                    .metadata_equals("project", serde_json::json!("memolite")),
            )
            .await
            .unwrap();
        assert!(result
            .items
            .iter()
            .any(|i| i.memory.id == Uuid::parse_str(&id).unwrap()));
    }

    #[tokio::test]
    async fn store_with_options_rejects_non_positive_custom_expiry() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let zero = StoreRequest::new("bad expiry", MemoryType::Working, 0.5)
            .expiry(ExpiryPolicy::Custom(chrono::Duration::zero()));
        assert!(matches!(
            engine.store_with_options(zero).await,
            Err(MemoliteError::InvalidArgument(_))
        ));

        let negative = StoreRequest::new("bad expiry", MemoryType::Working, 0.5)
            .expiry(ExpiryPolicy::Custom(chrono::Duration::seconds(-10)));
        assert!(matches!(
            engine.store_with_options(negative).await,
            Err(MemoliteError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn store_with_options_never_expiry_leaves_expires_at_null() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let request =
            StoreRequest::new("never expires", MemoryType::Working, 0.5).expiry(ExpiryPolicy::Never);
        let id = engine.store_with_options(request).await.unwrap();

        let stored = engine.get(&id).await.unwrap().unwrap();
        assert!(stored.expires_at.is_none());
    }

    #[tokio::test]
    async fn update_content_only_preserves_other_fields() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let mut metadata = HashMap::new();
        metadata.insert("k".to_string(), serde_json::json!("v"));
        let request = StoreRequest::new("original content", MemoryType::Semantic, 0.6)
            .metadata(metadata.clone());
        let old_id = engine.store_with_options(request).await.unwrap();

        let update = MemoryUpdate {
            new_content: Some("updated content".to_string()),
            ..Default::default()
        };
        let new_id = engine.update(&old_id, update).await.unwrap();

        let new_memory = engine.get(&new_id).await.unwrap().unwrap();
        assert_eq!(new_memory.content, "updated content");
        assert_eq!(new_memory.importance, 0.6);
        assert_eq!(new_memory.memory_type, MemoryType::Semantic);
        assert_eq!(new_memory.metadata, metadata);

        let old_memory = engine.get(&old_id).await.unwrap().unwrap();
        assert_eq!(
            old_memory.superseded_by,
            Some(Uuid::parse_str(&new_id).unwrap())
        );
    }

    #[tokio::test]
    async fn update_preserves_never_expiry() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let request =
            StoreRequest::new("never expires", MemoryType::Working, 0.5).expiry(ExpiryPolicy::Never);
        let old_id = engine.store_with_options(request).await.unwrap();

        let update = MemoryUpdate {
            new_importance: Some(0.9),
            ..Default::default()
        };
        let new_id = engine.update(&old_id, update).await.unwrap();

        let new_memory = engine.get(&new_id).await.unwrap().unwrap();
        assert!(new_memory.expires_at.is_none());
        assert_eq!(new_memory.importance, 0.9);
    }

    #[tokio::test]
    async fn update_rejects_reviving_an_expired_memory_without_explicit_new_expiry() {
        let path =
            std::env::temp_dir().join(format!("memolite-m5-update-expired-{}.db", Uuid::new_v4()));
        let engine = MemoryEngine::open(&path).await.expect("engine should open");

        let id = engine
            .store("will expire", MemoryType::Working, 0.5)
            .await
            .unwrap();

        {
            let conn = engine.conn.lock().unwrap();
            let past = (Utc::now() - chrono::Duration::days(1)).timestamp();
            conn.execute(
                "UPDATE memories SET expires_at = ?1 WHERE id = ?2",
                params![past, id],
            )
            .unwrap();
        }

        let no_expiry_update = MemoryUpdate {
            new_content: Some("still expired".to_string()),
            ..Default::default()
        };
        let result = engine.update(&id, no_expiry_update).await;
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));

        let revive_update = MemoryUpdate {
            new_expiry: Some(ExpiryPolicy::Never),
            ..Default::default()
        };
        let new_id = engine.update(&id, revive_update).await.unwrap();
        let revived = engine.get(&new_id).await.unwrap().unwrap();
        assert!(revived.expires_at.is_none());

        drop(engine);
        std::fs::remove_file(&path).expect("failed to remove temp db file");
    }

    #[tokio::test]
    async fn update_on_a_nonexistent_id_is_not_found() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let result = engine
            .update(&Uuid::new_v4().to_string(), MemoryUpdate::default())
            .await;
        assert!(matches!(result, Err(MemoliteError::NotFound(_))));
    }

    #[tokio::test]
    async fn update_on_a_malformed_id_is_invalid_uuid() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let result = engine.update("not-a-uuid", MemoryUpdate::default()).await;
        assert!(matches!(result, Err(MemoliteError::InvalidUuid(_))));
    }

    /// Fixes the M5 review finding: calling `update()` a second time on an
    /// already-superseded memory must be rejected, not silently re-point
    /// the supersession chain.
    #[tokio::test]
    async fn update_on_an_already_superseded_memory_is_rejected() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let original_id = engine
            .store("v1", MemoryType::Semantic, 0.5)
            .await
            .unwrap();

        let first_update = MemoryUpdate {
            new_content: Some("v2".to_string()),
            ..Default::default()
        };
        let v2_id = engine.update(&original_id, first_update).await.unwrap();

        // Trying to update the now-superseded v1 again must fail, and must
        // not touch v1's superseded_by link (still points at v2).
        let second_update_on_v1 = MemoryUpdate {
            new_content: Some("v1-attempted-again".to_string()),
            ..Default::default()
        };
        let result = engine.update(&original_id, second_update_on_v1).await;
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));

        let v1 = engine.get(&original_id).await.unwrap().unwrap();
        assert_eq!(v1.superseded_by, Some(Uuid::parse_str(&v2_id).unwrap()));

        // Updating v2 (the current version) must still work fine.
        let update_v2 = MemoryUpdate {
            new_content: Some("v3".to_string()),
            ..Default::default()
        };
        let v3_id = engine.update(&v2_id, update_v2).await.unwrap();
        let v3 = engine.get(&v3_id).await.unwrap().unwrap();
        assert_eq!(v3.content, "v3");
    }

    /// Direct unit test of `mark_superseded`'s atomic guard: manually
    /// racing it against an already-completed supersession must fail with
    /// `InvalidArgument`, not silently overwrite the existing link.
    #[tokio::test]
    async fn mark_superseded_rejects_a_second_call_against_the_same_source_id() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let original_id = engine
            .store("source", MemoryType::Semantic, 0.5)
            .await
            .unwrap();
        let uuid = Uuid::parse_str(&original_id).unwrap();

        let winner_id = engine
            .store("winner", MemoryType::Semantic, 0.5)
            .await
            .unwrap();
        let loser_id = engine
            .store("loser", MemoryType::Semantic, 0.5)
            .await
            .unwrap();

        engine.mark_superseded(&uuid, &winner_id).unwrap();

        let result = engine.mark_superseded(&uuid, &loser_id);
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));

        let original = engine.get(&original_id).await.unwrap().unwrap();
        assert_eq!(
            original.superseded_by,
            Some(Uuid::parse_str(&winner_id).unwrap()),
            "the first successful mark_superseded call must remain the source of truth"
        );
    }

    #[tokio::test]
    async fn mark_superseded_on_a_nonexistent_id_is_not_found() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");
        let winner_id = engine.store("winner", MemoryType::Semantic, 0.5).await.unwrap();
        let missing = Uuid::new_v4();
        let result = engine.mark_superseded(&missing, &winner_id);
        assert!(matches!(result, Err(MemoliteError::NotFound(_))));
    }

    #[tokio::test]
    async fn include_superseded_reveals_the_original_after_update() {
        let engine = MemoryEngine::open(":memory:")
            .await
            .expect("engine should open");

        let old_id = engine
            .store("superseded content", MemoryType::Semantic, 0.7)
            .await
            .unwrap();
        engine
            .update(
                &old_id,
                MemoryUpdate {
                    new_content: Some("superseded content v2".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let default_result = engine
            .recall_query(RecallQuery::new("superseded content"))
            .await
            .unwrap();
        assert!(default_result
            .items
            .iter()
            .all(|i| i.memory.id != Uuid::parse_str(&old_id).unwrap()));

        let included_result = engine
            .recall_query(RecallQuery::new("superseded content").include_superseded(true))
            .await
            .unwrap();
        assert!(included_result
            .items
            .iter()
            .any(|i| i.memory.id == Uuid::parse_str(&old_id).unwrap()));
    }
}