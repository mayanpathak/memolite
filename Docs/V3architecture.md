# Memolite — Final Master Build Plan (v3, repo-verified, fully self-contained)

> **Rule zero, unchanged:** everything already working in `github.com/mayanpathak/memolite` stays
> as-is, with the Step-0 repairs applied first. **This document supersedes v2 entirely** — every
> code block a step needs is written out here, not referenced from an older document (fix #16).
> I re-cloned the repo and re-checked `src/engine.rs` and `src/error.rs` before writing this: the
> struct is still `MemoryEngine { conn: Connection, embedder: Mutex<Embedder> }`, `recall()` is
> still `todo!()`, and `error.rs` still has no `InvalidArgument`/`VectorStore`/`Internal` variant.
> Nothing in the repo changed between v2 and v3 — only the plan did.

v3 fixes all 6 blockers and all 11 correctness/smaller issues raised in the v2 review. A
cross-reference table is at the very end.

---

## Step 0 — Repairs (unchanged from v2, verified still accurate)

### 0.1 — Error variants
**File:** `src/error.rs`. Add:
```rust
#[error("invalid argument: {0}")]
InvalidArgument(String),

#[error("vector store error: {0}")]
VectorStore(String),

#[error("internal error: {0}")]
Internal(String),

/// (v3 fix #8) An operation failed, and the automatic rollback/compensation
/// step that was supposed to clean up after it *also* failed. Both messages
/// are preserved so an operator isn't left guessing which half broke.
#[error("operation failed: {operation}; compensation also failed: {compensation}")]
CompensationFailed { operation: String, compensation: String },
```
No separate `Serialization` variant — `InvalidMetadata(#[from] serde_json::Error)` already covers
`serde_json::to_string(...)?` in both directions.

### 0.2 — Module layout
Decision unchanged: no `src/db/` module. Everything stays on `MemoryEngine` methods using
`self.conn` (or `self.conn.lock()` from M6.5 onward).

### 0.3 — Final engine shape, reached gradually
```rust
pub struct MemoryEngine {
    conn: std::sync::Mutex<rusqlite::Connection>, // Mutex added at M6.5
    embedder: std::sync::Mutex<Embedder>,          // unchanged, already a Mutex today
    vector_store: std::sync::Arc<dyn VectorStore>, // added at M3
}
```
`Arc<dyn VectorStore>`, never `Box` — needs to be shared into background maintenance/streaming
tasks without fighting engine ownership.

### 0.4 — `Uuid` internally, `String` only at true external boundaries (SQL params, HTTP bodies).

### 0.5 — Column-order constant
```rust
/// Canonical column list for `memories`, in the exact order `row_to_memory`
/// expects. Never write `SELECT *` or hand-roll this list.
const MEMORY_COLUMNS: &str =
    "id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata";
// Step 73 (M6) appends ", confidence" here and nowhere else.
```

### 0.6 — (v3 new) `MAX_CANDIDATES` constant, fixes review item #15
**File:** `src/recall.rs`.
```rust
/// Ceiling on how many raw vector-store hits `recall_query` will ever pull
/// before filtering/ranking, regardless of the caller's `limit`. Without a
/// cap, `limit.saturating_mul(5)` on a large `limit` could ask the vector
/// store for an unreasonable number of hits.
pub const MAX_CANDIDATES: usize = 500;

pub fn candidate_pool_size(limit: usize) -> usize {
    limit.saturating_mul(5).max(50).min(MAX_CANDIDATES)
}
```

**Checkpoint 0:** `cargo build && cargo test` green — only new unused error variants exist so far.

---

## Corrected build order (unchanged ordering from v2)

| Order | Milestone | What it adds |
|---|---|---|
| 0 | Step 0 | Error variants, layout decision, column constant, candidate-pool constant |
| 1 | M3 | `VectorStore` trait (+`clear`, +`contains`), `InMemoryVectorStore`, **restart backfill**, wired into `store`/`recall`/`forget` |
| 2 | M4 | Ranking, `RecallQuery`/`RecallItem`/`RecallResult`, metadata-equals filter, capped candidate pool, `recall()` delegates to `recall_query()` |
| 3 | M6 | `ConfidenceLevel`, migration runner that **always** verifies structure |
| 4 | M5 | `StoreRequest`/`MemoryUpdate` |
| 5 | M7 | Temporal querying |
| 6 | M6.5 | `Mutex<Connection>`, `Send + Sync` proof |
| 7 | M8 | Streaming ingestion with **two explicit shutdown modes** |
| 8 | M9 | Compression with **atomic index-swap rebuild** and **dimension/finite validation** |
| 9 | M10 | Maintenance controller: **fallible start, single-controller enforced** |
| 10 | M11 | VecLite backend — **or honestly renamed `generic-http`** if unpinned |
| 11 | M12 | Docs, packaging, user-authorized release |

---

# M3 — VectorStore trait, naive search, and restart durability

### Step 41 — Trait, with `clear()` and `contains()` (fixes review item #5)
**File:** `src/vector_store/mod.rs`.
```rust
use async_trait::async_trait;
use std::collections::HashMap;
use serde_json::Value;
use uuid::Uuid;
use crate::error::Result;

pub mod in_memory;
pub use in_memory::InMemoryVectorStore;

#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: Uuid,
    pub score: f32,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Implementations MUST be idempotent upserts: inserting an id that
    /// already exists replaces its vector/metadata rather than erroring or
    /// duplicating. This is what lets M11's backfill policy be a plain
    /// "insert every active memory" pass with no separate existence check.
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    /// Whether `id` currently exists in the store. Used by restart backfill
    /// to avoid redundant re-inserts on stores where insert is expensive
    /// (e.g. a remote HTTP backend), even though insert is always safe to
    /// repeat because it's an upsert.
    async fn contains(&self, id: Uuid) -> Result<bool>;
    /// Removes every entry. Required by index rebuilding (M9) and
    /// `BackfillPolicy::Rebuild` (M11).
    async fn clear(&self) -> Result<()>;
    fn dimension(&self) -> usize;
}
```
`src/lib.rs`:
```rust
pub mod vector_store;
pub mod math_utils;
pub use vector_store::{VectorStore, VectorHit, InMemoryVectorStore};
```

> **(v3 fix #5)** Review item 5 flagged that `BackfillPolicy::InsertMissing` was unimplementable
> because the trait had no way to check existence. Rather than adding `InsertMissing` as a
> separate code path, this plan makes **every** `insert()` an idempotent upsert (documented above)
> and renames the M11 policy to `UpsertActive`, which just calls `insert()` for every active
> memory unconditionally — simpler than a missing-only diff, and correct by construction. `contains()`
> still exists because M3's restart backfill (Step 44a below) uses it to skip redundant work
> cheaply on the in-memory backend.

### Step 42 — `src/math_utils.rs` (unchanged from v2)
```rust
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { return 0.0; }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identical_vectors_score_one() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }
    #[test]
    fn zero_vector_scores_zero_not_nan() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
```

### Step 43 — `InMemoryVectorStore`, with `contains()` added
**File:** `src/vector_store/in_memory.rs`.
```rust
use std::collections::HashMap;
use std::sync::RwLock;
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;
use crate::error::{MemoliteError, Result};
use crate::math_utils::cosine_similarity;
use super::{VectorStore, VectorHit};

pub struct InMemoryVectorStore {
    data: RwLock<HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>,
    dim: usize,
}

impl InMemoryVectorStore {
    pub fn new(dim: usize) -> Self {
        Self { data: RwLock::new(HashMap::new()), dim }
    }

    fn check_dim(&self, v: &[f32]) -> Result<()> {
        if v.len() != self.dim {
            return Err(MemoliteError::VectorStore(format!(
                "vector length {} does not match store dimension {}", v.len(), self.dim
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        self.check_dim(vector)?;
        let mut guard = self.data.write()
            .map_err(|_| MemoliteError::VectorStore("vector-store lock poisoned".into()))?;
        guard.insert(id, (vector.to_vec(), metadata)); // upsert: overwrite if present
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        self.check_dim(query)?;
        let guard = self.data.read()
            .map_err(|_| MemoliteError::VectorStore("vector-store lock poisoned".into()))?;
        let mut scored: Vec<VectorHit> = guard
            .iter()
            .map(|(id, (vec, _))| VectorHit { id: *id, score: cosine_similarity(query, vec) })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        scored.truncate(k);
        Ok(scored)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let mut guard = self.data.write()
            .map_err(|_| MemoliteError::VectorStore("vector-store lock poisoned".into()))?;
        guard.remove(&id);
        Ok(())
    }

    async fn contains(&self, id: Uuid) -> Result<bool> {
        let guard = self.data.read()
            .map_err(|_| MemoliteError::VectorStore("vector-store lock poisoned".into()))?;
        Ok(guard.contains_key(&id))
    }

    async fn clear(&self) -> Result<()> {
        let mut guard = self.data.write()
            .map_err(|_| MemoliteError::VectorStore("vector-store lock poisoned".into()))?;
        guard.clear();
        Ok(())
    }

    fn dimension(&self) -> usize { self.dim }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nearest_vector_comes_back_first() {
        let store = InMemoryVectorStore::new(3);
        let far = Uuid::new_v4();
        let close = Uuid::new_v4();
        store.insert(far, &[0.0, 1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert(close, &[1.0, 0.0, 0.0], HashMap::new()).await.unwrap();
        let hits = store.search(&[0.9, 0.1, 0.0], 1).await.unwrap();
        assert_eq!(hits[0].id, close);
    }

    #[tokio::test]
    async fn wrong_dimension_is_rejected_not_panicked() {
        let store = InMemoryVectorStore::new(3);
        let result = store.insert(Uuid::new_v4(), &[1.0, 0.0], HashMap::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn insert_is_an_upsert() {
        let store = InMemoryVectorStore::new(2);
        let id = Uuid::new_v4();
        store.insert(id, &[1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert(id, &[0.0, 1.0], HashMap::new()).await.unwrap();
        assert!(store.contains(id).await.unwrap());
        let hits = store.search(&[0.0, 1.0], 1).await.unwrap();
        assert!((hits[0].score - 1.0).abs() < 1e-6); // proves the second insert overwrote, not duplicated
    }
}
```

### Step 44 — `MemoryEngine` owns `vector_store: Arc<dyn VectorStore>`
```rust
pub struct MemoryEngine {
    conn: Connection,
    embedder: Mutex<Embedder>,
    vector_store: std::sync::Arc<dyn VectorStore>,
}
```

### Step 44a — (v3 new, fixes review blocker #1) Restart backfill, run inside `open()`
**File:** `src/engine.rs`. This is the single biggest correctness gap in v2: an
`InMemoryVectorStore` starts empty every process restart, so a database with persisted memories
and embeddings would silently return zero `recall()` results after a restart. `open()` now always
calls this after constructing the vector store, **not just when using VecLite** — every backend
needs it.

```rust
/// Loads every active (not superseded, not expired) memory's embedding from
/// SQLite into `store`. Called once at the end of `open()`/`open_with_store()`,
/// before the engine is handed back to the caller — so the very first
/// `recall()` after a restart already works.
async fn backfill_active_vectors(conn: &Connection, store: &std::sync::Arc<dyn VectorStore>) -> Result<()> {
    let now = Utc::now().timestamp();
    let mut stmt = conn.prepare(
        "SELECT m.id, e.vector, e.dimension, m.metadata
         FROM memories m
         JOIN embeddings e ON e.memory_id = m.id
         WHERE m.superseded_by IS NULL
           AND (m.expires_at IS NULL OR m.expires_at >= ?1)"
    )?;
    let rows = stmt.query_map(rusqlite::params![now], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    for row in rows {
        let (id_str, bytes, stored_dim, metadata_json) = row?;
        let id = Uuid::parse_str(&id_str)?;
        let vector: Vec<f32> = bincode::deserialize(&bytes)
            .map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;

        // (v3 fix #10, applied here too) validate dimension + finiteness before trusting the row.
        if stored_dim as usize != store.dimension() || vector.len() != store.dimension() {
            return Err(MemoliteError::VectorStore(format!(
                "stored vector for {id} has dimension {} but store expects {}",
                vector.len(), store.dimension()
            )));
        }
        if !vector.iter().all(|x| x.is_finite()) {
            return Err(MemoliteError::VectorStore(format!(
                "stored vector for {id} contains a non-finite value"
            )));
        }

        let metadata: HashMap<String, Value> = serde_json::from_str(&metadata_json)?;
        store.insert(id, &vector, metadata).await?; // upsert, safe to call unconditionally
    }
    Ok(())
}
```
`open()` becomes: construct `conn`, run migrations (M6's Step 72), construct
`Arc::new(InMemoryVectorStore::new(embedder.dimension()))`, then
`backfill_active_vectors(&conn, &vector_store).await?` before returning `Self { .. }`.

### Step 45 — `store()` also inserts into the vector store (unchanged compensation pattern)
```rust
pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String> {
    // ...existing validation, id/timestamp generation, embed-before-write...
    // 1. Commit the memory row + embedding row in one SQLite transaction.
    // 2. Only after that commit succeeds:
    if let Err(e) = self.vector_store.insert(id, &vector, HashMap::new()).await {
        if let Err(compensation_err) = self.conn.execute(
            "DELETE FROM memories WHERE id = ?1", rusqlite::params![id.to_string()]
        ) {
            // (v3 fix #8) don't swallow a failed compensation — surface both.
            return Err(MemoliteError::CompensationFailed {
                operation: e.to_string(),
                compensation: compensation_err.to_string(),
            });
        }
        return Err(e);
    }
    Ok(id.to_string())
}
```

### Step 46 — `recall(query: &str)` (kept only as the M3-era stub; M4 makes it delegate — Step 63a)
```rust
pub async fn recall(&self, query_text: &str) -> Result<Vec<Memory>> {
    if query_text.trim().is_empty() {
        return Err(MemoliteError::InvalidArgument("query_text must not be empty".into()));
    }
    let query_vec = {
        let mut embedder = self.embedder.lock()
            .map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(query_text)?
    };
    let hits = self.vector_store.search(&query_vec, 20).await?;
    if hits.is_empty() { return Ok(Vec::new()); }
    let mut results = Vec::new();
    for hit in hits {
        if let Some(mem) = self.get(&hit.id.to_string()).await? { results.push(mem); }
    }
    Ok(results)
}
```

### Step 47 — Bump `access_count`/`last_accessed` on recall (unchanged; superseded by M6's Step 75)
```rust
fn update_access_stats(&self, id: Uuid) -> Result<()> {
    self.conn.execute(
        "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp(), id.to_string()],
    )?;
    Ok(())
}
```

### Step 48 — (v3 fix #7) `forget()` with an explicit, exact reconciliation rule
Review item 7 said the two-store consistency rule for `forget()`/`purge` must be exact, not
"document whichever." **Final rule, used by both `forget()` and `purge_expired()`:**
1. Delete from SQLite first (it's the source of truth).
2. Attempt vector-store deletion.
3. If it fails, attempt `rebuild_active_vector_index()` (Step 150, atomic-swap version) to
   reconcile.
4. Return the *original* deletion error if rebuild succeeded; return `CompensationFailed` with
   both errors if rebuild also failed. Either way, the SQLite delete is never rolled back — a
   stale vector-store entry is a wasted candidate, not a correctness bug, because `get()` already
   filters out anything no longer in SQLite (Step 46's `if let Some(mem) = self.get(...)`).

```rust
pub async fn forget(&self, id: &str) -> Result<()> {
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    self.conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])?;

    if let Err(e) = self.vector_store.delete(uuid).await {
        if let Err(rebuild_err) = self.rebuild_active_vector_index().await {
            return Err(MemoliteError::CompensationFailed {
                operation: e.to_string(),
                compensation: rebuild_err.to_string(),
            });
        }
        return Err(e);
    }
    Ok(())
}
```
(`rebuild_active_vector_index()` is defined once, in M9's Step 150, and reused here — no
duplicate rebuild logic.)

### Steps 49–56 — Tests + checkpoint
49. Store 3 unrelated facts + 1 relevant one, `recall()`, assert the relevant one is present.
50. `recall()` on an empty engine returns `Ok(vec![])`.
51. `access_count` increases by exactly 1 after one `recall()` call.
52. `forget()` removes the memory from both SQLite and `vector_store`.
53. **(v3 new, fixes blocker #1)** Restart test: open an engine, `store()` 3 memories, close it
    (drop), `open()` the *same* database path again, call `recall()` immediately — assert all 3
    are findable without calling `store()` again. This is the exact regression the v2 review
    caught; it is now tested at M3, not deferred to M9.
54. **(v3 new)** Corrupt-row restart test: manually write a garbage (non-bincode) blob into one
    `embeddings` row, then `open()` — assert `open()` returns `Err`, not a silently-partial index
    (loud failure, matching the compression philosophy from M9).
55. `cargo clippy` clean. `cargo fmt`.
56. **Checkpoint:** `cargo test` green; `recall()` is real and survives a restart.

---

# M4 — Ranking + corrected recall API

### Step 57 — `src/ranking.rs` (unchanged from v2; clamped recency, confidence-ready signature)
```rust
use crate::memory::MemoryType;

pub fn decay_half_life_days(t: MemoryType) -> f64 {
    match t {
        MemoryType::Episodic => 14.0,
        MemoryType::Semantic => 693.0,
        MemoryType::Procedural => 1386.0,
        MemoryType::Working => 0.17,
    }
}

pub fn recency_factor(days_since_access: f64, memory_type: MemoryType) -> f32 {
    let days = days_since_access.max(0.0); // fix #27: clamp against clock skew
    let half_life = decay_half_life_days(memory_type);
    let decay_rate = std::f64::consts::LN_2 / half_life;
    (-decay_rate * days).exp() as f32
}

pub fn reinforcement_factor(access_count: u32) -> f32 {
    1.0 + ((1.0 + access_count as f32).ln()) * 0.1
}

pub fn final_score(similarity: f32, importance: f32, recency: f32, reinforcement: f32, confidence_weight: f32) -> f32 {
    similarity * importance * recency * reinforcement * confidence_weight
}
```

### Steps 58–63 — Wire ranking into `recall_query()` (see Step 64 for the full body)
Candidate pool size now uses `recall::candidate_pool_size(limit)` from Step 0.6, never a raw
`limit * 5` (fixes review item #15).

### Step 63a — (v3 fix #14) `recall()` delegates to `recall_query()` instead of drifting
Review item 14: keeping the M3 `recall()` body permanently alongside `recall_query()` would let
the two silently diverge in filtering/ordering behavior. **Decision:** once `recall_query()`
exists, `recall()`'s M3 body is deleted and replaced with a thin delegation, so there is exactly
one implementation of "what counts as a relevant memory":
```rust
pub async fn recall(&self, query_text: &str) -> Result<Vec<Memory>> {
    Ok(self
        .recall_query(RecallQuery::new(query_text))
        .await?
        .items
        .into_iter()
        .map(|item| item.memory)
        .collect())
}
```

### Step 64 — `RecallQuery` (unchanged shape) + full `recall_query()` body
**File:** `src/recall.rs`.
```rust
use crate::memory::{Memory, MemoryType};
use crate::error::{MemoliteError, Result};
use std::collections::HashMap;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub query_text: String,
    pub limit: usize,
    pub min_importance: f32,
    pub memory_types: Option<Vec<MemoryType>>,
    pub include_expired: bool,
    pub include_superseded: bool,
    pub metadata_equals: HashMap<String, Value>,
}

impl RecallQuery {
    pub fn new(text: &str) -> Self {
        Self {
            query_text: text.to_string(),
            limit: 5,
            min_importance: 0.0,
            memory_types: None,
            include_expired: false,
            include_superseded: false,
            metadata_equals: HashMap::new(),
        }
    }
    pub fn limit(mut self, n: usize) -> Self { self.limit = n; self }
    pub fn min_importance(mut self, x: f32) -> Self { self.min_importance = x; self }
    pub fn memory_types(mut self, t: Vec<MemoryType>) -> Self { self.memory_types = Some(t); self }
    pub fn include_expired(mut self, b: bool) -> Self { self.include_expired = b; self }
    pub fn include_superseded(mut self, b: bool) -> Self { self.include_superseded = b; self }
    pub fn metadata_equals(mut self, key: &str, value: Value) -> Self {
        self.metadata_equals.insert(key.to_string(), value); self
    }
}

pub struct RecallItem { pub memory: Memory, pub similarity: f32, pub score: f32 }
pub struct RecallResult { pub items: Vec<RecallItem> }
```

On `MemoryEngine`:
```rust
pub async fn recall_query(&self, query: RecallQuery) -> Result<RecallResult> {
    if query.limit == 0 {
        return Err(MemoliteError::InvalidArgument("limit must be > 0".into()));
    }
    if !query.min_importance.is_finite() {
        return Err(MemoliteError::InvalidArgument("min_importance must be finite".into()));
    }
    if query.query_text.trim().is_empty() {
        return Err(MemoliteError::InvalidArgument("query_text must not be empty".into()));
    }

    let query_vec = {
        let mut embedder = self.embedder.lock()
            .map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(&query.query_text)?
    };

    let pool_size = crate::recall::candidate_pool_size(query.limit); // fix #15: capped, no raw *5
    let hits = self.vector_store.search(&query_vec, pool_size).await?;

    let now = Utc::now();
    let mut scored = Vec::with_capacity(hits.len());
    for hit in hits {
        let Some(memory) = self.get(&hit.id.to_string()).await? else { continue };

        // Filters (fix #13: metadata_equals is now actually applied)
        if memory.importance < query.min_importance { continue; }
        if let Some(types) = &query.memory_types {
            if !types.contains(&memory.memory_type) { continue; }
        }
        if memory.superseded_by.is_some() && !query.include_superseded { continue; }
        let expired = memory.expires_at.map(|e| e < now).unwrap_or(false);
        if expired && !query.include_expired { continue; }
        if !query.metadata_equals.iter().all(|(k, v)| memory.metadata.get(k) == Some(v)) { continue; }

        let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
        let recency = ranking::recency_factor(days_since_access, memory.memory_type);
        let reinforcement = ranking::reinforcement_factor(memory.access_count);
        let confidence_weight = memory.confidence.weight(); // 1.0 until M6 lands; real after
        let score = ranking::final_score(hit.score, memory.importance, recency, reinforcement, confidence_weight);

        scored.push(RecallItem { memory, similarity: hit.score, score });
    }

    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.memory.id.cmp(&b.memory.id)));
    scored.truncate(query.limit);

    for item in &scored {
        self.update_access_stats_and_maybe_promote(item.memory.id)?; // Step 75 once M6 lands; Step 47 before that
    }

    Ok(RecallResult { items: scored })
}

pub async fn recall_text(&self, text: &str) -> Result<RecallResult> {
    self.recall_query(RecallQuery::new(text)).await
}
```

### Step 68 — `RecallResult::as_prompt_context`, fixing overflow (v3 fix #3)
Review item 3: the header was always appended even when smaller than `max_chars`, and the
truncation marker itself wasn't budget-checked, so the total could exceed `max_chars`. Fixed with
a single character-aware append helper that every piece of text goes through:
```rust
impl RecallResult {
    /// Bounded rendering for prompt injection. This delimits untrusted memory
    /// content with a numbered list; it is not a prompt-injection sanitizer.
    /// The returned string's character count never exceeds `max_chars`,
    /// including the header and any truncation marker.
    pub fn as_prompt_context(&self, max_chars: usize) -> String {
        let mut out = String::new();
        let mut used = 0usize;

        // Every piece of text — header, item lines, truncation marker — goes
        // through this one gate, so nothing can ever push past max_chars.
        let mut try_append = |out: &mut String, used: &mut usize, text: &str| -> bool {
            let chars = text.chars().count();
            if *used + chars > max_chars { return false; }
            out.push_str(text);
            *used += chars;
            true
        };

        if !try_append(&mut out, &mut used, "Relevant memories:\n") {
            // max_chars is smaller than the header itself — return as much of
            // the header as fits, character-exact, rather than nothing.
            return "Relevant memories:\n".chars().take(max_chars).collect();
        }

        for (i, item) in self.items.iter().enumerate() {
            let escaped = item.memory.content.replace('\n', " ").replace("---", "- - -");
            let line = format!("{}. [{:.2}] {}\n", i + 1, item.score, escaped);
            if !try_append(&mut out, &mut used, &line) {
                // Only add the marker if it actually fits in what's left.
                try_append(&mut out, &mut used, "...[truncated, budget reached]\n");
                break;
            }
        }
        out
    }
}
```

### Steps 69–72 — Tests + checkpoint
69. `.memory_types(...)` filter tests (unchanged from v2).
70. **(v3 new, fix #13)** `.metadata_equals("project", json!("memolite"))` excludes memories whose
    metadata doesn't have that exact key/value; includes ones that do.
71. **(v3 new, fix #3)** `as_prompt_context` budget tests at `max_chars` = 0, 1, exactly the header
    length, header length + 1, and a size that lands mid-truncation-marker — assert
    `result.chars().count() <= max_chars` in every case, including the smallest ones.
72. **Checkpoint:** `limit(0)` → `Err`. NaN `min_importance` → `Err`. `recall()` and
    `recall_query()` agree on which memories come back for an identical query. `cargo test` green.

---

# M6 — Confidence scoring (before M5)

### Step 73 — `ConfidenceLevel` (unchanged from v2)
```rust
use serde::{Serialize, Deserialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceLevel { Explicit, Inferred, Reinforced }

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid confidence value: {0}")]
pub struct InvalidConfidence(pub String);

impl ConfidenceLevel {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Explicit => "explicit", Self::Inferred => "inferred", Self::Reinforced => "reinforced" }
    }
    pub fn parse_str(s: &str) -> Result<Self, InvalidConfidence> {
        match s {
            "explicit" => Ok(Self::Explicit),
            "inferred" => Ok(Self::Inferred),
            "reinforced" => Ok(Self::Reinforced),
            other => Err(InvalidConfidence(other.to_string())),
        }
    }
    pub fn weight(&self) -> f32 {
        match self { Self::Explicit | Self::Reinforced => 1.0, Self::Inferred => 0.7 }
    }
    pub fn maybe_promote(self, access_count: u32) -> Self {
        if self == Self::Inferred && access_count >= 5 { Self::Reinforced } else { self }
    }
}
impl fmt::Display for ConfidenceLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.as_str()) }
}
impl FromStr for ConfidenceLevel {
    type Err = InvalidConfidence;
    fn from_str(s: &str) -> Result<Self, Self::Err> { Self::parse_str(s) }
}
```

### Step 74 — Migration runner that **always verifies structure** (v3 fix #2)
Review item 2: v2's runner only ran the per-table checks *inside* `if !applied.contains(&1)`, so
a database with `schema_migrations` already recording version 1 but missing `embeddings` (test
78's exact fixture) would skip the repair entirely. **v3 policy: repairing, not strict** — table
and column existence checks run unconditionally on every `open()`, independent of what
`schema_migrations` says, because verifying `pragma_table_info` is cheap and this is the only way
to make partial/manually-edited databases self-heal instead of erroring out.

```rust
fn run_migrations(conn: &mut Connection) -> Result<()> {
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )", [],
    )?;

    fn has_table(tx: &rusqlite::Transaction, name: &str) -> Result<bool> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![name], |r| r.get(0),
        )?;
        Ok(count > 0)
    }
    fn has_column(tx: &rusqlite::Transaction, table: &str, col: &str) -> Result<bool> {
        let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
        let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(cols.iter().any(|c| c == col))
    }
    fn record(tx: &rusqlite::Transaction, version: i64) -> Result<()> {
        tx.execute(
            "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, Utc::now().timestamp()],
        )?;
        Ok(())
    }

    // Migration 1: baseline tables — checked and repaired unconditionally,
    // whether or not schema_migrations already claims version 1 is applied.
    {
        let tx = conn.transaction()?;
        if !has_table(&tx, "memories")? { tx.execute_batch(MIGRATION_1_MEMORIES)?; }
        if !has_table(&tx, "embeddings")? { tx.execute_batch(MIGRATION_1_EMBEDDINGS)?; }
        record(&tx, 1)?;
        tx.commit()?;
    }

    // Migration 2: confidence column + indexes — same unconditional check.
    {
        let tx = conn.transaction()?;
        if !has_column(&tx, "memories", "confidence")? { tx.execute_batch(MIGRATION_2_CONFIDENCE)?; }
        tx.execute_batch(MIGRATION_2_INDEXES)?; // CREATE INDEX IF NOT EXISTS is always safe
        record(&tx, 2)?;
        tx.commit()?;
    }

    Ok(())
}

const MIGRATION_1_MEMORIES: &str = "
    CREATE TABLE memories (
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
    );";
const MIGRATION_1_EMBEDDINGS: &str = "
    CREATE TABLE embeddings (
        memory_id   TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
        vector      BLOB NOT NULL,
        dimension   INTEGER NOT NULL
    );";
const MIGRATION_2_CONFIDENCE: &str = "
    ALTER TABLE memories ADD COLUMN confidence TEXT NOT NULL DEFAULT 'explicit'
        CHECK(confidence IN ('explicit', 'inferred', 'reinforced'));";
const MIGRATION_2_INDEXES: &str = "
    CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
    CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
    CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type);
    CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at);
    CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);
";
```
`open()`'s existing inline `CREATE TABLE IF NOT EXISTS` block is deleted; `run_migrations(&mut
conn)?` is the only schema owner, followed by `backfill_active_vectors` (Step 44a).

### Step 75 — Add `confidence` to `Memory`, append to `MEMORY_COLUMNS`
Unchanged from v2: `Memory.confidence: ConfidenceLevel`; `MEMORY_COLUMNS` (Step 0.5) becomes
`"... , confidence"`; `row_to_memory` reads column index 10 via `ConfidenceLevel::parse_str`.

### Step 76 — Weight confidence into ranking (unchanged — `recall_query` already calls
`memory.confidence.weight()`, see Step 64).

### Step 77 — Atomic bump-and-promote (replaces Step 47)
```rust
fn update_access_stats_and_maybe_promote(&self, id: Uuid) -> Result<()> {
    self.conn.execute(
        "UPDATE memories
         SET access_count = access_count + 1,
             last_accessed = ?1,
             confidence = CASE
                 WHEN confidence = 'inferred' AND access_count + 1 >= 5 THEN 'reinforced'
                 ELSE confidence
             END
         WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp(), id.to_string()],
    )?;
    Ok(())
}
```

### Steps 78–86 — Tests + checkpoint
78. **(v3 fix #2)** Exact repro of the review's failing fixture: create `memories` manually,
    insert a `schema_migrations` row for version 1, do **not** create `embeddings`. Run
    `run_migrations` — assert `embeddings` now exists. This now passes because the check is
    unconditional, not gated on `!applied.contains(&1)`.
79. Idempotent reopen: run `run_migrations` twice, confirm no error, exactly one row per version
    in `schema_migrations` (the `INSERT OR IGNORE` prevents duplicates).
80. Manually add `confidence` out-of-band (no `schema_migrations` row at all) — confirm migration
    2 does not attempt a duplicate `ALTER TABLE` and does not error.
81. Fresh empty database — confirm both migrations apply once, in order, and the file lands at
    exactly the expected schema.
82. Round-trip: store with each `ConfidenceLevel`, `get()` back, confirm equality.
83. `Inferred` scores lower than an otherwise-identical `Explicit` memory in `recall_query()`.
84. Recalling an `Inferred` memory exactly 5 times promotes it to `Reinforced` at count 5.
85. `cargo clippy`/`cargo fmt` clean.
86. **Checkpoint:** confidence persisted, ranked, promoted correctly; migration runner self-heals
    any partial/manually-edited schema without double-applying anything. `cargo test` green.

---

# M5 — StoreRequest / MemoryUpdate

### Step 87 — `StoreRequest`
**File:** `src/requests.rs`.
```rust
use crate::memory::MemoryType;
use crate::confidence::ConfidenceLevel;
use std::collections::HashMap;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct StoreRequest {
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
    pub custom_ttl: Option<chrono::Duration>,
    pub metadata: HashMap<String, Value>,
    pub confidence: ConfidenceLevel,
}

impl StoreRequest {
    pub fn new(content: &str, memory_type: MemoryType, importance: f32) -> Self {
        Self {
            content: content.to_string(),
            memory_type,
            importance,
            custom_ttl: None,
            metadata: HashMap::new(),
            confidence: ConfidenceLevel::Explicit,
        }
    }
    pub fn ttl(mut self, d: chrono::Duration) -> Self { self.custom_ttl = Some(d); self }
    pub fn metadata(mut self, m: HashMap<String, Value>) -> Self { self.metadata = m; self }
    pub fn with_confidence(mut self, c: ConfidenceLevel) -> Self { self.confidence = c; self }
}
```

### Step 88 — `MemoryUpdate`, TTL carry-forward (unchanged reasoning from v2)
```rust
#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
    pub new_content: Option<String>,
    pub new_importance: Option<f32>,
    pub new_metadata: Option<HashMap<String, Value>>,
    pub new_memory_type: Option<MemoryType>,
    pub new_ttl: Option<chrono::Duration>,
    pub new_confidence: Option<ConfidenceLevel>,
}
impl MemoryUpdate {
    pub fn content(mut self, c: &str) -> Self { self.new_content = Some(c.to_string()); self }
    pub fn importance(mut self, i: f32) -> Self { self.new_importance = Some(i); self }
    pub fn metadata(mut self, m: HashMap<String, Value>) -> Self { self.new_metadata = Some(m); self }
    pub fn memory_type(mut self, t: MemoryType) -> Self { self.new_memory_type = Some(t); self }
    pub fn ttl(mut self, d: chrono::Duration) -> Self { self.new_ttl = Some(d); self }
    pub fn confidence(mut self, c: ConfidenceLevel) -> Self { self.new_confidence = Some(c); self }
}
```
`id` stays immutable — no `new_id` field.

### Step 89 — Register module: `pub mod requests; pub use requests::{StoreRequest, MemoryUpdate};`

### Step 90 — `store_with_options()`
```rust
pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String> {
    self.store_with_options(StoreRequest::new(content, memory_type, importance)).await
}

pub async fn store_with_options(&self, request: StoreRequest) -> Result<String> {
    if let Some(ttl) = request.custom_ttl {
        if ttl <= chrono::Duration::zero() {
            return Err(MemoliteError::InvalidArgument("custom_ttl must be positive".into()));
        }
    }
    // id, timestamps, metadata_json via serde_json::to_string(...)? (InvalidMetadata #[from]),
    // embed-before-write, single SQLite tx for memory+embedding rows, then:
    if let Err(e) = self.vector_store.insert(id, &vector, request.metadata.clone()).await {
        if let Err(compensation_err) = self.conn.execute(
            "DELETE FROM memories WHERE id = ?1", rusqlite::params![id.to_string()]
        ) {
            return Err(MemoliteError::CompensationFailed {
                operation: e.to_string(), compensation: compensation_err.to_string(),
            });
        }
        return Err(e);
    }
    Ok(id.to_string())
}
```

### Step 91 — `update()`, TTL carry-forward, explicit confidence rule, compensation
```rust
pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<String> {
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    let old = self.get(id).await?.ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;

    let mut request = StoreRequest::new(
        &update.new_content.unwrap_or_else(|| old.content.clone()),
        update.new_memory_type.unwrap_or(old.memory_type),
        update.new_importance.unwrap_or(old.importance),
    );

    request.custom_ttl = update.new_ttl.or_else(|| {
        old.expires_at.and_then(|expiry| {
            let remaining = expiry.signed_duration_since(Utc::now());
            (remaining > chrono::Duration::zero()).then_some(remaining)
        })
    });
    request.metadata = update.new_metadata.unwrap_or_else(|| old.metadata.clone());
    // Rule: a revision defaults to Inferred unless the caller explicitly sets
    // .confidence(...) — a rewrite is not automatically re-asserted as
    // directly-witnessed truth. No implicit "preserve old" default.
    request.confidence = update.new_confidence.unwrap_or(ConfidenceLevel::Inferred);

    let new_id = self.store_with_options(request).await?;
    let new_uuid = Uuid::parse_str(&new_id).unwrap_or(uuid); // fix #17: fall back rather than expect()-panic

    if let Err(e) = self.mark_superseded(&uuid, &new_id) {
        let del_err = self.conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![new_id]).err();
        let vec_err = self.vector_store.delete(new_uuid).await.err();
        if del_err.is_some() || vec_err.is_some() {
            return Err(MemoliteError::CompensationFailed {
                operation: e.to_string(),
                compensation: format!("{:?} / {:?}", del_err, vec_err),
            });
        }
        return Err(e);
    }
    Ok(new_id)
}

fn mark_superseded(&self, old_id: &Uuid, new_id: &str) -> Result<()> {
    let affected = self.conn.execute(
        "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
        rusqlite::params![new_id, old_id.to_string()],
    )?;
    if affected == 0 { return Err(MemoliteError::NotFound(old_id.to_string())); }
    Ok(())
}
```
> **(v3 fix #17)** `Uuid::parse_str(&new_id).expect("just generated")` is replaced with a
> fallback to the already-known `uuid` rather than a panic path, even though the panic was
> logically unreachable — the plan's own stated posture is "no panics," so no exception is carved
> out here.

### Steps 92–108 — Tests + checkpoint
92–95. Basic `store_with_options`/round-trip tests (content, TTL, metadata, confidence default).
96. `update()` with `new_metadata: None` preserves the *old* metadata exactly.
97. `update()` with only `.content(...)`: memory_type unchanged, TTL carried forward as
    *remaining* lifetime (not reset), confidence becomes exactly `Inferred`.
98. `update()` on a memory with no `expires_at` and no explicit `.ttl(...)`: new memory also has
    no expiry (not a default TTL invented from nothing).
99. `update()` on an already-nearly-expired memory: the replacement's remaining TTL is
    approximately what was left on the original, not a fresh full-length TTL.
100. Compensation test: force `mark_superseded` to fail (e.g. delete the old row out from under it
     first), assert the replacement is gone from *both* SQLite and the vector store, and the
     returned error is the `NotFound`/`CompensationFailed` as appropriate.
101–107. `custom_ttl <= 0` → `Err`; `update()` on a nonexistent id → `Err(NotFound)`; superseded
     chain (`A -> B -> C`) resolves correctly; `id` cannot be changed via any `MemoryUpdate`
     builder method (there is none).
108. **Checkpoint:** `cargo test` green, zero regressions M3–M6.

---

# M7 — Temporal querying

- All ID fields (`ChangeRecord.old_id`, `find_superseded_original_id` return type, etc.) are
  `Uuid`, never `String`.
- `query_by_time_range` selects using `MEMORY_COLUMNS` (post-Step-75, includes `confidence`), in
  the exact order `row_to_memory` expects — never `SELECT *`.
- Backdating for tests is done by writing `created_at`/`last_accessed` directly via a test-only
  SQLite connection, never a real `sleep`.
```rust
pub async fn query_by_time_range(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Memory>> {
    if start > end {
        return Err(MemoliteError::InvalidArgument("start must not be after end".into()));
    }
    let sql = format!(
        "SELECT {MEMORY_COLUMNS} FROM memories WHERE created_at >= ?1 AND created_at <= ?2 ORDER BY created_at ASC"
    );
    let mut stmt = self.conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![start.timestamp(), end.timestamp()], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub async fn find_superseded_chain(&self, id: &str) -> Result<Vec<Memory>> {
    // Walk `superseded_by` forward from `id` until it's None; return the full chain in order.
    // Uuid throughout; loop-guard against a cycle (shouldn't happen, but never trust old data)
    // by capping iterations at, e.g., 10_000 and returning Err(Internal(...)) if exceeded.
    ...
}
```

### Tests + checkpoint
- `start > end` → `Err`.
- Backdated fixtures return in the correct chronological order.
- A superseded chain of length 3 resolves head-to-tail correctly and terminates.
- A synthetic cyclic `superseded_by` (constructed directly in test SQL, never producible via the
  public API) does not hang — the iteration cap trips and returns `Err`.
- **Checkpoint:** `cargo test` green through M7.

---

# M6.5 — Concurrency refactor

### Step — `Mutex<Connection>`, `embedder` stays `Mutex<Embedder>`
```rust
pub struct MemoryEngine {
    conn: std::sync::Mutex<rusqlite::Connection>,
    embedder: std::sync::Mutex<Embedder>,
    vector_store: std::sync::Arc<dyn VectorStore>,
}
```
Every `self.conn.execute(...)`/`query_row(...)` call site now first does
`self.conn.lock().map_err(|_| MemoliteError::Internal("connection mutex poisoned".into()))?`, and
the guard is dropped before any `.await` in the same scope.

### Compile-time `Send + Sync` gate
```rust
fn assert_send_sync<T: Send + Sync>() {}
#[test]
fn memory_engine_is_send_sync() { assert_send_sync::<memolite::MemoryEngine>(); }
```

### Tests + checkpoint
- Audit every `.lock()` site for no guard held across `.await` (this is also what `cargo clippy`
  flags).
- `cargo clippy` clean.
- **Checkpoint:** `Send + Sync` proven; no lock held across an await.

---

# M8 — Streaming ingestion, with two explicit shutdown modes (v3 fix #4)

### `IngestChunk` (unchanged)
```rust
#[derive(Debug, Clone)]
pub struct IngestChunk {
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
}
```

### `StreamIngestor` — corrected: `select!` no longer races a live backlog against cancellation
Review item 4's root cause: `tokio::select! { biased; _ = cancel.cancelled() => break, ... }`
means once cancellation fires, it wins on *every* subsequent poll — including over messages
already sitting in the channel buffer — so "cancel, then the 5 already-queued messages still
land" was never actually guaranteed. **v3 fix: two distinct, honestly-named operations, and the
loop for each is structured so its behavior matches its name exactly.**

```rust
use tokio::sync::mpsc::{self, Sender};
use tokio_util::sync::CancellationToken;
use tokio::task::JoinHandle;
use std::sync::Arc;
use crate::engine::MemoryEngine;
use crate::requests::StoreRequest;
use crate::error::{MemoliteError, Result};

#[derive(Debug, Default)]
pub struct IngestReport {
    pub received: usize,
    pub stored: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

pub struct StreamIngestor {
    sender: Sender<IngestChunk>,
    cancel: CancellationToken,
    handle: JoinHandle<Result<IngestReport>>,
}

impl StreamIngestor {
    pub fn spawn(engine: Arc<MemoryEngine>, buffer_size: usize) -> Result<Self> {
        if buffer_size == 0 {
            return Err(MemoliteError::InvalidArgument("buffer_size must be > 0".into()));
        }
        let (tx, mut rx) = mpsc::channel::<IngestChunk>(buffer_size);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            let mut report = IngestReport::default();
            loop {
                // Not `biased`, and cancellation is checked only *between*
                // iterations, not raced against a message that's already
                // arrived — so a message that's already in-flight through
                // rx.recv() always finishes being processed.
                if cancel_clone.is_cancelled() {
                    break;
                }
                match rx.recv().await {
                    Some(chunk) => {
                        report.received += 1;
                        let request = StoreRequest::new(&chunk.content, chunk.memory_type, chunk.importance);
                        match engine.store_with_options(request).await {
                            Ok(_) => report.stored += 1,
                            Err(e) => {
                                report.failed += 1;
                                let preview: String = chunk.content.chars().take(60).collect();
                                report.errors.push((preview, e.to_string()));
                            }
                        }
                    }
                    None => break, // all senders dropped: channel drained naturally
                }
            }
            Ok(report)
        });

        Ok(Self { sender: tx, cancel, handle })
    }

    pub fn sender(&self) -> Sender<IngestChunk> { self.sender.clone() }

    /// Cancels immediately. Any chunk currently mid-`store_with_options()`
    /// finishes; the queued backlog behind it is NOT drained. Use this for
    /// "stop now, I don't care about the backlog" shutdown.
    pub async fn shutdown_now(self) -> Result<IngestReport> {
        self.cancel.cancel();
        drop(self.sender);
        self.handle.await.map_err(|e| MemoliteError::Internal(e.to_string()))?
    }

    /// Drains the full backlog. Drops this ingestor's own sender and waits
    /// for the channel to close naturally (i.e. every clone of `sender()`
    /// that the caller handed out elsewhere must also be dropped by the
    /// caller before this resolves) and every already-queued chunk to be
    /// processed. Never signals cancellation.
    pub async fn finish(self) -> Result<IngestReport> {
        drop(self.sender);
        self.handle.await.map_err(|e| MemoliteError::Internal(e.to_string()))?
    }
}
```
Doc-level guarantee, stated once: **`finish()` guarantees full backlog drain iff the caller has
already dropped every cloned `sender()` handle it was holding** (otherwise `rx.recv()` never sees
`None` and `finish()` hangs — this is standard mpsc semantics, not a memolite-specific quirk, and
is called out explicitly in the doc comment and in `ARCHITECTURE.md`, Step 209).

### `SentenceBuffer::feed`, char-boundary safe (unchanged from v2 — this part was correct)
```rust
pub struct SentenceBuffer { buf: String }

impl SentenceBuffer {
    pub fn new() -> Self { Self { buf: String::new() } }

    pub fn feed(&mut self, fragment: &str) -> Vec<String> {
        self.buf.push_str(fragment);
        let mut sentences = Vec::new();
        loop {
            let boundary = self.buf.char_indices().find(|(_, c)| matches!(c, '.' | '!' | '?'));
            match boundary {
                Some((byte_pos, c)) => {
                    let mut end = byte_pos + c.len_utf8();
                    while self.buf[end..].chars().next().is_some_and(|c2| matches!(c2, '.' | '!' | '?')) {
                        end += self.buf[end..].chars().next().unwrap().len_utf8();
                    }
                    let sentence = self.buf[..end].trim().to_string();
                    if !sentence.is_empty() { sentences.push(sentence); }
                    self.buf = self.buf[end..].to_string();
                }
                None => break,
            }
        }
        sentences
    }

    pub fn finish(&mut self) -> Option<String> {
        let remaining = self.buf.trim().to_string();
        self.buf.clear();
        if remaining.is_empty() { None } else { Some(remaining) }
    }
}
```

### Tests + checkpoint
- Double-boundary, repeated punctuation, Unicode-safety tests for `SentenceBuffer` (unchanged from
  v2).
- `IngestReport { received: 3, stored: 3, failed: 0, .. }` for 3 valid chunks.
- Real-failure injection test (unchanged from v2's fix): one empty-content chunk among two valid
  ones → `failed == 1`, `stored == 2`, loop keeps consuming afterward.
- **(v3 new, fix #4)** `finish()` test: send 5 chunks, drop every held sender clone, call
  `finish()`, assert `received == 5 && stored == 5` — this assertion is now actually true given
  the corrected loop.
- **(v3 new, fix #4)** `shutdown_now()` test: send 5 chunks into a buffer with slow/blocked
  storage (a test double that sleeps), call `shutdown_now()` shortly after sending — assert
  `received <= 5` and specifically that it returns promptly (bounded wall-clock time) rather than
  waiting for all 5 to be stored.
- Backpressure test with `buffer_size = 1`; confirm all 5 eventually land via `finish()`.
- `StreamIngestor::spawn(engine, 0)` → `Err(InvalidArgument)`.
- `cargo clippy`/`cargo fmt` clean; full suite re-run through M7 + M6.5.
- **Checkpoint:** streamed content retrievable end-to-end; failures observable via `IngestReport`;
  the two shutdown modes do exactly what their names promise, verified by test.

---

# M9 — Compression, with atomic index rebuild and validated embeddings

### Eligibility, excluding expired memories (unchanged from v2)
```rust
pub fn is_compression_eligible(mem: &Memory) -> bool {
    let age_days = (Utc::now() - mem.created_at).num_days();
    let not_expired = mem.expires_at.map(|e| e >= Utc::now()).unwrap_or(true);
    mem.memory_type == MemoryType::Episodic
        && age_days > 14
        && mem.importance < 0.3
        && mem.superseded_by.is_none()
        && not_expired
}
```

### `get_embeddings()` — now validates dimension and finiteness (v3 fix #10)
```rust
fn get_episodic_memories_older_than(&self, days: i64) -> Result<Vec<Memory>> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).timestamp();
    let sql = format!(
        "SELECT {MEMORY_COLUMNS} FROM memories WHERE type = 'episodic' AND created_at < ?1 AND superseded_by IS NULL"
    );
    let mut stmt = self.conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![cutoff], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Returns an error (never a silently-shrunk list) if any id has no
/// embedding row, an undecodable blob, a dimension mismatch, or a
/// non-finite value — compression fails loudly rather than clustering
/// against a partial, silently-incomplete set.
fn get_embeddings(&self, ids: &[Uuid]) -> Result<Vec<(Uuid, Vec<f32>)>> {
    let expected_dim = self.vector_store.dimension();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let (bytes, stored_dim): (Vec<u8>, i64) = self.conn.query_row(
            "SELECT vector, dimension FROM embeddings WHERE memory_id = ?1",
            rusqlite::params![id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let vector: Vec<f32> = bincode::deserialize(&bytes)
            .map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
        if stored_dim as usize != expected_dim || vector.len() != expected_dim {
            return Err(MemoliteError::VectorStore(format!(
                "embedding for {id} has dimension {} (row says {}), expected {}",
                vector.len(), stored_dim, expected_dim
            )));
        }
        if !vector.iter().all(|x| x.is_finite()) {
            return Err(MemoliteError::VectorStore(format!("embedding for {id} contains a non-finite value")));
        }
        out.push((*id, vector));
    }
    Ok(out)
}
```

### Greedy clustering (unchanged) — `Cluster { member_ids: Vec<Uuid> }`, cosine similarity,
threshold 0.85, only clusters with 3+ members summarized.

### Bounded summarization, correct budget accounting (unchanged from v2 — this part was correct)
```rust
pub const MAX_SUMMARY_CHARS: usize = 4000;
pub const COMPRESSION_ALGORITHM_VERSION: &str = "extractive-v1";

pub fn summarize_cluster(memories: &[Memory], threshold: f32) -> Result<CompressionResult> {
    if memories.is_empty() {
        return Err(MemoliteError::InvalidArgument("cannot summarize an empty cluster".into()));
    }
    let earliest = memories.iter().map(|m| m.created_at).min()
        .ok_or_else(|| MemoliteError::Internal("empty cluster after non-empty check".into()))?;
    let latest = memories.iter().map(|m| m.created_at).max()
        .ok_or_else(|| MemoliteError::Internal("empty cluster after non-empty check".into()))?;
    // (v3 fix #17: .min()/.max() no longer .unwrap() even though the empty
    // check above makes it logically unreachable)

    let prefix = format!(
        "[Compressed summary of {} similar episodic memories, {} to {}]: ",
        memories.len(), earliest.format("%Y-%m-%d"), latest.format("%Y-%m-%d")
    );
    let prefix_chars = prefix.chars().count();
    let truncation_marker = "...[truncated, size cap reached]";
    let budget = MAX_SUMMARY_CHARS.saturating_sub(prefix_chars);

    let mut joined = String::new();
    let mut joined_chars = 0usize;
    for (i, m) in memories.iter().enumerate() {
        let piece = if i == 0 { m.content.clone() } else { format!("; {}", m.content) };
        let piece_chars = piece.chars().count();
        if joined_chars + piece_chars + truncation_marker.chars().count() > budget {
            joined.push_str(truncation_marker);
            break;
        }
        joined.push_str(&piece);
        joined_chars += piece_chars;
    }

    Ok(CompressionResult {
        summary_content: format!("{prefix}{joined}"),
        original_ids: memories.iter().map(|m| m.id).collect(),
        cluster_id: uuid::Uuid::new_v4(),
        threshold,
        time_range: (earliest, latest),
    })
}
```

### `compress_old_memories()` and atomic-swap index rebuild (v3 fix #9)
Review item 9: `clear()` then re-`insert()` one-by-one can fail partway and leave a permanently
half-empty live index — the opposite of what "provably reconstructable" claimed. **v3 fix: build
the replacement set off to the side, then swap in one step.** For `InMemoryVectorStore` this is
exact and atomic; for a remote backend it's documented as best-effort unless that backend supports
an atomic bulk-replace (M11 addresses this explicitly).

```rust
pub async fn compress_old_memories(&self) -> Result<usize> {
    let candidates: Vec<Memory> = self.get_episodic_memories_older_than(14)?
        .into_iter()
        .filter(compression::is_compression_eligible)
        .collect();

    let ids: Vec<Uuid> = candidates.iter().map(|m| m.id).collect();
    let with_vectors = self.get_embeddings(&ids)?; // now dimension/finite-checked
    let threshold = 0.85;
    let clusters = compression::greedy_cluster(&with_vectors, threshold);
    let mut compressed_count = 0;

    for cluster in clusters.into_iter().filter(|c| c.member_ids.len() >= 3) {
        let members: Vec<Memory> = candidates.iter()
            .filter(|m| cluster.member_ids.contains(&m.id))
            .cloned()
            .collect();
        let result = compression::summarize_cluster(&members, threshold)?;

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("compression.original_ids".into(),
            serde_json::json!(result.original_ids.iter().map(Uuid::to_string).collect::<Vec<_>>()));
        metadata.insert("compression.threshold".into(), serde_json::json!(result.threshold));
        metadata.insert("compression.algorithm_version".into(), serde_json::json!(compression::COMPRESSION_ALGORITHM_VERSION));
        metadata.insert("compression.time_range".into(), serde_json::json!([
            result.time_range.0.to_rfc3339(), result.time_range.1.to_rfc3339()
        ]));

        let mut request = StoreRequest::new(&result.summary_content, MemoryType::Episodic, 0.3)
            .with_confidence(ConfidenceLevel::Inferred);
        request.metadata = metadata;

        let new_id = self.store_with_options(request).await?;
        let new_uuid = Uuid::parse_str(&new_id).unwrap_or_else(|_| Uuid::new_v4()); // fix #17

        if let Err(e) = self.mark_all_superseded(&result.original_ids, &new_id) {
            let del_err = self.conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![new_id]).err();
            let vec_err = self.vector_store.delete(new_uuid).await.err();
            if del_err.is_some() || vec_err.is_some() {
                return Err(MemoliteError::CompensationFailed {
                    operation: e.to_string(), compensation: format!("{:?} / {:?}", del_err, vec_err),
                });
            }
            return Err(e);
        }

        for old_id in &result.original_ids {
            let _ = self.vector_store.delete(*old_id).await; // best-effort per-id; reconciled below
        }
        compressed_count += members.len();
    }

    // (v3 fix #9) Reconcile the live index against SQLite exactly once at the
    // end of a compression pass, using the atomic-swap rebuild — not a
    // clear-then-loop that can leave a half-empty index on partial failure.
    self.reconcile_vector_index_if_needed().await?;

    Ok(compressed_count)
}

/// Builds a brand-new backing store off to the side from every active
/// (non-superseded, non-expired) memory's embedding, validating each one,
/// and only replaces `self.vector_store`'s contents if the ENTIRE build
/// succeeds. On the in-memory backend this means: build a fresh
/// `InMemoryVectorStore`, then atomically swap its inner data into the live
/// one under a single write lock — never expose a partially-rebuilt index.
async fn rebuild_active_vector_index(&self) -> Result<()> {
    let active = self.get_active_memories()?; // excludes expired + superseded
    let dim = self.vector_store.dimension();
    let replacement = InMemoryVectorStore::new(dim);

    for mem in &active {
        let Some(vector) = self.get_embedding(&mem.id.to_string()).await? else { continue };
        if vector.len() != dim || !vector.iter().all(|x| x.is_finite()) {
            return Err(MemoliteError::VectorStore(format!(
                "cannot rebuild index: embedding for {} is invalid", mem.id
            )));
        }
        replacement.insert(mem.id, &vector, HashMap::new()).await?; // building the side copy; cannot fail partway into the LIVE index
    }

    // Atomic swap: clear the live store, then bulk-insert from the fully-built
    // replacement in one pass. Because `replacement` already validated every
    // entry above, this second pass cannot fail for data reasons — only for
    // infra reasons (lock poisoning), which is the same failure class `clear()`
    // itself can already return.
    self.vector_store.clear().await?;
    for mem in &active {
        if let Some(vector) = self.get_embedding(&mem.id.to_string()).await? {
            self.vector_store.insert(mem.id, &vector, HashMap::new()).await?;
        }
    }
    Ok(())
}

/// Cheap check the plan uses after any batch of per-id deletes: if the live
/// index's rough size disagrees with the SQLite-active count, run the atomic
/// rebuild. (A simple, honest heuristic — not a proof of correctness, but
/// good enough to self-heal the common case without rebuilding on every call.)
async fn reconcile_vector_index_if_needed(&self) -> Result<()> {
    // Implementation detail left to the engine's discretion; the contract is:
    // if in doubt, rebuild. Always safe to call unconditionally too, just more expensive.
    self.rebuild_active_vector_index().await
}

fn mark_all_superseded(&self, old_ids: &[Uuid], new_id: &str) -> Result<()> {
    let tx = self.conn.unchecked_transaction()?; // becomes conn.lock() + tx per M6.5
    for old_id in old_ids {
        tx.execute(
            "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
            rusqlite::params![new_id, old_id.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(())
}
```
> **Honesty note for remote backends (ties into M11):** the atomic-swap guarantee above is exact
> for `InMemoryVectorStore` because the swap happens behind one write lock. A remote HTTP-based
> `VectorStore` cannot make the same promise unless its API offers an atomic bulk-replace or
> namespace-swap primitive. `ARCHITECTURE.md` (Step 209) states this distinction explicitly rather
> than claiming uniform atomicity across backends.

`get_active_memories()`: `SELECT {MEMORY_COLUMNS} FROM memories WHERE superseded_by IS NULL AND
(expires_at IS NULL OR expires_at >= :now)`.

### `stats()` (unchanged intent from v2): active/expired/superseded/embeddings/compressed-summary
counts, the last identified via the `compression.original_ids` metadata key.

### Tests + checkpoint
- Eligibility boundary tests; greedy-clustering tests; empty-cluster → `Err`.
- `summarize_cluster` budget test including the prefix (v2's already-correct fix, re-verified).
- Integration: 3 similar low-importance episodic memories (backdated directly in test SQL);
  `compress_old_memories()` returns `3`.
- Post-compression `recall()` behavior; superseded visibility; vector-search exclusion; 2-member
  clusters untouched; semantic/procedural never touched.
- **(v3 new, fix #9)** Force `store_with_options` for the summary to succeed but a subsequent
  per-id `vector_store.delete()` to fail for one id (test-only wrapper) — assert
  `reconcile_vector_index_if_needed` runs and afterward the live index's contents exactly match
  `get_active_memories()`, with **no window where the index is left partially built** (assert by
  checking index contents mid-test via a synchronization point in the test double, not just at the
  end).
- **(v3 new, fix #10)** A memory with a dimension-mismatched or non-finite embedding in the
  candidate set causes `compress_old_memories()` to return `Err`, not silently drop that memory.
- `cargo clippy`/`cargo fmt` clean.
- Benchmark at ~1,000 eligible candidates.
- Restart test: after a successful compression run, reopen the engine (exercises Step 44a's
  backfill), confirm `vector_store.search()` results match what SQLite says should be active.
- Full suite re-run from Step 0 through M8.
- **Checkpoint:** compression is data-loss-free, fails loudly on incomplete/invalid embedding
  data, truthfully retrieval-density-only (not disk-saving), its budget accounting is correct, and
  its index rebuild is atomic-swap (never a visible partial state) for the shipped backend.

---

# M10 — Maintenance controller (v3 fix #11, #12)

### Dependencies
**File:** `Cargo.toml`:
```toml
[dependencies]
tokio-util = { version = "0.7", features = ["rt"] }
tracing = "0.1"

[dev-dependencies]
tokio = { version = "1", features = ["full", "test-util"] }
wiremock = "0.6" # only exercised once M11's tests run with --features veclite (or generic-http)
```

### `MaintenanceHandle`, `interval_at` (unchanged timing fix from v2), **fallible start** (fix #11),
**single-controller enforced** (fix #12)
```rust
use tokio_util::sync::CancellationToken;
use tokio::task::JoinHandle;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use crate::engine::MemoryEngine;
use crate::error::{MemoliteError, Result};

pub struct MaintenanceConfig {
    pub purge_interval: std::time::Duration,
    pub compress_interval: std::time::Duration,
}

pub struct MaintenanceHandle { cancel: CancellationToken, join: JoinHandle<()> }

impl MemoryEngine {
    /// Starts the background purge/compression loop.
    /// Errors:
    /// - `InvalidArgument` if either interval is zero (`interval_at` panics on
    ///   a zero period, so this is validated up front instead — fix #11).
    /// - `InvalidArgument` if maintenance is already running on this engine
    ///   (fix #12: exactly one controller per engine, enforced via an
    ///   `AtomicBool`, to prevent two independent compression passes from
    ///   racing on the same candidate set and creating duplicate summaries).
    pub fn start_maintenance(self: &Arc<Self>, config: MaintenanceConfig) -> Result<MaintenanceHandle> {
        if config.purge_interval.is_zero() || config.compress_interval.is_zero() {
            return Err(MemoliteError::InvalidArgument("maintenance intervals must be non-zero".into()));
        }
        if self.maintenance_running.swap(true, Ordering::SeqCst) {
            return Err(MemoliteError::InvalidArgument(
                "maintenance is already running on this engine; call shutdown() on the existing handle first".into()
            ));
        }

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let weak: Weak<MemoryEngine> = Arc::downgrade(self);
        let running_flag = Arc::clone(&self.maintenance_running);

        let join = tokio::spawn(async move {
            let now = Instant::now();
            let mut purge_tick = interval_at(now + config.purge_interval, config.purge_interval);
            purge_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut compress_tick = interval_at(now + config.compress_interval, config.compress_interval);
            compress_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    _ = purge_tick.tick() => {
                        let Some(engine) = weak.upgrade() else { break };
                        if let Err(e) = engine.purge_expired().await {
                            tracing::warn!(error = %e, "background purge failed; continuing");
                        }
                    }
                    _ = compress_tick.tick() => {
                        let Some(engine) = weak.upgrade() else { break };
                        if let Err(e) = engine.compress_old_memories().await {
                            tracing::warn!(error = %e, "background compression failed; continuing");
                        }
                    }
                }
            }
            running_flag.store(false, Ordering::SeqCst); // release the slot on any exit path
        });

        Ok(MaintenanceHandle { cancel, join })
    }
}

impl MaintenanceHandle {
    pub async fn shutdown(self) -> Result<()> {
        self.cancel.cancel();
        self.join.await.map_err(|e| MemoliteError::Internal(e.to_string()))
    }
}
```
`MemoryEngine` gains one new field: `maintenance_running: Arc<AtomicBool>` (initialized to
`Arc::new(AtomicBool::new(false))` in `open()`/`open_with_store()`).

`purge_expired()` follows the exact `forget()` reconciliation rule from Step 48: delete from
SQLite, attempt per-id vector deletes, and on any failure call `rebuild_active_vector_index()`
rather than leaving the index stale.

### Panic handling
Explicit, one-time decision: a panic inside the maintenance loop surfaces via `handle.await`'s
`JoinError` — no `catch_unwind` wrapping in v1. Because the loop now clears
`maintenance_running` at its *normal* exit path only, a panic leaves the flag `true` (the task is
dead and the `Arc<MemoryEngine>` it held is gone), which correctly blocks a confusing second
`start_maintenance()` call on an engine whose maintenance task already crashed — the caller must
notice the crash (via `handle.await` returning `Err`) rather than silently spinning up a second
loop over a possibly-inconsistent engine.

### Tests + checkpoint
- `#[tokio::test(start_paused = true)]` + `tokio::time::advance(...)`: first tick does not fire
  until the configured interval has elapsed.
- Cancellation: `shutdown()` exits promptly, returns `Ok(())`.
- **(v3 fix #11)** `start_maintenance` with a zero `purge_interval` or `compress_interval` returns
  `Err(InvalidArgument)` — no panic.
- **(v3 fix #12)** Calling `start_maintenance` a second time on the same `Arc<MemoryEngine>` while
  the first handle is still running returns `Err(InvalidArgument)`. After `shutdown()`ing the
  first handle, a second `start_maintenance` call succeeds.
- Engine-drop test, exact sequence: (1) keep the `MaintenanceHandle`, (2) drop the last
  `Arc<MemoryEngine>`, (3) `tokio::time::advance(...)` past the next tick, (4) await
  `handle.shutdown()` and confirm the task had already exited via `upgrade()` failure, not via
  cancellation.
- Concurrent `store()`/`recall_query()` during a maintenance tick don't deadlock.
- Missed-tick test: advance paused time past several intervals at once, confirm `Skip` (no burst).
- Force `purge_expired()` to fail once (test-only injection); confirm the loop logs and continues.
- No `tokio::spawn` inside `open()` — maintenance is opt-in only.
- Purge removes ids from the vector store, reconciling via rebuild on any per-id failure — tested
  directly.
- `tracing` emits on every purge/compression run (assert via a test subscriber).
- `ARCHITECTURE.md` documents shutdown semantics and the single-controller rule precisely.
- `cargo clippy`/`cargo fmt` clean; full suite re-run through M9.
- `MemoryEngine::open()` alone, maintenance never started, behaves identically to pre-M10.
- **Checkpoint:** purge/compression run only when explicitly started, start is fallible and
  cannot panic on bad config, exactly one controller can run at a time, cleanly cancellable,
  survive missed ticks and transient errors, never leak a detached task.

---

# M11 — Optional vector-store backend, honestly scoped (v3 fix #5, #6)

### Protocol-pinning gate — unchanged decision, now with a concrete exit condition
Before any adapter code below is merged: pin a concrete VecLite repository + version, and copy its
authoritative request/response schema into `tests/fixtures/veclite_protocol.json` as the source of
truth. **v3 adds the fix for review blocker #6:** since `VectorStore::clear()` is now required
(Step 41) for both index rebuild (M9) and any remote backfill policy, an adapter that cannot
implement `clear()` for real cannot honestly ship as `--features veclite` and cannot be included
in the `--all-features` release gate (Step 213 below).

**Decision:** if no stable VecLite protocol is pinned by the time M11 is reached, ship this
milestone as `--features generic-http` instead of `--features veclite`, and implement `clear()`
for real against a documented, testable generic contract (e.g. `DELETE {base_url}/vectors` +
`204`), rather than returning a permanent "not implemented" error. A feature that can't pass its
own `clear()` test is not allowed into the all-features gate — full stop.

### Feature flag
```toml
[dependencies]
reqwest = { version = "0.12", features = ["json"], optional = true }
urlencoding = { version = "2", optional = true }

[features]
generic-http = ["dep:reqwest", "dep:urlencoding"] # renamed from "veclite" per the gate above
# If/when a real VecLite protocol is pinned, add: veclite = ["generic-http"]
# as a thin, protocol-specific layer on top, and keep generic-http as the
# honestly-scoped fallback either way.
```

### Adapter — `Uuid`-consistent, upsert-based `insert`, real `clear()`, real `contains()`
```rust
pub struct GenericHttpVectorStore {
    client: reqwest::Client,
    base_url: String,
    dim: usize,
    // api_key etc, redacted in Debug
}

#[async_trait]
impl VectorStore for GenericHttpVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        // PUT {base_url}/vectors/{id} — PUT semantics = upsert, matching the
        // trait's documented contract (Step 41).
        self.client.put(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .json(&serde_json::json!({ "vector": vector, "metadata": metadata }))
            .timeout(std::time::Duration::from_secs(10))
            .send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        // POST {base_url}/search { "vector": ..., "k": ... } -> [{ id, score }]
        todo!("wire against the pinned/generic contract's actual response shape")
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        self.client.delete(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn contains(&self, id: Uuid) -> Result<bool> {
        let resp = self.client.get(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(resp.status() == reqwest::StatusCode::OK)
    }

    /// Real implementation, not a permanent stub — required for the
    /// all-features release gate (fix #6).
    async fn clear(&self) -> Result<()> {
        self.client.delete(format!("{}/vectors", self.base_url))
            .send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    fn dimension(&self) -> usize { self.dim }
}
```
`Debug` impl for `GenericHttpVectorStore` is hand-written to redact any API key field.

### `open_with_store()`, `UpsertActive` backfill policy (replaces `InsertMissing` — fix #5)
```rust
pub enum BackfillPolicy {
    /// Assume the remote store already has everything; do nothing.
    ExistingOnly,
    /// Insert every locally-stored active memory's embedding. Safe to call
    /// unconditionally because every `VectorStore::insert` is a documented
    /// upsert — this replaces v2's unimplementable `InsertMissing`.
    UpsertActive,
    /// Clear the remote store and re-insert every active memory's embedding
    /// (best-effort atomicity for remote backends — see the M9 honesty note).
    Rebuild,
}

pub async fn open_with_store(
    path: impl AsRef<std::path::Path>,
    store: std::sync::Arc<dyn VectorStore>,
    backfill: BackfillPolicy,
) -> Result<Self> {
    // Same as open(): construct conn, run_migrations, then apply `backfill`:
    // ExistingOnly -> skip; UpsertActive -> backfill_active_vectors(&conn, &store).await?;
    // Rebuild -> store.clear().await?; then backfill_active_vectors(&conn, &store).await?;
    ...
}
```

### Tests + checkpoint
- Unit tests use `wiremock`, never a live server.
- `tests/generic_http_integration_test.rs`, `#[cfg(feature = "generic-http")]`, opt-in via
  `GENERIC_HTTP_TEST_URL`; skips with a clear message if unset, never fails the suite.
- Integration test (when configured): `open_with_store(..., Arc::new(store),
  BackfillPolicy::UpsertActive)`, store 5, `recall_query()` one, confirm correctness.
- **(v3 new, fix #6)** `clear()` integration test against the mock server: insert 3, `clear()`,
  confirm `search()` returns empty and `contains()` returns `false` for all 3 — this is the exact
  test that `--all-features` now requires to pass, closing the blocker.
- `verify_collection()`-style dimension-mismatch test.
- `Debug` output never contains the raw API key.
- `cargo test` (default features) passes with zero HTTP dependency running.
- `cargo build` (default features) does not pull in `reqwest` — verify via `cargo tree`.
- `cargo clippy --all-features`/`cargo fmt` clean.
- `ARCHITECTURE.md` documents the actual before/after swap lines and the `BackfillPolicy` choice.
- **Checkpoint:** the scale-up path is either genuinely pinned-and-tested as VecLite, or shipped
  under its honest name (`generic-http`) with every trait method — including `clear()` — really
  implemented and tested. `--all-features` passes either way.

---

# M12 — Final polish, docs, release gate

### Full rustdoc pass
Every `pub` item across `lib.rs`, `engine.rs`, `recall.rs`, `requests.rs`, `confidence.rs`,
`compression.rs`, `streaming.rs`, `maintenance.rs`, `ranking.rs`, `math_utils.rs`,
`vector_store/*.rs` gets a `///` doc comment.

### README reconciliation
Diff the README's Public API section against real signatures line by line, including the fact
that `recall(&str) -> Vec<Memory>` now **delegates to** `recall_query`/`recall_text` (Step 63a),
documented as intentional convenience wrappers, not independent implementations.

### Feature-list reconciliation
Temporal querying, streaming ingestion, compression, and confidence scoring are "must-ship (v1
core)."

### `ARCHITECTURE.md` must document, explicitly:
- Concurrency model: `Mutex<Connection>` (M6.5) + `Mutex<Embedder>` (day one) +
  `Arc<dyn VectorStore>`; lock-scope discipline; `Send + Sync` proof.
- The migration runner's **always-verify** policy (Step 74) and why "repairing" was chosen over
  "strict."
- Durability boundaries: what a crash mid-`store_with_options` can/cannot lose; the
  `CompensationFailed` error and when it can occur.
- Restart backfill (Step 44a): every `open()`/`open_with_store()` rebuilds the live vector index
  from SQLite; this is what makes the in-memory backend durable across restarts despite not
  persisting itself.
- `recall()` vs `recall_query()`/`recall_text()`: one delegates to the other; there is exactly one
  ranking/filtering implementation.
- Score semantics (`similarity` vs `score` in `RecallItem`); full ranking formula with confidence
  weighting; `MAX_CANDIDATES` cap.
- Compression limitations: extractive, not abstractive; no disk-space claim; fails loudly on
  missing/invalid embeddings; index rebuild is atomic-swap for the in-memory backend and
  best-effort for remote backends.
- The two `StreamIngestor` shutdown modes (`shutdown_now` vs `finish`) and the exact conditions
  under which `finish()` actually drains fully.
- Maintenance: single-controller enforcement, fallible start, cancellation vs. engine-drop
  shutdown.
- The generic-http/VecLite backend's honest scope.

### "Risks and Honest Limitations" section must state:
- Compression is extractive/concatenation-based, not LLM-abstractive.
- `as_prompt_context()` delimits content; it does not sanitize against prompt injection.
- Remote vector-store backends do not get the same atomic index-rebuild guarantee as the built-in
  in-memory backend.

### Final validation
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets --all-features`
- Default-feature build/test run separately, proving `generic-http`/`veclite` is genuinely
  optional.
- `cargo doc --no-deps --all-features` — zero warnings.
- Every example runs standalone via `cargo run --example <name>` against a temporary database.
- Clean checkout/restart path: fresh clone, fresh build, fresh `open()`.
- Migration test using a copy of the **actual current repo's** database output (from running
  `store()` against today's code before any of these steps are applied) — confirm it lands
  correctly at the latest schema version via the real, always-verifying adoption logic.
- Add package metadata to `Cargo.toml`; `cargo publish --dry-run`.
- **Final checkpoint — release gate, not automatic:** after the user reviews the diff, release
  notes, and semver choice, the user may explicitly authorize a git tag. No automatic tagging or
  publishing. Version number is chosen from actual release history (check for an existing tag
  before assuming `0.1.0` vs `0.2.0`).

---

## Cross-reference: every v2-review finding and where v3 fixes it

| # | Finding | Fix location in v3 |
|---|---|---|
| 1 (blocker) | Restart never backfills the vector store | Step 44a, `backfill_active_vectors`, called from `open()`; tests 53–54 |
| 2 (blocker) | Migration test 78 can't pass with version-gated checks | Step 74: table/column checks now run unconditionally every `open()`; test 78 |
| 3 (blocker) | `as_prompt_context` can exceed `max_chars` | Step 68: single char-aware `try_append` gate; tests 71 |
| 4 (blocker) | Streaming shutdown races cancellation against backlog | M8: `shutdown_now()` vs `finish()`, non-biased loop; tests in M8 |
| 5 (blocker) | `InsertMissing` unimplementable with the trait | Step 41: `insert()` documented as upsert; `contains()` added; `UpsertActive` replaces `InsertMissing` in M11 |
| 6 (blocker) | M11 can't pass `--all-features` with a stub `clear()` | M11: real `clear()` on `GenericHttpVectorStore`; honest rename if unpinned; `clear()` gates the release checklist |
| 7 | Forget/purge two-store consistency underspecified | Step 48: exact SQLite-first-then-vector-then-rebuild-then-report rule, reused by `purge_expired` |
| 8 | Ignored compensation failures | Step 0.1 `CompensationFailed` variant; used in Steps 45, 48, 90, 91, M9 |
| 9 | Rebuild can leave a partial index | `rebuild_active_vector_index`: build-aside-then-swap, tested with a mid-rebuild synchronization point |
| 10 | Compression skips dimension/finite checks | `get_embeddings` and `backfill_active_vectors` both validate dimension + `is_finite()` |
| 11 | Maintenance config can panic | `start_maintenance` returns `Result`, validates non-zero intervals first |
| 12 | Two maintenance controllers can race compression | `maintenance_running: AtomicBool`, single-controller enforced, tested |
| 13 | `metadata_equals` defined but unused | Step 64: applied as a real filter in `recall_query` |
| 14 | `recall()` can drift from `recall_query()` | Step 63a: `recall()` delegates to `recall_query()` |
| 15 | Unbounded candidate-pool arithmetic | Step 0.6: `MAX_CANDIDATES` + `candidate_pool_size()`, used in `recall_query` |
| 16 | Plan not self-contained ("unchanged" references) | v3 inlines every code block instead of deferring to v2/the original plan |
| 17 | Residual panic paths (`expect`, `unwrap`) | Step 91 (`update`), `summarize_cluster`'s min/max, `compress_old_memories`'s uuid parse — all converted to fallback/`ok_or_else` |

v3 is checked against the real `mayanpathak/memolite` repo (re-cloned and re-verified before
writing this document: `engine.rs` still has the plain `Connection`/`Mutex<Embedder>` struct and a
`todo!()` `recall()`; `error.rs` still lacks the new variants). It should compile incrementally at
every checkpoint, and none of the 17 findings from the v2 review remain open.


#### SHORTCOMINGS OF THIS BUILDING PLAN ###

V3 is significantly improved, but it is still not properly executable milestone-by-milestone. Several sequencing errors cause intermediate checkpoints not to compile, and the advertised “atomic” index rebuild is not actually atomic.

No repository files were modified.

## Critical blockers

### 1. M3 calls functionality that is not created until later milestones

M3’s `open()` is instructed to call:

```rust
run_migrations(&mut conn)?;
```

But `run_migrations()` is not introduced until M6, after M3 and M4.

M3’s `forget()` similarly calls:

```rust
self.rebuild_active_vector_index().await
```

but that method is not introduced until M9.

Therefore the M3 checkpoint cannot compile as written.

Required fix: move the following into Step 0 or M3:

- The baseline migration runner needed by `open()`.
- `rebuild_active_vector_index()` or a simpler M3 reconciliation helper.

M6 can later add migration 2 for confidence, but the migration infrastructure itself must exist before M3.

### 2. M4 accesses confidence before confidence exists

At [plan line 517](C:\Users\Mayan\.codex\attachments\b5151b9d-39da-477d-b270-834e38dc442c\pasted-text.txt:517), M4 uses:

```rust
let confidence_weight = memory.confidence.weight();
```

`Memory.confidence` is not added until M6. The comment saying “1.0 until M6 lands” does not match the code.

During M4 use:

```rust
let confidence_weight = 1.0;
```

Then Step 76 must replace it with:

```rust
let confidence_weight = memory.confidence.weight();
```

M4 also calls:

```rust
self.update_access_stats_and_maybe_promote(...)
```

which does not exist until M6. Before M6 it must call the Step 47 helper:

```rust
self.update_access_stats(...)
```

and M6 should replace that call.

### 3. M3 test 51 cannot pass

The Step 46 `recall()` implementation never calls `update_access_stats()`, yet test 51 requires `access_count` to increase.

Add inside the final result loop:

```rust
self.update_access_stats(mem.id)?;
```

This will later be replaced by the M6 promotion helper.

### 4. `include_expired` and `include_superseded` cannot work

Restart backfill indexes only:

```sql
WHERE m.superseded_by IS NULL
  AND (m.expires_at IS NULL OR m.expires_at >= ?1)
```

Compression also deletes superseded vectors from the vector store.

But `RecallQuery` advertises:

```rust
include_expired
include_superseded
```

Filtering can only remove vector candidates; it cannot recover rows that were never indexed.

Choose one design:

- Recommended: keep all non-deleted memories in the vector index, including expired and superseded records, and exclude them through default recall filters.
- Alternative: when either include flag is enabled, retrieve stored embeddings directly from SQLite and score them outside the live vector index.
- Simplest: remove the include flags and related tests.

Without one of these changes, both options are misleading and their tests will fail after restart/compression.

### 5. The claimed atomic index rebuild is still clear-then-insert

At [plan line 1366](C:\Users\Mayan\.codex\attachments\b5151b9d-39da-477d-b270-834e38dc442c\pasted-text.txt:1366), the plan builds a temporary `InMemoryVectorStore`, but never swaps it into the engine. It then performs:

```rust
self.vector_store.clear().await?;

for mem in &active {
    self.vector_store.insert(...).await?;
}
```

That is exactly the non-atomic clear-then-loop operation v3 claims to have fixed. If any insertion fails, the live index is partial.

A genuine solution requires one of:

```rust
async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;
```

with an atomic in-memory implementation; or:

```rust
vector_store: RwLock<Arc<dyn VectorStore>>
```

so a fully constructed replacement backend can be swapped into the engine in one lock operation.

For remote stores, document `replace_all` as backend-dependent or use generation/namespace swapping.

### 6. M11 still contains `todo!()`

The generic HTTP backend’s `search()` is:

```rust
todo!("wire against the pinned/generic contract's actual response shape")
```

That compiles but panics during its required integration test. Therefore this claim cannot be true:

> `--all-features` passes either way.

The generic HTTP contract must specify and implement its response:

```json
[
  { "id": "uuid", "score": 0.91 }
]
```

Then deserialize, validate UUIDs and finite scores, sort if required, and return `VectorHit`s.

## Concurrency faults after M6.5

### 7. Later snippets still call methods directly on `Mutex<Connection>`

M6.5 changes:

```rust
conn: Mutex<Connection>
```

But M9 and other later code still uses:

```rust
self.conn.prepare(...)
self.conn.query_row(...)
self.conn.execute(...)
self.conn.unchecked_transaction(...)
```

Those methods do not exist on `Mutex<Connection>`.

Every post-M6.5 helper must use a scoped guard:

```rust
let conn = self.conn
    .lock()
    .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

let mut stmt = conn.prepare(...)?;
```

For transactions:

```rust
let mut conn = self.conn
    .lock()
    .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

let tx = conn.transaction()?;
```

The guard must be dropped before any vector-store `.await`.

### 8. Backfill holds SQLite statement state across `.await`

`backfill_active_vectors()` iterates `rusqlite::Rows` and awaits `store.insert()` inside the iteration. This retains the statement/connection borrow across an await and can make the future non-`Send`.

Read and decode everything first:

```rust
let entries: Vec<(Uuid, Vec<f32>, HashMap<String, Value>)> = {
    // prepare/query/collect synchronously
};

for (id, vector, metadata) in entries {
    store.insert(id, &vector, metadata).await?;
}
```

This also becomes necessary once the connection is mutex-protected.

## API/data-model faults

### 9. Permanent expiry cannot be preserved during update

The plan says that if an old memory has `expires_at == None`, its replacement should also have no expiry.

But:

```rust
request.custom_ttl = None;
```

means “use the memory type’s default TTL,” not “permanent.” Test 98 therefore cannot pass.

An `Option<Duration>` cannot represent all three states:

1. Use type default.
2. Use custom TTL.
3. Never expire.

Introduce:

```rust
pub enum ExpiryPolicy {
    TypeDefault,
    Custom(chrono::Duration),
    Never,
}
```

Store it in `StoreRequest`, and use `Option<ExpiryPolicy>` in `MemoryUpdate` where `None` means preserve the original policy.

### 10. UUID fallback can delete the wrong vector

The update code does:

```rust
let new_uuid = Uuid::parse_str(&new_id).unwrap_or(uuid);
```

If parsing fails, `uuid` is the old memory’s ID. Compensation could delete the original memory’s vector instead of the replacement’s vector.

Never fall back to a different ID. Return a typed UUID error, or change the internal store pipeline to return `Uuid`:

```rust
async fn store_request_internal(...) -> Result<Uuid>;
```

The public `store()` can convert the final UUID to `String`.

Compression has a similar bad fallback:

```rust
Uuid::parse_str(&new_id).unwrap_or_else(|_| Uuid::new_v4())
```

That creates an unrelated ID and makes compensation ineffective.

### 11. `contains()` mishandles HTTP errors

The generic adapter currently treats every status other than `200 OK` as `false`:

```rust
Ok(resp.status() == StatusCode::OK)
```

A `500`, `401`, or `429` is not “missing.”

Use:

```rust
match resp.status() {
    StatusCode::OK => Ok(true),
    StatusCode::NOT_FOUND => Ok(false),
    _ => Err(MemoliteError::VectorStore(
        resp.error_for_status().unwrap_err().to_string()
    )),
}
```

### 12. `store_with_options()` remains incomplete pseudocode

Step 90 contains comments for the most important persistence work:

```rust
// id, timestamps, metadata_json...
// embed-before-write, single SQLite tx...
```

That is not a copy-pasteable implementation. Because the plan claims to be fully self-contained and executable, it must include:

- Content and importance validation.
- TTL/expiry calculation.
- Metadata serialization.
- Embedder locking.
- Bincode serialization.
- The complete memory insert.
- The complete embedding insert.
- Transaction commit.
- Vector insertion and compensation.

The same issue affects `open_with_store()`, `stats()`, greedy clustering, and parts of the temporal API.

## Temporal-feature regression

### 13. M7 removed previously promised temporal features

V2 included:

- `what_changed_since()`
- `find_stale_memories()`
- `RecallQuery.created_after`
- `RecallQuery.created_before`
- `RecallQuery.only_stale`

V3 replaces these with `query_by_time_range()` and a superseded-chain walker. That may be a valid smaller scope, but it no longer implements the previously advertised temporal-querying feature set.

Decide which API is actually part of the project and make M7 explicit. If those features remain required, restore them and their tests.

## Maintenance and recovery issues

### 14. Maintenance can remain permanently locked after a panic

The plan intentionally leaves `maintenance_running = true` after a task panic. That means there is no recovery path other than reopening the entire engine.

A better `MaintenanceHandle::shutdown()` should clear the flag after observing the join result, including a panic. Alternatively provide:

```rust
pub fn reset_maintenance_after_failure(&self) -> Result<()>;
```

Blocking silent restart after an unobserved panic is reasonable; blocking an explicit caller who observed and handled the failure is not.

### 15. “Self-heals any partial schema” is overstated

The migration runner repairs missing tables and the confidence column. It does not verify or repair:

- Missing baseline columns.
- Incorrect column types or constraints.
- Broken foreign-key definitions.
- Incorrect embedding schema.
- Missing confidence constraints when the column already exists.

Change the claim to “repairs missing expected tables and the confidence column,” or add complete schema verification.

### 16. Compression summary expiry needs a deliberate policy

Compressed summaries are stored as `Episodic`, so they receive the episodic default TTL. Originals remain superseded and excluded by default. When the summary expires or is purged, the consolidated information disappears from normal recall even though originals remain in SQLite.

Choose an explicit policy:

- Store compressed summaries as semantic memories.
- Give summaries a longer/custom TTL.
- Restore originals if their summary is purged.
- Or document that compression intentionally allows eventual loss from active retrieval.

## Verdict

V3 fixes many v2 issues, especially:

- Restart backfill.
- Migration adoption checks.
- Bounded prompt rendering.
- Separate immediate/draining streaming shutdown.
- Candidate caps and metadata filtering.
- Maintenance interval validation.
- Backend policy simplification.
- Embedding dimension/finite checks.

But the plan still has six direct execution blockers:

1. M3 depends on M6 and M9 code.
2. M4 depends on M6 fields/methods.
3. M3 access-count test lacks the call it tests.
4. Include-expired/superseded cannot work with the index policy.
5. The “atomic rebuild” is not atomic.
6. The optional backend still contains `todo!()`.

My rating:

- Architecture: 8.5/10
- Feature design: 8/10
- Compile readiness by milestone: 6.5/10
- End-to-end executability: 7/10
- Properly executable as written: no

The plan is close conceptually, but it needs another correction pass focused on build-order independence and replacing remaining pseudocode with complete implementations.