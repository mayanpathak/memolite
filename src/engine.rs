
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
 
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use uuid::Uuid;
 
use crate::compression::{self, CompressionResult};
use crate::confidence::ConfidenceLevel;
use crate::embedder::Embedder;
use crate::error::{MemoliteError, Result};
use crate::memory::{Memory, MemoryType};
use crate::requests::{ExpiryPolicy, MemoryUpdate, StoreRequest};
use crate::vector_store::{validate_vector, InMemoryVectorStore, VectorEntry, VectorStore};
 
const MEMORY_COLUMNS: &str = "id, content, type, importance, access_count, \
    created_at, last_accessed, expires_at, superseded_by, metadata, confidence";
 
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillPolicy {
    ExistingOnly,
    UpsertLocal,
    ReplaceAll,
}
 
pub struct MemoryEngine {
    conn: Mutex<Connection>,
    embedder: Mutex<Embedder>,
    vector_store: RwLock<Arc<dyn VectorStore>>,
    #[allow(dead_code)]
    maintenance_running: Arc<AtomicBool>,
}
 
impl MemoryEngine {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_store_internal(path, None, BackfillPolicy::ReplaceAll).await
    }
 
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
 
    pub async fn store(
        &self,
        content: &str,
        memory_type: MemoryType,
        importance: f32,
    ) -> Result<String> {
        self.store_with_options(StoreRequest::new(content, memory_type, importance))
            .await
    }
 
    pub async fn store_with_options(&self, request: StoreRequest) -> Result<String> {
        self.store_with_options_id(request)
            .await
            .map(|id| id.to_string())
    }
 
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
        let metadata_json = serde_json::to_string(&request.metadata)?;
 
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
 
            let tx = conn.transaction()?;
            tx.execute(
                r#"
                INSERT INTO memories (
                    id, content, type, importance, access_count,
                    created_at, last_accessed, expires_at, superseded_by, metadata, confidence
                )
                VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, NULL, ?7, ?8)
                "#,
                params![
                    id_str,
                    request.content,
                    request.memory_type.as_str(),
                    request.importance,
                    created_at.timestamp(),
                    expires_at.map(|e| e.timestamp()),
                    metadata_json,
                    request.confidence.as_str(),
                ],
            )?;
            tx.execute(
                "INSERT INTO embeddings (memory_id, vector, dimension) VALUES (?1, ?2, ?3)",
                params![id_str, encoded_vector, dimension as i64],
            )?;
            tx.commit()?;
        }
 
        let store = {
            let guard = self
                .vector_store
                .read()
                .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            Arc::clone(&*guard)
        };
 
        if let Err(e) = store.insert(id, &vector, request.metadata.clone()).await {
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
 
    pub async fn recall(&self, query: &str) -> Result<Vec<Memory>> {
        let result = self
            .recall_query(crate::recall::RecallQuery::new(query))
            .await?;
        Ok(result.items.into_iter().map(|i| i.memory).collect())
    }
 
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
        // M7: an inverted [created_after, created_before] window can never
        // match anything, so reject it up front rather than silently
        // returning an empty result the caller might mistake for "no
        // matches" instead of "malformed query".
        if let (Some(after), Some(before)) = (query.created_after, query.created_before) {
            if after > before {
                return Err(MemoliteError::InvalidArgument(
                    "created_after must not exceed created_before".into(),
                ));
            }
        }
 
        let query_vector = {
            let mut embedder = self
                .embedder
                .lock()
                .map_err(|_| MemoliteError::Internal("embedder mutex poisoned".into()))?;
            embedder.embed(&query.query_text)?
        };
 
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
 
        let by_id = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
            fetch_memories_by_ids(&conn, &hit_ids)?
        };
 
        let mut scored: Vec<RecallItem> = Vec::with_capacity(hits.len());
        for hit in &hits {
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
            // M7: creation-window filters.
            if let Some(after) = query.created_after {
                if memory.created_at < after {
                    continue;
                }
            }
            if let Some(before) = query.created_before {
                if memory.created_at > before {
                    continue;
                }
            }
 
            let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
            let recency = crate::ranking::recency_factor(days_since_access, memory.memory_type);
            let reinforcement = crate::ranking::reinforcement_factor(memory.access_count);
            let confidence_weight = memory.confidence.weight();
 
            // M7: stale-only filter. A memory is "stale" once it has gone
            // twice its type's decay half-life without being touched. This
            // must reuse ranking::decay_half_life_days rather than invent a
            // second cutoff, so find_stale_memories() and this filter can
            // never silently disagree about what "stale" means.
            if query.only_stale {
                let stale_cutoff_days =
                    crate::ranking::decay_half_life_days(memory.memory_type) * 2.0;
                if days_since_access < stale_cutoff_days {
                    continue;
                }
            }
 
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
                "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1, \
                 confidence = CASE \
                     WHEN confidence = 'inferred' AND access_count + 1 >= {threshold} \
                         THEN 'reinforced' \
                     ELSE confidence \
                 END \
                 WHERE id IN ({id_placeholders})",
                threshold = ConfidenceLevel::PROMOTION_THRESHOLD,
            );
 
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now_ts)];
            for id in &bumped_ids {
                params_vec.push(Box::new(id.to_string()));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|p| p.as_ref()).collect();
            tx.execute(&sql, params_refs.as_slice())?;
            tx.commit()?;
        }
 
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
 
    pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
        let uuid = Uuid::parse_str(id)?;
 
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
 
    pub fn dimension(&self) -> usize {
        self.embedder
            .lock()
            .expect("embedder mutex poisoned")
            .dimension()
    }
 
    pub async fn forget(&self, id: &str) -> Result<()> {
        let uuid = Uuid::parse_str(id)?;
 
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
 
    pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<String> {
        let uuid = Uuid::parse_str(id)?;
        let old = self
            .get(id)
            .await?
            .ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;
 
        if old.superseded_by.is_some() {
            return Err(MemoliteError::InvalidArgument(format!(
                "memory {id} has already been superseded and cannot be updated directly; \
                 update the memory it was superseded by instead"
            )));
        }
 
        let now = Utc::now();
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
        request.expiry = update.new_expiry.unwrap_or(match old.expires_at {
            None => ExpiryPolicy::Never,
            Some(old_expires_at) => {
                ExpiryPolicy::Custom(old_expires_at.signed_duration_since(now))
            }
        });
        request.metadata = update.new_metadata.unwrap_or_else(|| old.metadata.clone());
        request.confidence = update.new_confidence.unwrap_or(ConfidenceLevel::Inferred);
 
        let new_uuid = self.store_with_options_id(request).await?;
 
        if let Err(e) = self.mark_superseded(&uuid, &new_uuid.to_string()) {
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
            return Err(MemoliteError::VectorStore(combined));
        }
 
        Ok(deleted_ids.len())
    }
 
    // ---------------------------------------------------------------
    // M7 — temporal querying
    //
    // Everything below is a pure SQLite read: no embedder call, no
    // vector-store call, no lock ever held across an `.await`. These
    // methods exist for direct audit/inspection of the database and are
    // orthogonal to semantic recall — `RecallQuery`'s `created_after`/
    // `created_before`/`only_stale` filters (wired in above, inside
    // `recall_query`) are the semantic-search-facing equivalent of the
    // same concepts.
    // ---------------------------------------------------------------
 
    /// Every memory created within `[start, end]`, inclusive, ordered by
    /// creation time ascending. A pure SQLite range scan — does not touch
    /// the embedder or vector store.
    pub async fn query_by_time_range(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Memory>> {
        if start > end {
            return Err(MemoliteError::InvalidArgument(
                "start must not be after end".into(),
            ));
        }
 
        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let sql = format!(
            "SELECT {MEMORY_COLUMNS} FROM memories \
             WHERE created_at >= ?1 AND created_at <= ?2 \
             ORDER BY created_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![start.timestamp(), end.timestamp()], row_to_memory)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
 
    /// Every memory *created or accessed* since `since`, most recent
    /// creation first.
    ///
    /// Recalling a memory bumps `last_accessed`, so a pure read can
    /// legitimately make an old memory show up here — that's intentional,
    /// not a bug: this method answers "what's touched the database since
    /// this point", not "what's been edited". A true edit-only audit would
    /// need a separate `updated_at` column bumped only by `update()`; that
    /// column does not exist yet and is a stated follow-up, not something
    /// this method claims to already provide.
    pub async fn created_or_accessed_since(&self, since: DateTime<Utc>) -> Result<Vec<Memory>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let sql = format!(
            "SELECT {MEMORY_COLUMNS} FROM memories \
             WHERE created_at >= ?1 OR last_accessed >= ?1 \
             ORDER BY created_at DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![since.timestamp()], row_to_memory)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
 
    /// Every *active* memory (not superseded, not expired) that hasn't
    /// been accessed in at least twice its type's decay half-life.
    ///
    /// Uses the exact same "active" definition as the live vector index
    /// (`get_active_memories`, below) and the exact same staleness cutoff
    /// as `RecallQuery::only_stale`'s in-loop filter above, so all three
    /// can never silently disagree about what "stale" or "active" means.
    pub async fn find_stale_memories(&self) -> Result<Vec<Memory>> {
        let now = Utc::now();
        let active = self.get_active_memories()?;
        Ok(active
            .into_iter()
            .filter(|m| {
                let cutoff_days = crate::ranking::decay_half_life_days(m.memory_type) * 2.0;
                let days_since_access = (now - m.last_accessed).num_seconds() as f64 / 86400.0;
                days_since_access >= cutoff_days
            })
            .collect())
    }
 
    /// Not-superseded, not-(already-)expired memories. Synchronous SQLite
    /// helper shared by `find_stale_memories`; kept private since it's an
    /// implementation detail of "what counts as active", not a public
    /// query shape on its own.
    fn get_active_memories(&self) -> Result<Vec<Memory>> {
        let now = Utc::now().timestamp();
        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let sql = format!(
            "SELECT {MEMORY_COLUMNS} FROM memories \
             WHERE superseded_by IS NULL AND (expires_at IS NULL OR expires_at > ?1)"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now], row_to_memory)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
 
    /// Walks a memory's `superseded_by` chain forward from `id` to its
    /// current version, inclusive of both ends. `id` may be any memory in
    /// the chain, not just the original.
    ///
    /// Guards against a corrupted (cyclic) `superseded_by` chain, which
    /// `update()`'s own API can never produce but a hand-edited database
    /// could: after 10,000 hops it fails loudly with `Internal` rather than
    /// looping forever.
    pub async fn find_superseded_chain(&self, id: &str) -> Result<Vec<Memory>> {
        let start_uuid = Uuid::parse_str(id)?;
        let mut chain = Vec::new();
        let mut current = self
            .get(id)
            .await?
            .ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;
        chain.push(current.clone());
 
        let mut guard_iterations = 0usize;
        while let Some(next_id) = current.superseded_by {
            guard_iterations += 1;
            if guard_iterations > 10_000 {
                return Err(MemoliteError::Internal(format!(
                    "superseded_by cycle detected starting from {start_uuid}"
                )));
            }
            let Some(next) = self.get(&next_id.to_string()).await? else {
                break;
            };
            chain.push(next.clone());
            current = next;
        }
        Ok(chain)
    }
 
    // ---------------------------------------------------------------
    // M9 — compression + index rebuild
    //
    // `compress_old_memories` is the only method here that touches the
    // embedder/vector-store *and* SQLite together; the rest are thin,
    // synchronous SQLite helpers or a one-line delegation into
    // `reconcile_vector_index` (Step 0.8 / the top of this file).
    // ---------------------------------------------------------------
 
    /// All episodic memories with `created_at <= now - days`. A pure
    /// SQLite range scan, reused by `compress_old_memories` as the first,
    /// cheap filter before the more expensive eligibility/embedding checks.
    fn get_episodic_memories_older_than(&self, days: i64) -> Result<Vec<Memory>> {
        let cutoff = (Utc::now() - chrono::Duration::days(days)).timestamp();
        let conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let sql =
            format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE type = 'episodic' AND created_at <= ?1");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![cutoff], row_to_memory)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
 
    /// Resynchronizes the entire vector index from SQLite via
    /// `BackfillPolicy::ReplaceAll`. Useful for manual recovery if the
    /// vector store is ever suspected to have drifted from SQLite (e.g.
    /// after a hand-edited database, or a crash mid-compensation).
    pub async fn rebuild_vector_index(&self) -> Result<()> {
        let store = {
            let guard = self
                .vector_store
                .read()
                .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            Arc::clone(&*guard)
        };
        reconcile_vector_index(&self.conn, &store, BackfillPolicy::ReplaceAll).await
    }
 
    /// Consolidates old, low-importance episodic memories into semantic
    /// summaries.
    ///
    /// Candidates are episodic memories older than 14 days, filtered by
    /// `compression::is_compression_eligible`. Every candidate *must* have
    /// a persisted embedding (Step 5.4's invariant: `store_with_options_id`
    /// always writes memory + embedding in one transaction) — a missing
    /// embedding is treated as `Err(Corruption)`, never silently skipped.
    ///
    /// Candidates are clustered by cosine similarity (threshold `0.85`);
    /// any cluster with 3+ members is summarized (extractively) into one
    /// new `Semantic`/`Inferred` memory, and every original in that
    /// cluster is marked `superseded_by` the new summary — recoverable via
    /// `RecallQuery::include_superseded(true)`, never deleted.
    ///
    /// Returns the total number of *original* memories folded into a
    /// summary (not the number of summaries created).
    pub async fn compress_old_memories(&self) -> Result<usize> {
        let candidates: Vec<Memory> = self
            .get_episodic_memories_older_than(14)?
            .into_iter()
            .filter(compression::is_compression_eligible)
            .collect();
 
        if candidates.is_empty() {
            return Ok(0);
        }
 
        let expected_dim = {
            let guard = self
                .vector_store
                .read()
                .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            guard.dimension()
        };
 
        let mut with_vectors: Vec<(Uuid, Vec<f32>)> = Vec::with_capacity(candidates.len());
        for m in &candidates {
            // A candidate came straight out of `memories`, which — per
            // Step 5.4's invariant — always has a matching embeddings row.
            // A miss here means the database is corrupted, so this fails
            // loudly instead of silently excluding the candidate from
            // clustering.
            let vector = self.get_embedding(&m.id.to_string()).await?.ok_or_else(|| {
                MemoliteError::Corruption(format!(
                    "memory {} has no persisted embedding",
                    m.id
                ))
            })?;
            validate_vector(&format!("embedding for {}", m.id), &vector, expected_dim)?;
            with_vectors.push((m.id, vector));
        }
 
        let clusters = compression::greedy_cluster(&with_vectors, 0.85);
        let mut compressed_count = 0usize;
 
        for cluster in clusters.into_iter().filter(|c| c.member_ids.len() >= 3) {
            let members: Vec<Memory> = candidates
                .iter()
                .filter(|m| cluster.member_ids.contains(&m.id))
                .cloned()
                .collect();
 
            let CompressionResult {
                summary_content,
                original_ids,
            } = compression::summarize_cluster(&members, 0.85)?;
 
            let mut metadata = HashMap::new();
            metadata.insert(
                "compression.original_ids".to_string(),
                serde_json::json!(original_ids.iter().map(Uuid::to_string).collect::<Vec<_>>()),
            );
            metadata.insert(
                "compression.algorithm_version".to_string(),
                serde_json::json!(compression::COMPRESSION_ALGORITHM_VERSION),
            );
 
            let mut request = StoreRequest::new(&summary_content, MemoryType::Semantic, 0.3)
                .with_confidence(ConfidenceLevel::Inferred);
            request.metadata = metadata;
 
            let new_uuid = self.store_with_options_id(request).await?;
 
            if let Err(e) = self.mark_all_superseded(&original_ids, &new_uuid.to_string()) {
                // Best-effort compensation: try to remove the just-created
                // summary from both SQLite and the vector store so the
                // database isn't left with an orphaned, un-superseding
                // summary. Deletion failures here are not escalated to
                // CompensationFailed (unlike store/update's compensation
                // paths) because the *original* error `e` is already the
                // one that must be returned — we simply do our best to
                // clean up alongside it.
                let _ = {
                    let conn = self.conn.lock().map_err(|_| {
                        MemoliteError::Internal("database mutex poisoned".into())
                    });
                    conn.and_then(|c| {
                        c.execute(
                            "DELETE FROM memories WHERE id = ?1",
                            params![new_uuid.to_string()],
                        )
                        .map_err(Into::into)
                    })
                };
                let store = {
                    let guard = self.vector_store.read().map_err(|_| {
                        MemoliteError::Internal("vector-store lock poisoned".into())
                    })?;
                    Arc::clone(&*guard)
                };
                let _ = store.delete(new_uuid).await;
 
                return Err(e);
            }
 
            compressed_count += members.len();
        }
 
        Ok(compressed_count)
    }
 
    /// Marks every id in `old_ids` as `superseded_by` in `new_id`, all in
    /// one SQLite transaction — mirrors `mark_superseded` (M5), just over
    /// a batch of ids instead of one.
    fn mark_all_superseded(&self, old_ids: &[Uuid], new_id: &str) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let tx = conn.transaction()?;
        for old_id in old_ids {
            tx.execute(
                "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
                params![new_id, old_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}
 
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
    };
 
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
 
    let confidence_str: String = row.get(10)?;
    let confidence =
        ConfidenceLevel::parse_str(&confidence_str).map_err(|e| to_sql_conversion_err(10, e))?;
 
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
        confidence,
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
// ---------------------------------------------------------------------
// Paste everything below directly under the bottom of engine.rs
// (after the MemoryEngine impl block and any other production code).
//
// Contains two sibling test modules:
//   - `tests`        : core engine tests, with a nested `m7_tests`
//                       submodule (time-range / staleness / superseded
//                       chain queries).
//   - `compression_tests` : memory-compression / consolidation tests.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::RecallQuery;
    use crate::vector_store::VectorHit;
    use async_trait::async_trait;

    async fn open_test_engine() -> MemoryEngine {
        MemoryEngine::open(":memory:").await.unwrap()
    }

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

        let second_update_on_v1 = MemoryUpdate {
            new_content: Some("v1-attempted-again".to_string()),
            ..Default::default()
        };
        let result = engine.update(&original_id, second_update_on_v1).await;
        assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));

        let v1 = engine.get(&original_id).await.unwrap().unwrap();
        assert_eq!(v1.superseded_by, Some(Uuid::parse_str(&v2_id).unwrap()));

        let update_v2 = MemoryUpdate {
            new_content: Some("v3".to_string()),
            ..Default::default()
        };
        let v3_id = engine.update(&v2_id, update_v2).await.unwrap();
        let v3 = engine.get(&v3_id).await.unwrap().unwrap();
        assert_eq!(v3.content, "v3");
    }

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

    #[tokio::test]
    async fn store_defaults_to_explicit_confidence() {
        let engine = open_test_engine().await;
        let id = engine
            .store("plain fact", MemoryType::Semantic, 0.5)
            .await
            .unwrap();
        let memory = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(memory.confidence, ConfidenceLevel::Explicit);
    }

    #[tokio::test]
    async fn store_with_options_persists_requested_confidence() {
        let engine = open_test_engine().await;
        for level in [
            ConfidenceLevel::Explicit,
            ConfidenceLevel::Inferred,
            ConfidenceLevel::Reinforced,
        ] {
            let id = engine
                .store_with_options(
                    StoreRequest::new("a fact", MemoryType::Semantic, 0.5)
                        .with_confidence(level),
                )
                .await
                .unwrap();
            let memory = engine.get(&id).await.unwrap().unwrap();
            assert_eq!(memory.confidence, level);
        }
    }

    #[tokio::test]
    async fn explicit_memory_outranks_identical_inferred_memory() {
        let engine = open_test_engine().await;

        let explicit_id = engine
            .store_with_options(
                StoreRequest::new("the sky is blue during the day", MemoryType::Semantic, 0.8)
                    .with_confidence(ConfidenceLevel::Explicit),
            )
            .await
            .unwrap();
        let inferred_id = engine
            .store_with_options(
                StoreRequest::new("the sky appears blue in daylight", MemoryType::Semantic, 0.8)
                    .with_confidence(ConfidenceLevel::Inferred),
            )
            .await
            .unwrap();

        let result = engine
            .recall_query(RecallQuery::new("sky is blue").limit(2))
            .await
            .unwrap();
        assert_eq!(result.items.len(), 2);

        let explicit_score = result
            .items
            .iter()
            .find(|i| i.memory.id.to_string() == explicit_id)
            .unwrap()
            .score;
        let inferred_score = result
            .items
            .iter()
            .find(|i| i.memory.id.to_string() == inferred_id)
            .unwrap()
            .score;
        assert!(explicit_score > inferred_score);
    }

    #[tokio::test]
    async fn inferred_memory_promotes_to_reinforced_after_five_recalls() {
        let engine = open_test_engine().await;

        let id = engine
            .store_with_options(
                StoreRequest::new("user likes tabs over spaces", MemoryType::Semantic, 0.6)
                    .with_confidence(ConfidenceLevel::Inferred),
            )
            .await
            .unwrap();

        for i in 0..ConfidenceLevel::PROMOTION_THRESHOLD {
            let result = engine
                .recall_query(RecallQuery::new("tabs over spaces").limit(1))
                .await
                .unwrap();
            assert_eq!(result.items.len(), 1);
            let memory = &result.items[0].memory;
            assert_eq!(memory.id.to_string(), id);

            if i + 1 < ConfidenceLevel::PROMOTION_THRESHOLD {
                assert_eq!(
                    memory.confidence,
                    ConfidenceLevel::Inferred,
                    "should still be inferred after {} recall(s)",
                    i + 1
                );
            } else {
                assert_eq!(
                    memory.confidence,
                    ConfidenceLevel::Reinforced,
                    "should be reinforced after {} recalls",
                    ConfidenceLevel::PROMOTION_THRESHOLD
                );
            }
        }

        let result = engine
            .recall_query(RecallQuery::new("tabs over spaces").limit(1))
            .await
            .unwrap();
        assert_eq!(result.items[0].memory.confidence, ConfidenceLevel::Reinforced);
    }

    #[tokio::test]
    async fn explicit_memory_is_never_auto_promoted_or_demoted_by_recall() {
        let engine = open_test_engine().await;
        let id = engine
            .store_with_options(
                StoreRequest::new("explicit fact", MemoryType::Semantic, 0.6)
                    .with_confidence(ConfidenceLevel::Explicit),
            )
            .await
            .unwrap();

        for _ in 0..10 {
            engine
                .recall_query(RecallQuery::new("explicit fact").limit(1))
                .await
                .unwrap();
        }

        let memory = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(memory.confidence, ConfidenceLevel::Explicit);
        assert_eq!(memory.access_count, 10);
    }

    #[tokio::test]
    async fn update_without_new_confidence_downgrades_to_inferred() {
        let engine = open_test_engine().await;
        let id = engine
            .store_with_options(
                StoreRequest::new("the user uses VS Code", MemoryType::Semantic, 0.7)
                    .with_confidence(ConfidenceLevel::Explicit),
            )
            .await
            .unwrap();

        let new_id = engine
            .update(
                &id,
                MemoryUpdate {
                    new_content: Some("the user uses Zed".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let updated = engine.get(&new_id).await.unwrap().unwrap();
        assert_eq!(updated.confidence, ConfidenceLevel::Inferred);
    }

    #[tokio::test]
    async fn update_can_explicitly_preserve_or_set_confidence() {
        let engine = open_test_engine().await;
        let id = engine
            .store_with_options(
                StoreRequest::new("fact", MemoryType::Semantic, 0.5)
                    .with_confidence(ConfidenceLevel::Explicit),
            )
            .await
            .unwrap();

        let new_id = engine
            .update(
                &id,
                MemoryUpdate {
                    new_content: Some("updated fact".to_string()),
                    new_confidence: Some(ConfidenceLevel::Explicit),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let updated = engine.get(&new_id).await.unwrap().unwrap();
        assert_eq!(updated.confidence, ConfidenceLevel::Explicit);
    }

    #[tokio::test]
    async fn row_to_memory_round_trips_all_three_confidence_levels_via_recall() {
        let engine = open_test_engine().await;
        for (i, level) in [
            ConfidenceLevel::Explicit,
            ConfidenceLevel::Inferred,
            ConfidenceLevel::Reinforced,
        ]
        .into_iter()
        .enumerate()
        {
            engine
                .store_with_options(
                    StoreRequest::new(&format!("unique fact number {i}"), MemoryType::Semantic, 0.9)
                        .with_confidence(level),
                )
                .await
                .unwrap();
        }

        let results = engine
            .recall_query(RecallQuery::new("unique fact").limit(10))
            .await
            .unwrap();
        assert_eq!(results.items.len(), 3);
    }

    // -------------------------------------------------------------
    // m7: time-range queries, staleness, and superseded-chain walks
    // -------------------------------------------------------------
    mod m7_tests {
        use super::*;
        use crate::recall::RecallQuery;

        /// Opens a fresh MemoryEngine backed by a real temp-file SQLite
        /// database (not `:memory:`, so the on-disk path matches production
        /// behavior) that is deleted when the returned TempDir drops.
        async fn test_engine() -> (MemoryEngine, tempfile::TempDir) {
            let dir = tempfile::tempdir().expect("failed to create temp dir");
            let db_path = dir.path().join("m7_test.db");
            let engine = MemoryEngine::open(&db_path)
                .await
                .expect("failed to open engine");
            (engine, dir)
        }

        /// Directly rewrites a row's `created_at`/`last_accessed` timestamps.
        /// Only reachable via raw SQL -- there is no public API that lets a
        /// caller backdate a memory, which is exactly why these tests need
        /// direct `conn` access and therefore live inside `engine.rs` rather
        /// than in an external `tests/*.rs` file.
        fn backdate(engine: &MemoryEngine, id: &str, days_ago: i64) {
            let conn = engine.conn.lock().unwrap();
            let ts = (Utc::now() - chrono::Duration::days(days_ago)).timestamp();
            conn.execute(
                "UPDATE memories SET created_at = ?1, last_accessed = ?1 WHERE id = ?2",
                params![ts, id],
            )
            .unwrap();
        }

        fn backdate_last_accessed_only(engine: &MemoryEngine, id: &str, days_ago: i64) {
            let conn = engine.conn.lock().unwrap();
            let ts = (Utc::now() - chrono::Duration::days(days_ago)).timestamp();
            conn.execute(
                "UPDATE memories SET last_accessed = ?1 WHERE id = ?2",
                params![ts, id],
            )
            .unwrap();
        }

        // -----------------------------------------------------------------
        // query_by_time_range
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn query_by_time_range_rejects_start_after_end() {
            let (engine, _dir) = test_engine().await;
            let now = Utc::now();
            let err = engine
                .query_by_time_range(now, now - chrono::Duration::days(1))
                .await
                .unwrap_err();
            assert!(matches!(err, MemoliteError::InvalidArgument(_)));
        }

        #[tokio::test]
        async fn query_by_time_range_accepts_a_single_instant_window() {
            let (engine, _dir) = test_engine().await;
            let now = Utc::now();
            // start == end must be accepted, not rejected as "inverted".
            let result = engine.query_by_time_range(now, now).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn query_by_time_range_returns_only_the_window_in_order() {
            let (engine, _dir) = test_engine().await;
            let id_old = engine
                .store("old fact about the weather", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            let id_new = engine
                .store("new fact about the weather", MemoryType::Semantic, 0.5)
                .await
                .unwrap();

            backdate(&engine, &id_old, 10);

            let window_start = Utc::now() - chrono::Duration::days(1);
            let window_end = Utc::now() + chrono::Duration::minutes(1);
            let results = engine
                .query_by_time_range(window_start, window_end)
                .await
                .unwrap();

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id.to_string(), id_new);
        }

        #[tokio::test]
        async fn query_by_time_range_on_empty_engine_returns_empty() {
            let (engine, _dir) = test_engine().await;
            let now = Utc::now();
            let results = engine
                .query_by_time_range(now - chrono::Duration::days(1), now)
                .await
                .unwrap();
            assert!(results.is_empty());
        }

        // -----------------------------------------------------------------
        // RecallQuery.created_after / .created_before
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn recall_query_rejects_inverted_created_after_before() {
            let (engine, _dir) = test_engine().await;
            engine
                .store("some fact for inversion test", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            let now = Utc::now();
            let err = engine
                .recall_query(
                    RecallQuery::new("some fact for inversion test")
                        .created_after(now)
                        .created_before(now - chrono::Duration::days(1)),
                )
                .await
                .unwrap_err();
            assert!(matches!(err, MemoliteError::InvalidArgument(_)));
        }

        #[tokio::test]
        async fn recall_query_created_before_excludes_memories_created_after_cutoff() {
            let (engine, _dir) = test_engine().await;
            let id_early = engine
                .store("shared topic alpha marker", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            backdate(&engine, &id_early, 5);

            let cutoff = Utc::now() - chrono::Duration::days(1);

            let id_late = engine
                .store("shared topic alpha marker again", MemoryType::Semantic, 0.5)
                .await
                .unwrap();

            let results = engine
                .recall_query(RecallQuery::new("shared topic alpha marker").created_before(cutoff))
                .await
                .unwrap();

            let returned_ids: Vec<String> = results
                .items
                .iter()
                .map(|i| i.memory.id.to_string())
                .collect();
            assert!(returned_ids.contains(&id_early));
            assert!(!returned_ids.contains(&id_late));
        }

        #[tokio::test]
        async fn recall_query_created_after_excludes_memories_created_before_cutoff() {
            let (engine, _dir) = test_engine().await;
            let id_early = engine
                .store("beta marker early fact", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            backdate(&engine, &id_early, 5);

            let cutoff = Utc::now() - chrono::Duration::days(1);

            let id_late = engine
                .store("beta marker late fact", MemoryType::Semantic, 0.5)
                .await
                .unwrap();

            let results = engine
                .recall_query(RecallQuery::new("beta marker fact").created_after(cutoff))
                .await
                .unwrap();

            let returned_ids: Vec<String> = results
                .items
                .iter()
                .map(|i| i.memory.id.to_string())
                .collect();
            assert!(returned_ids.contains(&id_late));
            assert!(!returned_ids.contains(&id_early));
        }

        // -----------------------------------------------------------------
        // created_or_accessed_since
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn created_or_accessed_since_finds_a_recently_created_memory() {
            let (engine, _dir) = test_engine().await;
            let cutoff = Utc::now() - chrono::Duration::minutes(1);
            let id = engine
                .store("freshly created audit fact", MemoryType::Semantic, 0.5)
                .await
                .unwrap();

            let recent = engine.created_or_accessed_since(cutoff).await.unwrap();
            assert!(recent.iter().any(|m| m.id.to_string() == id));
        }

        #[tokio::test]
        async fn created_or_accessed_since_catches_a_fresh_recall_of_an_old_memory() {
            let (engine, _dir) = test_engine().await;
            let id = engine
                .store("audit me via recall bump", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            backdate(&engine, &id, 5);

            let cutoff = Utc::now() - chrono::Duration::hours(1);

            // recall() bumps last_accessed to "now", which should make this
            // old memory show up in the "since cutoff" window even though it
            // was created 5 days ago.
            engine.recall("audit me via recall bump").await.unwrap();

            let recent = engine.created_or_accessed_since(cutoff).await.unwrap();
            assert!(recent.iter().any(|m| m.id.to_string() == id));
        }

        #[tokio::test]
        async fn created_or_accessed_since_excludes_untouched_old_memories() {
            let (engine, _dir) = test_engine().await;
            let id = engine
                .store("never touched again fact", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            backdate(&engine, &id, 5);

            let cutoff = Utc::now() - chrono::Duration::hours(1);
            let recent = engine.created_or_accessed_since(cutoff).await.unwrap();
            assert!(!recent.iter().any(|m| m.id.to_string() == id));
        }

        // -----------------------------------------------------------------
        // find_stale_memories / RecallQuery.only_stale
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn find_stale_memories_respects_the_memory_types_decay_cutoff() {
            let (engine, _dir) = test_engine().await;
            let id = engine
                .store("stale candidate episodic", MemoryType::Episodic, 0.5)
                .await
                .unwrap();

            // Episodic half-life is 14 days -> stale cutoff is 28 days.
            backdate_last_accessed_only(&engine, &id, 29);

            let stale = engine.find_stale_memories().await.unwrap();
            assert!(stale.iter().any(|m| m.id.to_string() == id));
        }

        #[tokio::test]
        async fn find_stale_memories_excludes_recently_accessed_memories() {
            let (engine, _dir) = test_engine().await;
            let id = engine
                .store("fresh episodic memory", MemoryType::Episodic, 0.5)
                .await
                .unwrap();
            // Well under the 28-day episodic staleness cutoff.
            backdate_last_accessed_only(&engine, &id, 1);

            let stale = engine.find_stale_memories().await.unwrap();
            assert!(!stale.iter().any(|m| m.id.to_string() == id));
        }

        #[tokio::test]
        async fn find_stale_memories_excludes_superseded_memories() {
            let (engine, _dir) = test_engine().await;
            let id_a = engine
                .store("superseded stale candidate", MemoryType::Episodic, 0.5)
                .await
                .unwrap();
            backdate_last_accessed_only(&engine, &id_a, 29);

            let _id_b = engine
                .update(
                    &id_a,
                    MemoryUpdate {
                        new_content: Some("replacement content".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();

            let stale = engine.find_stale_memories().await.unwrap();
            // id_a is now superseded, so it must not appear even though it
            // would otherwise pass the staleness cutoff.
            assert!(!stale.iter().any(|m| m.id.to_string() == id_a));
        }

        #[tokio::test]
        async fn only_stale_filter_agrees_with_find_stale_memories() {
            let (engine, _dir) = test_engine().await;
            let id = engine
                .store("only stale filter marker fact", MemoryType::Episodic, 0.5)
                .await
                .unwrap();
            backdate_last_accessed_only(&engine, &id, 29);

            // find_stale_memories() must run FIRST: recall_query() bumps
            // access_count/last_accessed on every item it returns, regardless
            // of which filter let that item through. Calling recall_query()
            // first would reset last_accessed to "now" as a side effect of
            // successfully recalling the stale memory, un-staling it before
            // find_stale_memories() gets a chance to observe it. That bump is
            // correct production behavior (an access is an access) -- it's
            // this test's ordering that has to respect it.
            let via_find_stale = engine.find_stale_memories().await.unwrap();
            assert!(via_find_stale.iter().any(|m| m.id.to_string() == id));

            let via_recall = engine
                .recall_query(RecallQuery::new("only stale filter marker fact").only_stale(true))
                .await
                .unwrap();
            assert!(via_recall.items.iter().any(|i| i.memory.id.to_string() == id));
        }

        #[tokio::test]
        async fn only_stale_false_by_default_includes_fresh_memories() {
            let (engine, _dir) = test_engine().await;
            let id = engine
                .store("default recall includes fresh fact", MemoryType::Semantic, 0.5)
                .await
                .unwrap();

            let results = engine
                .recall("default recall includes fresh fact")
                .await
                .unwrap();
            assert!(results.iter().any(|m| m.id.to_string() == id));
        }

        // -----------------------------------------------------------------
        // find_superseded_chain
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn find_superseded_chain_walks_three_generations() {
            let (engine, _dir) = test_engine().await;
            let id_a = engine
                .store("chain v1", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            let id_b = engine
                .update(
                    &id_a,
                    MemoryUpdate {
                        new_content: Some("chain v2".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let id_c = engine
                .update(
                    &id_b,
                    MemoryUpdate {
                        new_content: Some("chain v3".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();

            let chain = engine.find_superseded_chain(&id_a).await.unwrap();
            let ids: Vec<String> = chain.iter().map(|m| m.id.to_string()).collect();
            assert_eq!(ids, vec![id_a, id_b, id_c]);
        }

        #[tokio::test]
        async fn find_superseded_chain_from_a_middle_link_returns_only_the_remaining_tail() {
            let (engine, _dir) = test_engine().await;
            let id_a = engine
                .store("mid-chain v1", MemoryType::Semantic, 0.5)
                .await
                .unwrap();
            let id_b = engine
                .update(
                    &id_a,
                    MemoryUpdate {
                        new_content: Some("mid-chain v2".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let id_c = engine
                .update(
                    &id_b,
                    MemoryUpdate {
                        new_content: Some("mid-chain v3".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();

            // Starting from the middle link should NOT include id_a.
            let chain = engine.find_superseded_chain(&id_b).await.unwrap();
            let ids: Vec<String> = chain.iter().map(|m| m.id.to_string()).collect();
            assert_eq!(ids, vec![id_b, id_c]);
        }

        #[tokio::test]
        async fn find_superseded_chain_on_a_never_updated_memory_is_a_single_element_chain() {
            let (engine, _dir) = test_engine().await;
            let id = engine
                .store("never updated", MemoryType::Semantic, 0.5)
                .await
                .unwrap();

            let chain = engine.find_superseded_chain(&id).await.unwrap();
            assert_eq!(chain.len(), 1);
            assert_eq!(chain[0].id.to_string(), id);
        }

        #[tokio::test]
        async fn find_superseded_chain_on_a_nonexistent_id_is_not_found() {
            let (engine, _dir) = test_engine().await;
            let random_id = Uuid::new_v4().to_string();
            let err = engine.find_superseded_chain(&random_id).await.unwrap_err();
            assert!(matches!(err, MemoliteError::NotFound(_)));
        }

        #[tokio::test]
        async fn find_superseded_chain_on_a_malformed_id_is_invalid_uuid() {
            let (engine, _dir) = test_engine().await;
            let err = engine
                .find_superseded_chain("not-a-uuid")
                .await
                .unwrap_err();
            assert!(matches!(err, MemoliteError::InvalidUuid(_)));
        }

        #[tokio::test]
        async fn find_superseded_chain_trips_the_cycle_guard_on_corrupted_data() {
            let (engine, _dir) = test_engine().await;
            let id_a = engine.store("cycle a", MemoryType::Semantic, 0.5).await.unwrap();
            let id_b = engine.store("cycle b", MemoryType::Semantic, 0.5).await.unwrap();

            // Hand-craft a cycle: a -> b -> a. Only reachable via raw SQL,
            // since the public update() API can never produce this shape.
            {
                let conn = engine.conn.lock().unwrap();
                conn.execute(
                    "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
                    params![id_b, id_a],
                )
                .unwrap();
                conn.execute(
                    "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
                    params![id_a, id_b],
                )
                .unwrap();
            }

            let err = engine.find_superseded_chain(&id_a).await.unwrap_err();
            assert!(matches!(err, MemoliteError::Internal(_)));
        }
    }
}

// ---------------------------------------------------------------------
// Memory compression / consolidation tests
// ---------------------------------------------------------------------
#[cfg(test)]
mod compression_tests {
    use super::*;
    use crate::recall::RecallQuery;

    /// Directly rewrites a memory's `created_at` column via the engine's
    /// own connection -- the private-field equivalent of what the
    /// black-box integration tests do through a second `Connection` to
    /// an on-disk file (not possible here since `:memory:` databases
    /// can't be shared across connections).
    fn backdate(engine: &MemoryEngine, id: &str, days_ago: i64) {
        let conn = engine.conn.lock().expect("database mutex poisoned");
        let cutoff = (Utc::now() - chrono::Duration::days(days_ago)).timestamp();
        conn.execute(
            "UPDATE memories SET created_at = ?1 WHERE id = ?2",
            params![cutoff, id],
        )
        .expect("backdate created_at");
    }

    /// Inserts a `memories` row with no matching `embeddings` row,
    /// directly via the engine's connection -- simulates the corruption
    /// case `compress_old_memories` must fail loudly on.
    fn insert_orphan_episodic_row(engine: &MemoryEngine, importance: f32, days_ago: i64) -> Uuid {
        let conn = engine.conn.lock().expect("database mutex poisoned");
        let id = Uuid::new_v4();
        let created_at = (Utc::now() - chrono::Duration::days(days_ago)).timestamp();
        conn.execute(
            r#"
            INSERT INTO memories (
                id, content, type, importance, access_count,
                created_at, last_accessed, expires_at, superseded_by, metadata, confidence
            )
            VALUES (?1, 'orphan with no embedding', 'episodic', ?2, 0, ?3, ?3, NULL, NULL, '{}', 'explicit')
            "#,
            params![id.to_string(), importance, created_at],
        )
        .expect("insert orphan memory row");
        id
    }

    #[tokio::test]
    async fn compress_old_memories_folds_similar_old_episodic_memories_into_one_summary() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();

        let id_a = engine
            .store(
                "The user debugged a login timeout issue in the auth service",
                MemoryType::Episodic,
                0.2,
            )
            .await
            .unwrap();
        let id_b = engine
            .store(
                "The user debugged a login timeout problem in the auth service",
                MemoryType::Episodic,
                0.2,
            )
            .await
            .unwrap();
        let id_c = engine
            .store(
                "The user debugged a login timeout bug in the auth service",
                MemoryType::Episodic,
                0.2,
            )
            .await
            .unwrap();
        let id_important = engine
            .store(
                "The production database credentials rotated successfully",
                MemoryType::Episodic,
                0.9,
            )
            .await
            .unwrap();

        for id in [&id_a, &id_b, &id_c] {
            backdate(&engine, id, 20);
        }

        let compressed = engine.compress_old_memories().await.unwrap();
        assert_eq!(compressed, 3);

        for id in [&id_a, &id_b, &id_c] {
            let mem = engine.get(id).await.unwrap().expect("original still present");
            assert!(mem.superseded_by.is_some());
        }

        let important = engine.get(&id_important).await.unwrap().unwrap();
        assert!(important.superseded_by.is_none());

        let summary_id = engine.get(&id_a).await.unwrap().unwrap().superseded_by.unwrap();
        let summary = engine
            .get(&summary_id.to_string())
            .await
            .unwrap()
            .expect("summary should exist");
        assert_eq!(summary.memory_type, MemoryType::Semantic);
        assert_eq!(summary.confidence, ConfidenceLevel::Inferred);

        let original_ids_value = summary
            .metadata
            .get("compression.original_ids")
            .expect("summary metadata should record original ids");
        let original_ids: Vec<String> = serde_json::from_value(original_ids_value.clone()).unwrap();
        let original_ids: std::collections::HashSet<_> = original_ids.into_iter().collect();
        assert!(original_ids.contains(&id_a));
        assert!(original_ids.contains(&id_b));
        assert!(original_ids.contains(&id_c));
    }

    #[tokio::test]
    async fn compress_old_memories_ignores_high_importance_memories() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();

        let id = engine
            .store("An important old memory", MemoryType::Episodic, 0.9)
            .await
            .unwrap();
        backdate(&engine, &id, 20);

        let compressed = engine.compress_old_memories().await.unwrap();
        assert_eq!(compressed, 0);

        let mem = engine.get(&id).await.unwrap().unwrap();
        assert!(mem.superseded_by.is_none());
    }

    #[tokio::test]
    async fn compress_old_memories_ignores_young_memories() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();

        // Not backdated -- still fresh, so should never be picked up by
        // get_episodic_memories_older_than(14) at all.
        let id = engine
            .store("A brand new low-importance episodic memory", MemoryType::Episodic, 0.1)
            .await
            .unwrap();

        let compressed = engine.compress_old_memories().await.unwrap();
        assert_eq!(compressed, 0);

        let mem = engine.get(&id).await.unwrap().unwrap();
        assert!(mem.superseded_by.is_none());
    }

    #[tokio::test]
    async fn compress_old_memories_leaves_clusters_smaller_than_three_uncompressed() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();

        let id_a = engine
            .store("A rare old episodic memory about the weather", MemoryType::Episodic, 0.1)
            .await
            .unwrap();
        let id_b = engine
            .store("A rare old episodic memory about the weather today", MemoryType::Episodic, 0.1)
            .await
            .unwrap();

        backdate(&engine, &id_a, 20);
        backdate(&engine, &id_b, 20);

        let compressed = engine.compress_old_memories().await.unwrap();
        assert_eq!(compressed, 0, "a 2-member cluster is below the >=3 threshold");

        assert!(engine.get(&id_a).await.unwrap().unwrap().superseded_by.is_none());
        assert!(engine.get(&id_b).await.unwrap().unwrap().superseded_by.is_none());
    }

    #[tokio::test]
    async fn compress_old_memories_reports_missing_embedding_as_corruption() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();

        // A healthy candidate so the candidate set isn't empty.
        let healthy_id = engine
            .store("A perfectly normal old episodic memory", MemoryType::Episodic, 0.1)
            .await
            .unwrap();
        backdate(&engine, &healthy_id, 20);

        // A corrupt row: present in `memories`, absent from `embeddings`.
        let orphan_id = insert_orphan_episodic_row(&engine, 0.1, 20);

        let result = engine.compress_old_memories().await;
        let err = result.expect_err("a missing embedding must surface as an error, not be skipped");
        assert!(matches!(err, MemoliteError::Corruption(_)));
        let orphan_id_str = orphan_id.to_string();
        assert!(
            err.to_string().contains(orphan_id_str.as_str()),
            "error should name the specific memory id with no embedding: {err}"
        );
    }

    #[tokio::test]
    async fn compress_old_memories_on_empty_database_returns_zero() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();
        let compressed = engine.compress_old_memories().await.unwrap();
        assert_eq!(compressed, 0);
    }
#[tokio::test]
async fn rebuild_vector_index_prunes_superseded_originals_but_keeps_summary_searchable() {
    let engine = MemoryEngine::open(":memory:").await.unwrap();

    // Near-templated sentences (identical structure, a single word
    // swapped) -- mirrors the login-timeout fixture in
    // `compress_old_memories_folds_similar_old_episodic_memories_into_one_summary`,
    // which is the pattern that reliably clears the 0.85 cosine
    // threshold under a real sentence-embedding model. Looser
    // paraphrases ("prefers dark mode" / "likes dark themes" /
    // "prefers a dark color theme") are semantically equivalent to a
    // person but routinely land in the 0.6-0.84 range with common
    // small embedding models, which is below the clustering
    // threshold and would make this test flaky depending on which
    // embedder backs `Embedder::embed`.
    let id_a = engine
        .store(
            "The user prefers dark mode across all their applications",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();
    let id_b = engine
        .store(
            "The user prefers dark mode across all their apps",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();
    let id_c = engine
        .store(
            "The user prefers dark mode across every application",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();

    for id in [&id_a, &id_b, &id_c] {
        backdate(&engine, id, 20);
    }

    assert_eq!(engine.compress_old_memories().await.unwrap(), 3);

    let summary_id = engine.get(&id_a).await.unwrap().unwrap().superseded_by.unwrap();

    // Before rebuild: compress_old_memories() never touched the
    // vector store's entries for the originals (it only inserted the
    // new summary vector and flipped `superseded_by` in SQLite), so
    // the stale in-memory index still has all four vectors. A
    // superseded-inclusive recall finds all of them.
    let before = engine
        .recall_query(
            RecallQuery::new("dark mode preference across applications")
                .limit(20)
                .include_superseded(true),
        )
        .await
        .unwrap();
    let before_ids: std::collections::HashSet<String> =
        before.items.iter().map(|i| i.memory.id.to_string()).collect();
    assert!(before_ids.contains(&id_a));
    assert!(before_ids.contains(&id_b));
    assert!(before_ids.contains(&id_c));
    assert!(before_ids.contains(&summary_id.to_string()));

    // rebuild_vector_index() runs `reconcile_vector_index` with
    // `BackfillPolicy::ReplaceAll`, whose query is
    // `WHERE superseded_by IS NULL AND (expires_at IS NULL OR
    // expires_at > now)` -- this intentionally excludes superseded
    // (and expired) memories from the rebuilt index, matching what
    // `open()`, `forget()`'s compensation path, and
    // `purge_expired()`'s compensation path already do. So after a
    // rebuild, the three now-superseded originals are no longer
    // *vector-searchable*, even though nothing was deleted from
    // SQLite.
    engine.rebuild_vector_index().await.unwrap();

    let after = engine
        .recall_query(
            RecallQuery::new("dark mode preference across applications")
                .limit(20)
                .include_superseded(true),
        )
        .await
        .unwrap();
    let after_ids: std::collections::HashSet<String> =
        after.items.iter().map(|i| i.memory.id.to_string()).collect();

    assert!(
        after_ids.contains(&summary_id.to_string()),
        "the active summary must remain searchable after a rebuild"
    );
    assert!(
        !after_ids.contains(&id_a) && !after_ids.contains(&id_b) && !after_ids.contains(&id_c),
        "superseded originals are expected to drop out of the rebuilt ANN index"
    );

    // The originals' data is still fully intact in SQLite -- rebuild
    // only affects what's searchable via the vector index, never
    // what `get()`/`find_superseded_chain()` can retrieve directly.
    for id in [&id_a, &id_b, &id_c] {
        let mem = engine.get(id).await.unwrap().expect("original must still exist in SQLite");
        assert_eq!(mem.superseded_by, Some(summary_id));
    }
}
    #[tokio::test]
    async fn get_episodic_memories_older_than_only_returns_old_episodic_rows() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();

        let old_episodic = engine
            .store("old episodic", MemoryType::Episodic, 0.5)
            .await
            .unwrap();
        backdate(&engine, &old_episodic, 20);

        let young_episodic = engine
            .store("young episodic", MemoryType::Episodic, 0.5)
            .await
            .unwrap();
        // left un-backdated: created "now", should not appear.

        let old_semantic = engine
            .store("old semantic", MemoryType::Semantic, 0.5)
            .await
            .unwrap();
        backdate(&engine, &old_semantic, 20);

        let rows = engine.get_episodic_memories_older_than(14).unwrap();
        let ids: std::collections::HashSet<String> =
            rows.iter().map(|m| m.id.to_string()).collect();

        assert!(ids.contains(&old_episodic));
        assert!(!ids.contains(&young_episodic));
        assert!(!ids.contains(&old_semantic));
    }

    #[tokio::test]
    async fn mark_all_superseded_updates_every_id_in_one_transaction() {
        let engine = MemoryEngine::open(":memory:").await.unwrap();

        let id_a = engine.store("a", MemoryType::Episodic, 0.5).await.unwrap();
        let id_b = engine.store("b", MemoryType::Episodic, 0.5).await.unwrap();
        let id_c = engine.store("c", MemoryType::Episodic, 0.5).await.unwrap();
        let new_id = engine.store("summary", MemoryType::Semantic, 0.5).await.unwrap();

        let old_ids: Vec<Uuid> = [&id_a, &id_b, &id_c]
            .iter()
            .map(|s| Uuid::parse_str(s.as_str()).unwrap())
            .collect();

        engine.mark_all_superseded(&old_ids, &new_id).unwrap();

        for id in [&id_a, &id_b, &id_c] {
            let mem = engine.get(id).await.unwrap().unwrap();
            assert_eq!(mem.superseded_by.unwrap().to_string(), new_id);
        }
    }
}