# Memolite — Final Master Build Plan (v5, root-cause-fixed, fully self-contained)

> **What changed from v4:** v4 was reviewed and found ~80–85% executable, with 18 findings.
> Several of those findings were symptoms of the *same* underlying design gap, not independent
> bugs. v5 fixes the gap once, at its source, instead of patching each symptom:
>
> - Findings #1, #3, #4, #5 (Step 0 depends on M3 types; resync doesn't remove stale ids; rebuild
>   drops expired/superseded rows; rebuild silently replaces a remote backend with an in-memory
>   one) were **all** downstream of one thing: the reconciliation logic never went through the
>   `VectorStore` trait itself. v5 adds a single trait method, `replace_all`, implemented once per
>   backend, and reorders Step 0 so the trait exists before anything calls it. Every
>   reconciliation path (`forget`, restart backfill, M9 rebuild) now calls `replace_all` through
>   the trait — there is exactly one reconciliation mechanism, not four.
> - Findings #9, #10, #17 (a filter test needs a method from a later milestone; `RecallQuery`
>   builder methods with no matching struct fields; `recall()` returning stale pre-increment data)
>   were all downstream of writing a snippet before checking what already exists earlier in the
>   same document. v5 fixes this by having every struct grow its fields in the *same* step that
>   introduces the first builder method or test that needs them — never split across milestones.
> - This document is written against the **actual current state** of
>   `github.com/mayanpathak/memolite` (verified directly, not assumed): `MemoryEngine` currently
>   holds a plain `rusqlite::Connection` (not yet a `Mutex`), `MemoryType::default_ttl()` already
>   exists and is correct (Working = 4 hours), `get()` takes `&str`, and `get_embedding()` is
>   already declared `async`. v5 builds forward from that, not from an imagined starting point.
>
> Rule zero, unchanged: everything already working in the repository stays as-is, with the Step-0
> repairs applied first. Every code block in this document only calls things defined at or before
> that point in this document — that is the concrete, checkable meaning of "executable as
> written," and it is checked at the end of every milestone's checkpoint.

Cross-reference table (every finding from the v4 review → where v5 fixes it, and which root cause
it was folded into) is at the very end.

---

## Step 0 — Foundations, correctly ordered this time

Step 0 now ships the `VectorStore` trait itself, *before* anything in `MemoryEngine` is changed to
depend on it. This directly fixes finding #1 (Step 0 previously referenced `VectorStore` and
`InMemoryVectorStore` before M3 created them).

### 0.1 — Cargo.toml, one authoritative dependency step (fixes finding #14)

All dependencies needed anywhere in this plan, added once, here, so no later milestone has an
implicit dependency it never declared:

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "2"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4", "serde"] }
rusqlite = { version = "0.37", features = ["bundled"] }
fastembed = "5"
bincode = "1.3"

async-trait = "0.1"
tokio-util = { version = "0.7", features = ["rt"] }
tracing = "0.1"

# generic-http backend only — never pulled in by default builds
reqwest = { version = "0.12", features = ["json"], optional = true }
urlencoding = { version = "2", optional = true }

[dev-dependencies]
tokio = { version = "1", features = ["full", "test-util"] }
wiremock = "0.6"

[features]
generic-http = ["dep:reqwest", "dep:urlencoding"]
```

### 0.2 — Error variants
**File:** `src/error.rs`. Add to the existing enum (all current variants stay):
```rust
#[error("invalid argument: {0}")]
InvalidArgument(String),

#[error("vector store error: {0}")]
VectorStore(String),

#[error("internal error: {0}")]
Internal(String),

/// An operation failed, and the automatic rollback/compensation step that
/// was supposed to clean up after it *also* failed. Both messages are
/// preserved so an operator isn't left guessing which half broke.
#[error("operation failed: {operation}; compensation also failed: {compensation}")]
CompensationFailed { operation: String, compensation: String },
```

### 0.3 — The `VectorStore` trait, with `replace_all` — the fix for findings #1/#3/#4/#5

**File:** `src/vector_store/mod.rs` (new module).

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

/// One row's worth of data needed to rebuild an index entry from scratch.
/// This is the unit `replace_all` operates on.
#[derive(Debug, Clone)]
pub struct VectorEntry {
    pub id: Uuid,
    pub vector: Vec<f32>,
    pub metadata: HashMap<String, Value>,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    /// MUST be an idempotent upsert: inserting an id that already exists
    /// replaces its vector/metadata rather than erroring or duplicating.
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn contains(&self, id: Uuid) -> Result<bool>;
    /// Removes every entry.
    async fn clear(&self) -> Result<()>;
    /// Replaces the *entire* contents of this store with exactly `entries` —
    /// nothing more, nothing less. Any id currently present but absent from
    /// `entries` MUST be gone afterward; every id in `entries` MUST be
    /// present and correct afterward.
    ///
    /// This is the single reconciliation primitive used everywhere the
    /// engine needs to make the vector index agree with SQLite: restart
    /// backfill, forget-time cleanup after a partial failure, and M9's
    /// index rebuild all call this one method instead of each inventing
    /// their own "clear, then loop, then hope nothing raced" logic.
    ///
    /// Each backend implements this however is correct for that backend —
    /// `InMemoryVectorStore` does it as a single atomic pointer/lock swap;
    /// `GenericHttpVectorStore` does it as a real HTTP call to a
    /// server-side bulk-replace endpoint. Critically, the *caller* (the
    /// engine) never has to know which — it never constructs a new backend
    /// instance itself, so a rebuild can never silently swap a remote
    /// backend out for an in-memory one.
    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

### 0.4 — `InMemoryVectorStore`, fully inlined (fixes the self-containedness gap)

**File:** `src/vector_store/in_memory.rs`.

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use serde_json::Value;
use uuid::Uuid;
use crate::error::{MemoliteError, Result};
use super::{VectorEntry, VectorHit, VectorStore};

pub struct InMemoryVectorStore {
    dim: usize,
    data: RwLock<HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>,
}

impl InMemoryVectorStore {
    pub fn new(dim: usize) -> Self {
        Self { dim, data: RwLock::new(HashMap::new()) }
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 { return 0.0; }
        dot / (na * nb)
    }
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        if vector.len() != self.dim {
            return Err(MemoliteError::VectorStore(format!(
                "vector for {id} has dimension {} but store expects {}", vector.len(), self.dim
            )));
        }
        let mut guard = self.data.write().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))?;
        guard.insert(id, (vector.to_vec(), metadata));
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        let guard = self.data.read().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))?;
        let mut hits: Vec<VectorHit> = guard.iter()
            .map(|(id, (v, _))| VectorHit { id: *id, score: Self::cosine(query, v) })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(k);
        Ok(hits)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let mut guard = self.data.write().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))?;
        guard.remove(&id);
        Ok(())
    }

    async fn contains(&self, id: Uuid) -> Result<bool> {
        let guard = self.data.read().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))?;
        Ok(guard.contains_key(&id))
    }

    async fn clear(&self) -> Result<()> {
        let mut guard = self.data.write().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))?;
        guard.clear();
        Ok(())
    }

    /// Builds the replacement map fully off to the side, validating every
    /// entry, and only then swaps it in under one write-lock acquisition.
    /// Either every entry lands, or (on a validation error) none of the
    /// current contents are touched at all.
    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()> {
        let mut replacement = HashMap::with_capacity(entries.len());
        for e in entries {
            if e.vector.len() != self.dim {
                return Err(MemoliteError::VectorStore(format!(
                    "entry for {} has dimension {} but store expects {}", e.id, e.vector.len(), self.dim
                )));
            }
            if !e.vector.iter().all(|x| x.is_finite()) {
                return Err(MemoliteError::VectorStore(format!("entry for {} contains a non-finite value", e.id)));
            }
            replacement.insert(e.id, (e.vector, e.metadata));
        }
        let mut guard = self.data.write().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))?;
        *guard = replacement;
        Ok(())
    }

    fn dimension(&self) -> usize { self.dim }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nearest_vector_ranks_first() {
        let store = InMemoryVectorStore::new(2);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        store.insert(a, &[1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert(b, &[0.0, 1.0], HashMap::new()).await.unwrap();
        let hits = store.search(&[1.0, 0.0], 1).await.unwrap();
        assert_eq!(hits[0].id, a);
    }

    #[tokio::test]
    async fn insert_is_an_upsert() {
        let store = InMemoryVectorStore::new(2);
        let id = Uuid::new_v4();
        store.insert(id, &[1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert(id, &[0.0, 1.0], HashMap::new()).await.unwrap();
        let guard = store.data.read().unwrap();
        assert_eq!(guard.len(), 1);
    }

    #[tokio::test]
    async fn wrong_dimension_is_rejected_not_panicked() {
        let store = InMemoryVectorStore::new(3);
        let result = store.insert(Uuid::new_v4(), &[1.0, 0.0], HashMap::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn replace_all_removes_ids_absent_from_the_new_set() {
        let store = InMemoryVectorStore::new(2);
        let stale = Uuid::new_v4();
        store.insert(stale, &[1.0, 0.0], HashMap::new()).await.unwrap();
        let kept = Uuid::new_v4();
        store.replace_all(vec![VectorEntry { id: kept, vector: vec![0.0, 1.0], metadata: HashMap::new() }]).await.unwrap();
        assert!(!store.contains(stale).await.unwrap());
        assert!(store.contains(kept).await.unwrap());
    }

    #[tokio::test]
    async fn replace_all_leaves_store_untouched_on_validation_failure() {
        let store = InMemoryVectorStore::new(2);
        let original = Uuid::new_v4();
        store.insert(original, &[1.0, 0.0], HashMap::new()).await.unwrap();
        let bad = VectorEntry { id: Uuid::new_v4(), vector: vec![1.0], metadata: HashMap::new() }; // wrong dim
        let result = store.replace_all(vec![bad]).await;
        assert!(result.is_err());
        assert!(store.contains(original).await.unwrap());
    }
}
```

### 0.5 — `MemoryEngine`'s final shape (introduced now, used from here on — never retrofitted)

**File:** `src/engine.rs`. The current repo has `conn: Connection` and `embedder: Mutex<Embedder>`.
This step changes the struct to:

```rust
pub struct MemoryEngine {
    conn: std::sync::Mutex<rusqlite::Connection>,
    embedder: std::sync::Mutex<crate::embedder::Embedder>,
    vector_store: std::sync::RwLock<std::sync::Arc<dyn crate::vector_store::VectorStore>>,
    maintenance_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
```

**The one rule for `conn`, from here on:**
```rust
let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
// ...conn.prepare / conn.execute / conn.query_row...
// guard MUST be dropped before any .await in the same function
```

**The one rule for `vector_store`:**
```rust
// read (search/insert/delete/contains/dimension): clone the Arc, drop the lock, then await
let store = {
    let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
    std::sync::Arc::clone(&*guard)
};
store.search(&query_vec, k).await?;

// swap (only the M9 rebuild path does this, and only via replace_all on the *existing* Arc — see 0.7):
```
No exceptions to either rule appear anywhere below.

### 0.6 — Column-order constant, `MAX_CANDIDATES`
Unchanged from the repo's existing 10-column layout, extended once (in M6, Step-numbered there)
when `confidence` is added — there is only ever one live definition, introduced exactly where the
column is introduced, not forward-declared:
```rust
const MEMORY_COLUMNS: &str =
    "id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata";
```
```rust
// src/recall.rs
pub const MAX_CANDIDATES: usize = 500;
pub fn candidate_pool_size(limit: usize) -> usize {
    limit.saturating_mul(5).max(50).min(MAX_CANDIDATES)
}
```

### 0.7 — Migration runner
**File:** `src/migrations.rs`, registered in `src/lib.rs` as `mod migrations;` **in this same step**
(fixes finding #13 — every module gets registered where it's created, not left implicit):

```rust
pub fn run_migrations(conn: &mut rusqlite::Connection) -> crate::error::Result<()> {
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
        [],
    )?;
    fn has_table(tx: &rusqlite::Transaction, name: &str) -> crate::error::Result<bool> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![name], |r| r.get(0),
        )?;
        Ok(count > 0)
    }
    let tx = conn.transaction()?;
    if !has_table(&tx, "memories")? {
        tx.execute_batch(
            "CREATE TABLE memories (
                id              TEXT PRIMARY KEY,
                content         TEXT NOT NULL,
                type            TEXT NOT NULL CHECK(type IN ('semantic','episodic','procedural','working')),
                importance      REAL NOT NULL DEFAULT 0.5 CHECK(importance BETWEEN 0.0 AND 1.0),
                access_count    INTEGER NOT NULL DEFAULT 0,
                created_at      INTEGER NOT NULL,
                last_accessed   INTEGER NOT NULL,
                expires_at      INTEGER,
                superseded_by   TEXT REFERENCES memories(id),
                metadata        TEXT NOT NULL DEFAULT '{}'
            );"
        )?;
    }
    if !has_table(&tx, "embeddings")? {
        tx.execute_batch(
            "CREATE TABLE embeddings (
                memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
                vector    BLOB NOT NULL,
                dimension INTEGER NOT NULL
            );"
        )?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
         CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
         CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type);
         CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at);
         CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);"
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
        rusqlite::params![chrono::Utc::now().timestamp()],
    )?;
    tx.commit()?;
    Ok(())
}
```
**Honesty note (this is the exact scope stated in `ARCHITECTURE.md` later, matching finding #15):**
this runner repairs exactly one thing — missing expected tables (`memories`, `embeddings`) and
their indexes. It does not verify column types, constraints, foreign keys, or repair a
hand-edited schema. M6 adds a *second*, explicitly separate repair path for the one column it
introduces (`confidence`) — see M6, Step 6.2. There is never a claim of "self-heals any partial
schema."

### 0.8 — Vector-index resync via `replace_all` — the fix, applied

Because `replace_all` already exists on the trait (0.3) and already removes stale ids (0.4),
resync is now a single, honest call — no separate "upsert only" helper that findings #3/#4 could
catch drifting out of sync with reality:

```rust
/// Rebuilds `store`'s contents to exactly match every memory row still
/// present in SQLite that has an embedding — including superseded and
/// expired rows, which stay in the index until the row is actually
/// deleted by `forget()` or `purge_expired()`. Filtering those out is a
/// `recall_query` filter concern, not an indexing concern — this is what
/// makes `include_expired`/`include_superseded` able to return anything.
///
/// Read phase is fully synchronous and collects into a `Vec` before any
/// `.await`, so no SQLite borrow is ever held across an await point.
async fn resync_vector_index(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    store: &std::sync::Arc<dyn crate::vector_store::VectorStore>,
) -> Result<()> {
    use crate::vector_store::VectorEntry;

    let entries: Vec<VectorEntry> = {
        let conn = conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector, e.dimension, m.metadata FROM memories m JOIN embeddings e ON e.memory_id = m.id"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id_str, bytes, stored_dim, metadata_json) = row?;
            let id = Uuid::parse_str(&id_str)?;
            let vector: Vec<f32> = bincode::deserialize(&bytes).map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
            if vector.len() != stored_dim as usize {
                return Err(MemoliteError::VectorStore(format!(
                    "stored vector for {id} has dimension {} but its row says {}", vector.len(), stored_dim
                )));
            }
            let metadata: HashMap<String, Value> = serde_json::from_str(&metadata_json)?;
            out.push(VectorEntry { id, vector, metadata });
        }
        out
    }; // MutexGuard dropped here — before any await below

    store.replace_all(entries).await
}
```
`replace_all`'s own dimension/finiteness validation (0.4) means `resync_vector_index` doesn't need
a second copy of that check against the store's expected dimension — one validation site, not two
that can drift apart.

**Checkpoint 0:** `cargo build && cargo test` green. `VectorStore`, `InMemoryVectorStore`,
`migrations::run_migrations`, and `resync_vector_index` all exist and are unit-testable in
isolation, all before `MemoryEngine::open()` is touched in M3. This is the concrete fix for
finding #1: nothing in Step 0 references a type that doesn't exist yet in this document.

---

## Corrected build order

| Order | Milestone | What it adds |
|---|---|---|
| 0 | Step 0 | `VectorStore` trait + `replace_all`, `InMemoryVectorStore`, migrations, resync — all before the engine changes to use them |
| 1 | M3 | Engine rewired onto `Mutex<Connection>` + `RwLock<Arc<dyn VectorStore>>`, `open()`/`store()`/`recall()`/`forget()` using the real trait, restart backfill |
| 2 | M4 | Ranking (confidence stubbed at `1.0`), `RecallQuery`/`RecallItem`/`RecallResult` — **all fields the milestone's own tests need, added in this milestone, not split across M4/M7** |
| 3 | M5 | `StoreRequest`/`MemoryUpdate`/`ExpiryPolicy`, `update()` — the include-superseded/expired test that needs `update()` moves here too |
| 4 | M6 | `ConfidenceLevel`, confidence-column repair, ranking's stub replaced |
| 5 | M7 | Temporal querying — full feature set, correctly named, fields added alongside their builder methods |
| 6 | M6.5 | `Send + Sync` + no-lock-across-await audit (types already exist from Step 0 — pure verification) |
| 7 | M8 | Streaming ingestion, two explicit shutdown modes |
| 8 | M9 | Compression + index rebuild via `replace_all` (backend-agnostic, keeps superseded/expired rows) |
| 9 | M10 | Maintenance controller: fallible start, single-controller enforced, recoverable after a panic |
| 10 | M11 | `generic-http` backend wired via `open_with_store`, `search()` fully implemented, dimension-checked |
| 11 | M12 | Docs, packaging, honest limitations, release gate |

---

# M3 — Engine rewired onto the real trait

### Step 3.1 — `open()`
```rust
pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
    Self::open_with_store_internal(path, None).await
}

/// Internal constructor shared by `open()` and M11's `open_with_store()`.
/// `store_override`: `None` means "use the default `InMemoryVectorStore`
/// sized to the embedder's dimension." `Some(store)` means the caller
/// supplied a backend explicitly (M11) — in that case its dimension is
/// validated against the embedder's dimension before anything is written.
async fn open_with_store_internal(
    path: impl AsRef<std::path::Path>,
    store_override: Option<std::sync::Arc<dyn crate::vector_store::VectorStore>>,
) -> Result<Self> {
    let mut raw_conn = rusqlite::Connection::open(path)?;
    crate::migrations::run_migrations(&mut raw_conn)?;
    let embedder = crate::embedder::Embedder::new()?;
    let dim = embedder.dimension();

    let vector_store: std::sync::Arc<dyn crate::vector_store::VectorStore> = match store_override {
        Some(store) => {
            if store.dimension() != dim {
                return Err(MemoliteError::InvalidArgument(format!(
                    "supplied vector store has dimension {} but the embedder produces {}",
                    store.dimension(), dim
                )));
            }
            store
        }
        None => std::sync::Arc::new(crate::vector_store::InMemoryVectorStore::new(dim)),
    };
    let conn = std::sync::Mutex::new(raw_conn);
    resync_vector_index(&conn, &vector_store).await?;

    Ok(Self {
        conn,
        embedder: std::sync::Mutex::new(embedder),
        vector_store: std::sync::RwLock::new(vector_store),
        maintenance_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    })
}
```

### Step 3.2 — `store()` — uses the repo's existing `MemoryType::default_ttl()` (fixes finding #2)

The repo already has a correct `default_ttl()` on `MemoryType` (Working = 4 hours). v4 introduced
a redundant, wrong `default_ttl_days()` that zeroed Working's TTL. v5 deletes that idea entirely
and just calls the method that already exists:

```rust
pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String> {
    self.store_id(content, memory_type, importance).await.map(|id| id.to_string())
}

async fn store_id(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<Uuid> {
    if content.trim().is_empty() {
        return Err(MemoliteError::InvalidArgument("content must not be empty".into()));
    }
    if !(0.0..=1.0).contains(&importance) {
        return Err(MemoliteError::InvalidArgument("importance must be in [0.0, 1.0]".into()));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    let expires_at = now + memory_type.default_ttl(); // <- the repo's existing, correct method
    let metadata_json = serde_json::to_string(&HashMap::<String, Value>::new())?;

    let vector = {
        let mut embedder = self.embedder.lock().map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(content)?
    };
    let vector_bytes = bincode::serialize(&vector).map_err(|e| MemoliteError::EmbeddingEncode(e.to_string()))?;

    {
        let mut conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO memories (id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, NULL, ?7)",
            rusqlite::params![id.to_string(), content, memory_type.as_str(), importance, now.timestamp(), expires_at.timestamp(), metadata_json],
        )?;
        tx.execute("INSERT INTO embeddings (memory_id, vector, dimension) VALUES (?1, ?2, ?3)",
            rusqlite::params![id.to_string(), vector_bytes, vector.len() as i64])?;
        tx.commit()?;
    } // conn guard dropped before the await below

    let store = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    };
    if let Err(e) = store.insert(id, &vector, HashMap::new()).await {
        let compensation = {
            let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
            conn.and_then(|c| c.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id.to_string()]).map_err(Into::into)).err()
        };
        if let Some(compensation_err) = compensation {
            return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: compensation_err.to_string() });
        }
        return Err(e);
    }
    Ok(id)
}
```

### Step 3.3 — `recall()` — refetches after the access-count bump (fixes finding #17)

v4's `recall()` fetched a memory, bumped its access stats in SQLite, then returned the
*already-fetched, pre-bump* struct — so callers could never observe the very update the function
just made. v5 refetches:

```rust
pub async fn recall(&self, query_text: &str) -> Result<Vec<Memory>> {
    if query_text.trim().is_empty() {
        return Err(MemoliteError::InvalidArgument("query_text must not be empty".into()));
    }
    let query_vec = {
        let mut embedder = self.embedder.lock().map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(query_text)?
    };
    let store = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    };
    let hits = store.search(&query_vec, 20).await?;
    if hits.is_empty() { return Ok(Vec::new()); }

    let mut results = Vec::new();
    for hit in hits {
        if self.get(&hit.id.to_string()).await?.is_none() { continue; }
        self.update_access_stats(hit.id)?;
        // Refetch so the returned Memory reflects the access_count/last_accessed
        // bump this call itself just made, not the pre-bump snapshot.
        if let Some(mem) = self.get(&hit.id.to_string()).await? {
            results.push(mem);
        }
    }
    Ok(results)
}

fn update_access_stats(&self, id: Uuid) -> Result<()> {
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    conn.execute(
        "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp(), id.to_string()],
    )?;
    Ok(())
}
```

### Step 3.4 — `forget()` — reconciles via `replace_all`, not a stale-leaving upsert loop

```rust
pub async fn forget(&self, id: &str) -> Result<()> {
    {
        let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])?;
    }
    let store = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    };
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    if let Err(e) = store.delete(uuid).await {
        // reconcile via the trait's own replace_all — this actually removes
        // the stale id from the store, unlike an upsert-only resync.
        if let Err(resync_err) = resync_vector_index(&self.conn, &store).await {
            return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: resync_err.to_string() });
        }
        return Err(e);
    }
    Ok(())
}
```

### Steps 3.5–3.9 — Tests + checkpoint
- Store 3 unrelated facts + 1 relevant, `recall()` finds the relevant one.
- `recall()` on an empty engine → `Ok(vec![])`.
- `access_count` increases by exactly 1 after one `recall()` call, **and** the returned `Memory`
  reflects the new count (the exact assertion finding #17 said v4 would fail).
- `forget()` removes the memory from SQLite *and* the vector store.
- **Restart test**, exercising Step 0.7 + 0.8 + this milestone's `open()`: store 3, drop the
  engine, `open()` the same path again, `recall()` immediately — all 3 findable.
- **Stale-removal test** (the exact test finding #3 said v4 could never pass): manually insert a
  vector for an id that has no SQLite row, call anything that triggers `resync_vector_index`
  (e.g. force a `store.delete` failure via a test double in `forget()`), assert the orphan id is
  gone from the store afterward — provable now because `replace_all` actually removes it.
- Corrupt-row restart test: garbage blob in one `embeddings` row → `open()` returns `Err`.
- `cargo clippy` / `cargo fmt` clean.
- **Checkpoint:** `cargo test` green; every function called above was defined at or before this
  point in this document.

---

# M4 — Ranking + `recall_query()` (all struct fields land where their first user needs them)

### Step 4.1 — `src/ranking.rs`
```rust
pub fn decay_half_life_days(t: MemoryType) -> f64 {
    match t {
        MemoryType::Episodic => 14.0,
        MemoryType::Semantic => 693.0,
        MemoryType::Procedural => 1386.0,
        MemoryType::Working => 0.17,
    }
}
pub fn recency_factor(days_since_access: f64, memory_type: MemoryType) -> f32 {
    let days = days_since_access.max(0.0);
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

### Step 4.2 — `RecallQuery`/`RecallItem`/`RecallResult` — **only** the fields M4 itself uses
(fixes finding #10: v4 had M7 reference builder fields that were never added to the struct in M4.
v5's rule: a struct never gets a builder method for a field it doesn't have yet, in this document
or in the real one.)
```rust
#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub query_text: String,
    pub limit: usize,
    pub min_importance: f32,
    pub memory_types: Option<Vec<MemoryType>>,
    pub include_superseded: bool,
    pub include_expired: bool,
    pub metadata_equals: HashMap<String, Value>,
}
impl RecallQuery {
    pub fn new(query_text: &str) -> Self {
        Self { query_text: query_text.to_string(), limit: 10, min_importance: 0.0, memory_types: None, include_superseded: false, include_expired: false, metadata_equals: HashMap::new() }
    }
    pub fn limit(mut self, n: usize) -> Self { self.limit = n; self }
    pub fn min_importance(mut self, v: f32) -> Self { self.min_importance = v; self }
    pub fn memory_types(mut self, t: Vec<MemoryType>) -> Self { self.memory_types = Some(t); self }
    pub fn include_superseded(mut self, b: bool) -> Self { self.include_superseded = b; self }
    pub fn include_expired(mut self, b: bool) -> Self { self.include_expired = b; self }
    pub fn metadata_equals(mut self, k: &str, v: Value) -> Self { self.metadata_equals.insert(k.to_string(), v); self }
}
#[derive(Debug, Clone)]
pub struct RecallItem { pub memory: Memory, pub similarity: f32, pub score: f32 }
#[derive(Debug, Clone)]
pub struct RecallResult { pub items: Vec<RecallItem> }
```
M7 (below) adds `created_after`/`created_before`/`only_stale` to this **same** struct definition,
in the same step that adds their builder methods — not as a forward reference.

### Step 4.3 — `recall()` delegates; `recall_query()` — confidence stubbed at `1.0`
```rust
pub async fn recall(&self, query_text: &str) -> Result<Vec<Memory>> {
    Ok(self.recall_query(RecallQuery::new(query_text)).await?.items.into_iter().map(|i| i.memory).collect())
}

pub async fn recall_query(&self, query: RecallQuery) -> Result<RecallResult> {
    if query.limit == 0 { return Err(MemoliteError::InvalidArgument("limit must be > 0".into())); }
    if !query.min_importance.is_finite() { return Err(MemoliteError::InvalidArgument("min_importance must be finite".into())); }
    if query.query_text.trim().is_empty() { return Err(MemoliteError::InvalidArgument("query_text must not be empty".into())); }

    let query_vec = {
        let mut embedder = self.embedder.lock().map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(&query.query_text)?
    };
    let store = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    };
    let pool_size = crate::recall::candidate_pool_size(query.limit);
    let hits = store.search(&query_vec, pool_size).await?;

    let now = Utc::now();
    let mut scored = Vec::with_capacity(hits.len());
    for hit in hits {
        let Some(memory) = self.get(&hit.id.to_string()).await? else { continue };
        if memory.importance < query.min_importance { continue; }
        if let Some(types) = &query.memory_types { if !types.contains(&memory.memory_type) { continue; } }
        if memory.superseded_by.is_some() && !query.include_superseded { continue; }
        if memory.expires_at.map(|e| e < now).unwrap_or(false) && !query.include_expired { continue; }
        if !query.metadata_equals.iter().all(|(k, v)| memory.metadata.get(k) == Some(v)) { continue; }

        let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
        let recency = ranking::recency_factor(days_since_access, memory.memory_type);
        let reinforcement = ranking::reinforcement_factor(memory.access_count);
        let confidence_weight = 1.0_f32; // M6 Step 6.4 replaces this one line only
        let score = ranking::final_score(hit.score, memory.importance, recency, reinforcement, confidence_weight);
        scored.push(RecallItem { memory, similarity: hit.score, score });
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.memory.id.cmp(&b.memory.id)));
    scored.truncate(query.limit);
    for item in &scored { self.update_access_stats(item.memory.id)?; } // M6 Step 6.5 replaces this one line only
    Ok(RecallResult { items: scored })
}
```

### Steps 4.4–4.8 — Tests + checkpoint
- `.memory_types(...)` filter test; `.metadata_equals(...)` filter test.
- **`include_superseded`/`include_expired` test moved to M5** (fixes finding #9 — v4's version
  called `update()`, which doesn't exist until M5; testing it here would fail to compile). The
  test that *is* valid here: manually write a `superseded_by` value and a backdated `expires_at`
  directly via test-only SQL (not through any public API), then assert
  `recall_query().include_superseded(true)`/`.include_expired(true)` return it, and that the
  default (`false`) hides it. This exercises Step 0.8's "index keeps every row" policy without
  needing `update()`.
- `limit(0)` → `Err`; NaN `min_importance` → `Err`; `recall()`/`recall_query()` agree.
- **Checkpoint:** `cargo test` green; nothing in this milestone calls a method from M5–M11.

---

# M5 — `StoreRequest` / `MemoryUpdate` / `ExpiryPolicy` / `update()`

### Step 5.1 — `ExpiryPolicy`
```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpiryPolicy {
    TypeDefault,
    Custom(chrono::Duration),
    Never,
}
```

### Step 5.2 — `StoreRequest`
```rust
#[derive(Debug, Clone)]
pub struct StoreRequest {
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
    pub expiry: ExpiryPolicy,
    pub metadata: HashMap<String, Value>,
}
impl StoreRequest {
    pub fn new(content: &str, memory_type: MemoryType, importance: f32) -> Self {
        Self { content: content.to_string(), memory_type, importance, expiry: ExpiryPolicy::TypeDefault, metadata: HashMap::new() }
    }
    pub fn expiry(mut self, e: ExpiryPolicy) -> Self { self.expiry = e; self }
    pub fn metadata(mut self, m: HashMap<String, Value>) -> Self { self.metadata = m; self }
}
```
(`confidence` is added to this struct in M6, Step 6.1 — the same discipline as `RecallQuery`
above: the field arrives in the same step as its first real use, not before.)

### Step 5.3 — `MemoryUpdate`
```rust
#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
    pub new_content: Option<String>,
    pub new_importance: Option<f32>,
    pub new_metadata: Option<HashMap<String, Value>>,
    pub new_memory_type: Option<MemoryType>,
    pub new_expiry: Option<ExpiryPolicy>,
}
```
`id` is never a field here — it's immutable, always.

### Step 5.4 — `store_with_options()` — typed `Uuid` core, no string-fallback ever possible
```rust
pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String> {
    self.store_with_options(StoreRequest::new(content, memory_type, importance)).await
}
pub async fn store_with_options(&self, request: StoreRequest) -> Result<String> {
    self.store_with_options_id(request).await.map(|id| id.to_string())
}
async fn store_with_options_id(&self, request: StoreRequest) -> Result<Uuid> {
    if request.content.trim().is_empty() { return Err(MemoliteError::InvalidArgument("content must not be empty".into())); }
    if !(0.0..=1.0).contains(&request.importance) { return Err(MemoliteError::InvalidArgument("importance must be in [0.0, 1.0]".into())); }
    if let ExpiryPolicy::Custom(d) = request.expiry {
        if d <= chrono::Duration::zero() { return Err(MemoliteError::InvalidArgument("Custom expiry duration must be positive".into())); }
    }
    let id = Uuid::new_v4();
    let now = Utc::now();
    let expires_at: Option<DateTime<Utc>> = match request.expiry {
        ExpiryPolicy::Never => None,
        ExpiryPolicy::Custom(d) => Some(now + d),
        ExpiryPolicy::TypeDefault => Some(now + request.memory_type.default_ttl()), // reuses the repo's existing method
    };
    let metadata_json = serde_json::to_string(&request.metadata)?;
    let vector = {
        let mut embedder = self.embedder.lock().map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(&request.content)?
    };
    let vector_bytes = bincode::serialize(&vector).map_err(|e| MemoliteError::EmbeddingEncode(e.to_string()))?;
    {
        let mut conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO memories (id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, NULL, ?7)",
            rusqlite::params![id.to_string(), request.content, request.memory_type.as_str(), request.importance,
                now.timestamp(), expires_at.map(|e| e.timestamp()), metadata_json],
        )?;
        tx.execute("INSERT INTO embeddings (memory_id, vector, dimension) VALUES (?1, ?2, ?3)",
            rusqlite::params![id.to_string(), vector_bytes, vector.len() as i64])?;
        tx.commit()?;
    }
    let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
    if let Err(e) = store.insert(id, &vector, request.metadata.clone()).await {
        let compensation = { let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
            conn.and_then(|c| c.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id.to_string()]).map_err(Into::into)).err() };
        if let Some(ce) = compensation { return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: ce.to_string() }); }
        return Err(e);
    }
    Ok(id)
}
```

### Step 5.5 — `update()` — rejects updating an already-expired memory unless a new expiry is given
(fixes finding #16: v4 silently revived expired memories with a 1-second Custom duration)
```rust
pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<String> {
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    let old = self.get(id).await?.ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;

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
        Some(expiry) => {
            let remaining = expiry.signed_duration_since(now);
            // reachable only when NOT expired, since the check above already
            // rejected the expired+no-new-expiry case
            ExpiryPolicy::Custom(remaining)
        }
    });
    request.metadata = update.new_metadata.unwrap_or_else(|| old.metadata.clone());

    let new_uuid = self.store_with_options_id(request).await?;
    if let Err(e) = self.mark_superseded(&uuid, &new_uuid.to_string()) {
        let del_err = { let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
            conn.and_then(|c| c.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![new_uuid.to_string()]).map_err(Into::into)).err() };
        let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
        let vec_err = store.delete(new_uuid).await.err(); // exact new id, never the old one
        if del_err.is_some() || vec_err.is_some() {
            return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: format!("{:?} / {:?}", del_err, vec_err) });
        }
        return Err(e);
    }
    Ok(new_uuid.to_string())
}

fn mark_superseded(&self, old_id: &Uuid, new_id: &str) -> Result<()> {
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let affected = conn.execute("UPDATE memories SET superseded_by = ?1 WHERE id = ?2", rusqlite::params![new_id, old_id.to_string()])?;
    if affected == 0 { return Err(MemoliteError::NotFound(old_id.to_string())); }
    Ok(())
}
```

### Steps 5.6–5.14 — Tests + checkpoint
- Basic `store_with_options` round-trip.
- `update()` with `new_metadata: None` preserves old metadata exactly.
- `update()` on a memory created with `ExpiryPolicy::Never`: replacement also has `expires_at: None`.
- `update()` on a nearly-expired-but-not-yet-expired memory: remaining TTL carried forward ≈ what
  was left.
- **(fixes finding #16's counterpart test)** `update()` on an already-expired memory with no
  `new_expiry` → `Err(InvalidArgument)`; the same call with `new_expiry: Some(ExpiryPolicy::Never)`
  succeeds and produces a non-expiring replacement.
- **(the test moved here from M4, fixes finding #9)** `include_superseded`/`include_expired`
  integration test: `store()` a memory, `update()` it (creating a real superseded original via the
  public API this time), call `recall_query().include_superseded(true)` and confirm the original is
  returned — provable now because `update()` exists at this point in the document.
- Compensation test: force `mark_superseded` to fail — assert the replacement is gone from *both*
  SQLite and the vector store, and specifically that the **original** memory's vector is untouched.
- `Custom(<= 0)` → `Err`; `update()` on nonexistent id → `Err(NotFound)`; superseded chain `A -> B
  -> C` resolves; no builder method can change `id`.
- **Checkpoint:** `cargo test` green, zero regressions M3–M4.

---

# M6 — Confidence scoring

### Step 6.1 — `ConfidenceLevel`, and `confidence` added to `StoreRequest`/`Memory` together
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceLevel { Explicit, Inferred, Reinforced }
impl ConfidenceLevel {
    pub fn as_str(&self) -> &'static str { match self { Self::Explicit => "explicit", Self::Inferred => "inferred", Self::Reinforced => "reinforced" } }
    pub fn parse_str(s: &str) -> Result<Self, InvalidConfidence> {
        match s { "explicit" => Ok(Self::Explicit), "inferred" => Ok(Self::Inferred), "reinforced" => Ok(Self::Reinforced), other => Err(InvalidConfidence(other.to_string())) }
    }
    pub fn weight(&self) -> f32 { match self { Self::Explicit | Self::Reinforced => 1.0, Self::Inferred => 0.7 } }
    pub fn maybe_promote(self, access_count: u32) -> Self { if self == Self::Inferred && access_count >= 5 { Self::Reinforced } else { self } }
}
```
Add to `StoreRequest` (Step 5.2's struct): `pub confidence: ConfidenceLevel` with
`ConfidenceLevel::Explicit` as the default in `StoreRequest::new`, plus a
`.with_confidence(c)` builder method. Add to `MemoryUpdate` (Step 5.3's struct):
`pub new_confidence: Option<ConfidenceLevel>`, defaulting `Inferred` when unset in `update()`'s
body. Add `confidence: ConfidenceLevel` to `Memory`, and `confidence` to `MEMORY_COLUMNS`
(0.6's constant becomes 11 columns from this step forward).

### Step 6.2 — Separate, explicitly-scoped repair migration for this one column
```rust
pub fn repair_confidence_column(conn: &mut rusqlite::Connection) -> crate::error::Result<()> {
    let tx = conn.transaction()?;
    let has_col: bool = {
        let mut stmt = tx.prepare("PRAGMA table_info(memories)")?;
        let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        cols.iter().any(|c| c == "confidence")
    };
    if !has_col {
        tx.execute_batch(
            "ALTER TABLE memories ADD COLUMN confidence TEXT NOT NULL DEFAULT 'explicit'
                CHECK(confidence IN ('explicit', 'inferred', 'reinforced'));"
        )?;
    }
    tx.commit()?;
    Ok(())
}
```
Call this from `run_migrations()` (0.7) right after the existing table-creation block, on every
open — fixes finding #15's honesty requirement by keeping this repair's scope named and separate
from the general table-existence check, rather than folding it in silently.

### Step 6.3 — `store_with_options_id` writes `confidence` into the INSERT (one-line addition to
Step 5.4's SQL and params — `confidence` column added, `request.confidence.as_str()` bound).

### Step 6.4 — Swap the ranking stub — the one line Step 4.3 flagged
```rust
let confidence_weight = memory.confidence.weight(); // replaces `1.0_f32`
```

### Step 6.5 — Atomic bump-and-promote replaces `update_access_stats` at its one call site inside
`recall_query()` (Step 4.3's loop):
```rust
fn update_access_stats_and_maybe_promote(&self, id: Uuid) -> Result<()> {
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    conn.execute(
        "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1,
             confidence = CASE WHEN confidence = 'inferred' AND access_count + 1 >= 5 THEN 'reinforced' ELSE confidence END
         WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp(), id.to_string()],
    )?;
    Ok(())
}
```
`recall()`'s plain `update_access_stats` (M3, Step 3.3) is untouched — it's a different call site
serving a different function and is not required to know about confidence promotion.

### Steps 6.6–6.10 — Tests + checkpoint
- Confidence repair test: create `memories` without `confidence` via raw SQL, run migrations,
  assert the column now exists with the right `CHECK`.
- Idempotent reopen: run migrations twice, no error, one row in `schema_migrations`.
- Round-trip per `ConfidenceLevel`; `Inferred` scores lower than an otherwise-identical `Explicit`
  memory; recalling an `Inferred` memory via `recall_query()` exactly 5 times promotes it to
  `Reinforced`.
- `cargo clippy`/`fmt` clean. **Checkpoint:** `cargo test` green through M6.

---

# M7 — Temporal querying (fields land with their builder methods, in the same step)

### Step 7.1 — Extend `RecallQuery` (Step 4.2's struct) and add the matching builder methods
**in the same step**, plus the validation that was missing in v4 (fixes finding #10):
```rust
// added fields on the existing struct:
pub created_after: Option<DateTime<Utc>>,
pub created_before: Option<DateTime<Utc>>,
pub only_stale: bool,
// initialized to None/None/false in RecallQuery::new()

impl RecallQuery {
    pub fn created_after(mut self, t: DateTime<Utc>) -> Self { self.created_after = Some(t); self }
    pub fn created_before(mut self, t: DateTime<Utc>) -> Self { self.created_before = Some(t); self }
    pub fn only_stale(mut self, b: bool) -> Self { self.only_stale = b; self }
}
```
`recall_query()` (Step 4.3) gains, right after its existing filters:
```rust
if let (Some(after), Some(before)) = (query.created_after, query.created_before) {
    if after > before {
        return Err(MemoliteError::InvalidArgument("created_after must not exceed created_before".into()));
    }
}
// ...inside the per-hit loop:
if let Some(after) = query.created_after { if memory.created_at < after { continue; } }
if let Some(before) = query.created_before { if memory.created_at > before { continue; } }
if query.only_stale {
    let stale_cutoff_days = ranking::decay_half_life_days(memory.memory_type) * 2.0;
    let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
    if days_since_access < stale_cutoff_days { continue; }
}
```

### Step 7.2 — Time-range and audit queries, correctly named (fixes finding #15's other half)
```rust
pub async fn query_by_time_range(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Memory>> {
    if start > end { return Err(MemoliteError::InvalidArgument("start must not be after end".into())); }
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE created_at >= ?1 AND created_at <= ?2 ORDER BY created_at ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![start.timestamp(), end.timestamp()], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Named for exactly what it measures: every memory *created or accessed*
/// since `since`. Recalling a memory bumps `last_accessed`, so a pure read
/// legitimately makes a memory show up here — this is documented as
/// intentional (recall is itself a signal something is still relevant),
/// not renamed to imply it tracks content edits. A future `updated_at`
/// column (bumped only by `update()`, not by `recall()`) is the correct
/// way to add a true "what was edited" query later, and is left as a
/// documented, separate follow-up rather than overloading this method.
pub async fn created_or_accessed_since(&self, since: DateTime<Utc>) -> Result<Vec<Memory>> {
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE created_at >= ?1 OR last_accessed >= ?1 ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![since.timestamp()], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub async fn find_stale_memories(&self) -> Result<Vec<Memory>> {
    let now = Utc::now();
    let active = self.get_active_memories()?; // defined here, reused unchanged by M9
    Ok(active.into_iter().filter(|m| {
        let cutoff_days = ranking::decay_half_life_days(m.memory_type) * 2.0;
        let days_since_access = (now - m.last_accessed).num_seconds() as f64 / 86400.0;
        days_since_access >= cutoff_days
    }).collect())
}

fn get_active_memories(&self) -> Result<Vec<Memory>> {
    let now = Utc::now().timestamp();
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE superseded_by IS NULL AND (expires_at IS NULL OR expires_at >= ?1)");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![now], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub async fn find_superseded_chain(&self, id: &str) -> Result<Vec<Memory>> {
    let start_uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    let mut chain = Vec::new();
    let mut current = self.get(id).await?.ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;
    chain.push(current.clone());
    let mut guard_iterations = 0usize;
    while let Some(next_id) = current.superseded_by {
        guard_iterations += 1;
        if guard_iterations > 10_000 {
            return Err(MemoliteError::Internal(format!("superseded_by cycle detected starting from {start_uuid}")));
        }
        // fixes finding #8: get() takes &str, next_id is a Uuid — convert explicitly
        let Some(next) = self.get(&next_id.to_string()).await? else { break };
        chain.push(next.clone());
        current = next;
    }
    Ok(chain)
}
```

### Tests + checkpoint
- `start > end` → `Err`; `created_after > created_before` → `Err` (this validation was entirely
  absent in v4 — fixes finding #10's other half).
- Backdated fixtures (written directly via test-only SQL, never `sleep`) return in correct order.
- `created_or_accessed_since` picks up both newly-created and newly-re-accessed memories, and its
  doc comment is checked against this exact behavior in the test's own description.
- `find_stale_memories`/`only_stale` agree on the same cutoff rule.
- Superseded chain of length 3 resolves; a synthetic cyclic `superseded_by` (test-only SQL) trips
  the guard and returns `Err`.
- **Checkpoint:** `cargo test` green through M7.

---

# M6.5 — Concurrency proof (pure verification — types have existed since Step 0)

```rust
fn assert_send_sync<T: Send + Sync>() {}
#[test]
fn memory_engine_is_send_sync() { assert_send_sync::<memolite::MemoryEngine>(); }
```
Audit every function above for: no `.lock()`/`.read()`/`.write()` guard held across an `.await`.
`cargo clippy` (which flags this) clean. **Checkpoint:** `Send + Sync` proven.

---

# M8 — Streaming ingestion

`StreamIngestor::spawn` takes `Arc<MemoryEngine>`, an mpsc channel, and a `CancellationToken`
(from `tokio-util`, added in Step 0.1). The loop checks `cancel.is_cancelled()` between
`rx.recv().await` calls. `shutdown_now()` cancels immediately without draining the backlog.
`finish()` drops this ingestor's sender and waits for the channel to close naturally, draining the
backlog once every cloned sender handle is also dropped by the caller — standard mpsc semantics,
stated explicitly in the doc comment. Every insert inside the loop goes through
`store_with_options`/`store_with_options_id` from M5/M6 — no separate insert path to keep in sync.

**Module registration (fixes finding #13):** `mod streaming;` added to `lib.rs` in this same step,
exporting only `StreamIngestor`, `IngestReport`, and the sender handle type.

### Tests + checkpoint
- `SentenceBuffer::feed` unicode/boundary tests.
- `IngestReport` accuracy, including a forced per-chunk failure (empty content) leaving
  `failed == 1` while the loop keeps consuming.
- `finish()`: send 5, drop every sender clone, `received == 5 && stored == 5`.
- `shutdown_now()` against a slow storage test-double: prompt return, `received <= 5`.
- Backpressure test, `buffer_size = 1`, all 5 eventually land via `finish()`.
- `spawn(engine, 0)` → `Err(InvalidArgument)`.
- **Checkpoint:** streamed content retrievable end-to-end; both shutdown modes verified by test.

---

# M9 — Compression + index rebuild via `replace_all` (fixes findings #4, #5, #7, #18)

### Eligibility
```rust
pub fn is_compression_eligible(mem: &Memory) -> bool {
    let age_days = (Utc::now() - mem.created_at).num_days();
    let not_expired = mem.expires_at.map(|e| e >= Utc::now()).unwrap_or(true);
    mem.memory_type == MemoryType::Episodic && age_days > 14 && mem.importance < 0.3 && mem.superseded_by.is_none() && not_expired
}
```

### `get_embedding()` — call sites fixed to actually `.await` it (fixes finding #7)
`get_embedding()` is already declared `async` in the current repo (verified) — the fix here is
purely at the call site, which v4 forgot the `.await` on:
```rust
let Some(vector) = self.get_embedding(&mem.id.to_string()).await? else { continue };
```

### Rebuild — backend-agnostic, keeps every row, via `replace_all` (fixes findings #4 and #5 at
their shared root: the engine never constructs a new backend instance itself)
```rust
/// Rebuilds the *entire* index — including superseded/expired rows, per
/// Step 0.8's policy, so include_superseded/include_expired keep working
/// after a rebuild — by calling replace_all on the live store. Because
/// replace_all is a trait method, this works identically whether the live
/// store is InMemoryVectorStore or GenericHttpVectorStore (M11): the engine
/// never constructs `InMemoryVectorStore::new(...)` here, so a rebuild can
/// never silently swap a remote backend out for an in-memory one.
async fn rebuild_vector_index(&self) -> Result<()> {
    use crate::vector_store::VectorEntry;
    let entries: Vec<VectorEntry> = {
        let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector, m.metadata FROM memories m JOIN embeddings e ON e.memory_id = m.id"
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id_str, bytes, metadata_json) = row?;
            let id = Uuid::parse_str(&id_str)?;
            let vector: Vec<f32> = bincode::deserialize(&bytes).map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
            let metadata: HashMap<String, Value> = serde_json::from_str(&metadata_json)?;
            out.push(VectorEntry { id, vector, metadata });
        }
        out
    };
    let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
    store.replace_all(entries).await // validated per-entry inside the backend's own implementation
}
```
> **Honesty note:** `replace_all` is exact/atomic for `InMemoryVectorStore` (one lock-guarded map
> swap after the whole replacement is validated). For `GenericHttpVectorStore` (M11), atomicity
> depends entirely on whether the remote server's bulk-replace endpoint is itself atomic — this
> crate cannot promise more than the backend it's talking to actually provides, and
> `ARCHITECTURE.md` states this plainly rather than implying uniform atomicity.

### `compress_old_memories()` — Semantic storage type, and an honest note on cross-step atomicity
(fixes finding #18: rather than overselling atomicity, this states plainly what is and isn't
transactional, and *does* tighten the one part that can be made transactional cheaply — marking
every original superseded happens in a single SQLite transaction, same as before; the remaining
gap is between "new summary committed" and "originals marked superseded," which compensation
already unwinds on failure)
```rust
pub async fn compress_old_memories(&self) -> Result<usize> {
    let candidates: Vec<Memory> = self.get_episodic_memories_older_than(14)?.into_iter().filter(compression::is_compression_eligible).collect();
    let ids: Vec<Uuid> = candidates.iter().map(|m| m.id).collect();
    let mut with_vectors = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(v) = self.get_embedding(&id.to_string()).await? { with_vectors.push((*id, v)); }
    }
    let clusters = compression::greedy_cluster(&with_vectors, 0.85);
    let mut compressed_count = 0;

    for cluster in clusters.into_iter().filter(|c| c.member_ids.len() >= 3) {
        let members: Vec<Memory> = candidates.iter().filter(|m| cluster.member_ids.contains(&m.id)).cloned().collect();
        let result = compression::summarize_cluster(&members, 0.85)?;

        let mut metadata = HashMap::new();
        metadata.insert("compression.original_ids".into(), serde_json::json!(result.original_ids.iter().map(Uuid::to_string).collect::<Vec<_>>()));
        metadata.insert("compression.algorithm_version".into(), serde_json::json!(compression::COMPRESSION_ALGORITHM_VERSION));

        // Semantic, not Episodic: a summary sits under Semantic's 365-day
        // default TTL so it doesn't quietly expire on the episodic TTL
        // while the (already-superseded) originals sit inert in SQLite.
        let mut request = StoreRequest::new(&result.summary_content, MemoryType::Semantic, 0.3).with_confidence(ConfidenceLevel::Inferred);
        request.metadata = metadata;

        let new_uuid = self.store_with_options_id(request).await?;
        if let Err(e) = self.mark_all_superseded(&result.original_ids, &new_uuid.to_string()) {
            let del_err = { let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
                conn.and_then(|c| c.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![new_uuid.to_string()]).map_err(Into::into)).err() };
            let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
            let vec_err = store.delete(new_uuid).await.err();
            if del_err.is_some() || vec_err.is_some() {
                return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: format!("{:?} / {:?}", del_err, vec_err) });
            }
            return Err(e);
        }
        compressed_count += members.len();
    }
    Ok(compressed_count)
}

fn mark_all_superseded(&self, old_ids: &[Uuid], new_id: &str) -> Result<()> {
    let mut conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let tx = conn.transaction()?;
    for old_id in old_ids {
        tx.execute("UPDATE memories SET superseded_by = ?1 WHERE id = ?2", rusqlite::params![new_id, old_id.to_string()])?;
    }
    tx.commit()?;
    Ok(())
}
```
**Documented honestly in `ARCHITECTURE.md` (M12):** creating the summary and marking every
original superseded are two separate SQLite writes. A crash between them is covered by
compensation *only* for the failure-returns-an-error case (the code above); a hard process kill
between the two writes is not automatically rolled back, and is a known, stated limitation — not
claimed as crash-atomic. A future version could close this by writing both in one transaction and
inserting the vector afterward with reconciliation; that is left as an explicit backlog item, not
implied as already solved.

### Tests + checkpoint
- Eligibility boundary, greedy-clustering, empty-cluster → `Err`.
- Integration: 3 similar low-importance episodic memories → `compress_old_memories()` returns `3`.
- **Rebuild-consistency test (replaces v4's now-impossible-to-write InMemory-only atomicity test):**
  after compression, call `rebuild_vector_index()`, then run `recall_query().include_superseded(true)`
  and confirm the superseded originals are still findable — proving the rebuild preserved rows per
  Step 0.8's policy (fixes finding #4, concretely).
- **Backend-preservation test (fixes finding #5, concretely):** open an engine with a test double
  `VectorStore` implementation (not `InMemoryVectorStore`), call `rebuild_vector_index()`, and
  assert (via a call-count instrumented in the test double) that `replace_all` was called on *that
  same instance* — never that a new `InMemoryVectorStore` silently appeared.
- Compressed summaries are `MemoryType::Semantic`; `recall_query()` past the episodic TTL window
  still returns the summary.
- Dimension/finite-invalid embedding in the candidate set → `Err`, not a silent drop (validated
  inside `replace_all`/`insert`, not duplicated ad hoc).
- Restart test: after a successful compression run, reopen (exercises Step 0.8's resync), confirm
  vector-store contents match SQLite's full row set.
- **Checkpoint:** `get_embedding` calls all correctly awaited; rebuild works identically across
  backends; compression's cross-transaction gap is documented, not hidden.

---

# M10 — Maintenance controller

```rust
pub struct MaintenanceConfig { pub purge_interval: std::time::Duration, pub compress_interval: std::time::Duration }
pub struct MaintenanceHandle { cancel: tokio_util::sync::CancellationToken, join: tokio::task::JoinHandle<()>, running_flag: std::sync::Arc<std::sync::atomic::AtomicBool> }

impl MemoryEngine {
    pub fn start_maintenance(self: &std::sync::Arc<Self>, config: MaintenanceConfig) -> Result<MaintenanceHandle> {
        if config.purge_interval.is_zero() || config.compress_interval.is_zero() {
            return Err(MemoliteError::InvalidArgument("maintenance intervals must be non-zero".into()));
        }
        if self.maintenance_running.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(MemoliteError::InvalidArgument("maintenance is already running on this engine".into()));
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let weak = std::sync::Arc::downgrade(self);
        let running_flag = std::sync::Arc::clone(&self.maintenance_running);
        let running_flag_for_task = std::sync::Arc::clone(&running_flag);
        let join = tokio::spawn(async move {
            let now = tokio::time::Instant::now();
            let mut purge_tick = tokio::time::interval_at(now + config.purge_interval, config.purge_interval);
            purge_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut compress_tick = tokio::time::interval_at(now + config.compress_interval, config.compress_interval);
            compress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    _ = purge_tick.tick() => { let Some(e) = weak.upgrade() else { break }; if let Err(err) = e.purge_expired().await { tracing::warn!(error = %err, "background purge failed; continuing"); } }
                    _ = compress_tick.tick() => { let Some(e) = weak.upgrade() else { break }; if let Err(err) = e.compress_old_memories().await { tracing::warn!(error = %err, "background compression failed; continuing"); } }
                }
            }
            running_flag_for_task.store(false, std::sync::atomic::Ordering::SeqCst);
        });
        Ok(MaintenanceHandle { cancel, join, running_flag })
    }
}
impl MaintenanceHandle {
    /// Always releases the single-controller flag, whether the task exited
    /// cleanly, was already dead, or panicked.
    pub async fn shutdown(self) -> Result<()> {
        self.cancel.cancel();
        let result = self.join.await;
        self.running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        result.map_err(|e| MemoliteError::Internal(e.to_string()))
    }
}
```
`purge_expired()` follows `forget()`'s reconciliation shape (Step 3.4): delete from SQLite,
best-effort delete from the vector store per id, resync-via-`replace_all` on failure.

### Tests + checkpoint
- Paused-clock interval tests; cancellation exits promptly.
- Zero-interval config → `Err`, no panic.
- Second `start_maintenance` while running → `Err`; succeeds after `shutdown()`.
- Panic-recovery test: force a panic inside the loop, `shutdown()` returns `Err`, a subsequent
  `start_maintenance()` on the same engine then succeeds.
- Engine-drop test: keep the handle, drop the last `Arc<MemoryEngine>`, advance time, `shutdown()`
  — task had already exited via `upgrade()` failure.
- Missed-tick `Skip` test; concurrent store/recall during a tick doesn't deadlock.
- **Checkpoint:** purge/compression opt-in only, fallible start, single controller, recoverable.

---

# M11 — `generic-http` vector backend, wired into the public API (fixes findings #6, #11, #12)

### Wire contract
```
POST {base_url}/search      Request:  { "vector": [f32,...], "k": usize }
                             Response: [ { "id": "uuid-string", "score": f32 }, ... ]
PUT  {base_url}/vectors/{id}          { "vector": [...], "metadata": {...} }
DELETE {base_url}/vectors/{id}
GET  {base_url}/vectors/{id}          200 = present, 404 = absent, anything else = Err
DELETE {base_url}/vectors
POST {base_url}/vectors:replace_all   { "entries": [ { "id": ..., "vector": [...], "metadata": {...} }, ... ] }
```

### Adapter — one client with the timeout configured once (fixes finding #11), `search()` fully
implemented with dimension checking, truncation, and a deterministic tie-break (fixes finding #12)
```rust
pub struct GenericHttpVectorStore { client: reqwest::Client, base_url: String, dim: usize }

impl GenericHttpVectorStore {
    pub fn new(base_url: impl Into<String>, dim: usize) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(Self { client, base_url: base_url.into(), dim })
    }
}

#[async_trait]
impl VectorStore for GenericHttpVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        if vector.len() != self.dim { return Err(MemoliteError::VectorStore(format!("vector for {id} has dimension {} but store expects {}", vector.len(), self.dim))); }
        self.client.put(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .json(&serde_json::json!({ "vector": vector, "metadata": metadata })).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        if query.len() != self.dim {
            return Err(MemoliteError::VectorStore(format!("query vector has dimension {} but store expects {}", query.len(), self.dim)));
        }
        #[derive(serde::Deserialize)] struct RawHit { id: String, score: f32 }
        const MAX_RESULTS: usize = 10_000; // documented ceiling against a misbehaving/huge response

        let resp = self.client.post(format!("{}/search", self.base_url))
            .json(&serde_json::json!({ "vector": query, "k": k })).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        let raw: Vec<RawHit> = resp.json().await.map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        if raw.len() > MAX_RESULTS {
            return Err(MemoliteError::VectorStore(format!("search returned {} results, exceeding the {} cap", raw.len(), MAX_RESULTS)));
        }

        let mut hits = Vec::with_capacity(raw.len());
        for h in raw {
            let id = Uuid::parse_str(&h.id).map_err(|e| MemoliteError::VectorStore(format!("invalid id in search response: {e}")))?;
            if !h.score.is_finite() { return Err(MemoliteError::VectorStore(format!("non-finite score for {id}"))); }
            hits.push(VectorHit { id, score: h.score });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id))); // deterministic tie-break
        hits.truncate(k); // server may over-return; the contract guarantees at most k out of this call
        Ok(hits)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        self.client.delete(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string()))).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn contains(&self, id: Uuid) -> Result<bool> {
        let resp = self.client.get(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string()))).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        match resp.status() {
            reqwest::StatusCode::OK => Ok(true),
            reqwest::StatusCode::NOT_FOUND => Ok(false),
            _ => Err(MemoliteError::VectorStore(match resp.error_for_status() { Ok(_) => "unexpected status".into(), Err(e) => e.to_string() })),
        }
    }

    async fn clear(&self) -> Result<()> {
        self.client.delete(format!("{}/vectors", self.base_url)).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()> {
        for e in &entries {
            if e.vector.len() != self.dim {
                return Err(MemoliteError::VectorStore(format!("entry for {} has dimension {} but store expects {}", e.id, e.vector.len(), self.dim)));
            }
        }
        let payload: Vec<_> = entries.iter().map(|e| serde_json::json!({ "id": e.id.to_string(), "vector": e.vector, "metadata": e.metadata })).collect();
        self.client.post(format!("{}/vectors:replace_all", self.base_url))
            .json(&serde_json::json!({ "entries": payload })).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    fn dimension(&self) -> usize { self.dim }
}
```
`Debug` is hand-written to redact any API-key field if one is added later.

### Public constructor — the missing piece that fixes finding #6
```rust
impl MemoryEngine {
    /// Opens the engine backed by a caller-supplied `VectorStore` instead of
    /// the default `InMemoryVectorStore`. The supplied store's dimension is
    /// validated against the embedder's dimension before anything is
    /// written (see Step 3.1's `open_with_store_internal`, which this calls
    /// directly — there is exactly one `open` code path, parameterized by
    /// an `Option<Arc<dyn VectorStore>>`, not two divergent ones).
    pub async fn open_with_store(
        path: impl AsRef<std::path::Path>,
        store: std::sync::Arc<dyn crate::vector_store::VectorStore>,
    ) -> Result<Self> {
        Self::open_with_store_internal(path, Some(store)).await
    }
}
```

### Tests + checkpoint
- `wiremock`-based unit tests, including `search()` against a real JSON body.
- Dimension-mismatch test: `search()`/`insert()` called with the wrong-length vector → `Err`
  before any HTTP call is made.
- Over-length response test: a mocked response with more than `MAX_RESULTS` entries → `Err`.
- `contains()`: a `500` response surfaces as `Err`, not `Ok(false)`.
- `replace_all()` integration test against wiremock: insert 3 via `insert`, `replace_all` with 2 of
  them plus a new one, confirm the dropped one is gone and the new one is present.
- `open_with_store()` end-to-end test: open with a `GenericHttpVectorStore` pointed at a wiremock
  server, `store()`, `recall()` round-trips through the HTTP backend.
- `cargo build` (default features) does not pull in `reqwest` — verified via `cargo tree`.
- `cargo test --all-features` passes with `search()` fully implemented, no `todo!()` anywhere.
- **Checkpoint:** `--all-features` passes; `open_with_store()` is reachable and tested; the HTTP
  backend is provably usable through the public API, not just defined and unused.

---

# M12 — Final polish, docs, release gate

### `ARCHITECTURE.md` states, in these exact terms:
- Concurrency model: `Mutex<Connection>` + `Mutex<Embedder>` + `RwLock<Arc<dyn VectorStore>>`,
  present from Step 0. Lock-then-clone-then-drop-then-await discipline, uniform throughout;
  `Send + Sync` proven at M6.5.
- **Single reconciliation primitive:** `VectorStore::replace_all` is the only mechanism that makes
  the index agree with SQLite — used identically by restart backfill, `forget`/`purge_expired`
  failure recovery, and M9's rebuild. No backend-specific reconciliation logic lives in the engine.
- Migration scope: table/index existence (Step 0.7) plus one explicitly separate, named repair for
  the `confidence` column (Step 6.2). Nothing else is verified or repaired.
- Vector-index policy: holds every memory row with an embedding, including superseded/expired,
  until the row is actually deleted; filtering is `recall_query`'s job.
- `replace_all`'s atomicity is exact for `InMemoryVectorStore`; for `GenericHttpVectorStore` it is
  only as atomic as the remote `/vectors:replace_all` endpoint actually is — stated plainly, not
  implied uniform.
- `ExpiryPolicy`'s three states and why `Option<Duration>` couldn't represent them; `update()`'s
  explicit refusal to silently revive an expired memory.
- The typed-`Uuid`-core pattern and why it exists: compensation logic can't target the wrong id
  because there's no string round-trip to fail.
- `created_or_accessed_since`'s exact, named semantics — and that a true "what was edited" query
  needs a future `updated_at` column, which does not exist yet.
- Compression's storage-type policy (`Semantic`, not `Episodic`) and its one honestly-documented
  cross-transaction gap between summary creation and marking originals superseded.
- Maintenance's single-controller enforcement, fallible start, and panic-recovery contract.
- The `generic-http` backend's real, tested `search()`/`replace_all()` contract, and
  `open_with_store()` as the supported way to select it.
- The full temporal API: `query_by_time_range`, `created_or_accessed_since`,
  `find_stale_memories`, `find_superseded_chain`, plus `RecallQuery`'s
  `created_after`/`created_before`/`only_stale`.

### "Risks and Honest Limitations" states:
- Compression is extractive/concatenation-based, not LLM-abstractive.
- `as_prompt_context()` delimits content; it does not sanitize against prompt injection.
- `replace_all`'s atomicity guarantee is backend-dependent; only the in-memory backend gets the
  strong guarantee described above.
- The migration runner's repair scope is intentionally narrow — table/index existence plus the one
  named `confidence`-column repair, not a general schema-validation tool.
- Compression's two-write sequence (summary, then supersession) is not crash-atomic; a hard
  process kill between them is a known, undocumented-by-omission-no-longer gap, now stated here
  explicitly, with a transactional redesign left as a backlog item.

### Final validation
- `cargo fmt --check`; `cargo clippy --all-targets --all-features -- -D warnings`.
- `cargo test --all-targets --all-features` **and** the default-feature test run, separately.
- `cargo doc --no-deps --all-features` — zero warnings.
- Fresh clone → fresh build → fresh `open()`, exercising Step 0's migration + resync path with no
  prior state.
- **Final checkpoint — release gate, not automatic:** after the user reviews the diff, release
  notes, and semver choice, the user may explicitly authorize a git tag. No automatic tagging.

---

## Cross-reference: every v4-review finding and where v5 fixes it

| # | Finding | Root cause it was folded into | Fix in v5 |
|---|---|---|---|
| 1 | Step 0 references `VectorStore`/`InMemoryVectorStore` before M3 creates them | Ordering bug — trait was defined after its first use | Step 0.3–0.4 now define the trait and `InMemoryVectorStore` themselves, before the engine (0.5) is changed to depend on them |
| 2 | Working memories get `expires_at == now` via a new, wrong `default_ttl_days()` | Reinventing something that already existed correctly | Deleted the redundant function entirely; Step 3.2/5.4 call the repo's existing, correct `MemoryType::default_ttl()` |
| 3 | Resync only upserts, never removes stale vector ids | **Reconciliation logic never matched the trait** | `VectorStore::replace_all` (0.3), implemented per backend (0.4/M11), is the one primitive every reconciliation path uses |
| 4 | M9 rebuild excludes expired/superseded rows, breaking `include_*` after any rebuild | Same root as #3/#5 | M9's `rebuild_vector_index` (this doc) selects every row with an embedding, not `get_active_memories()`, and reconciles via `replace_all` |
| 5 | M9 rebuild silently replaces a remote backend with `InMemoryVectorStore` | Same root as #3/#4 | Rebuild never constructs a backend — it calls `replace_all` on the *existing* `Arc<dyn VectorStore>`, whatever backend that is |
| 6 | `GenericHttpVectorStore` is defined but never reachable through the public API | Missing constructor | `open_with_store()` (M11), routed through the single `open_with_store_internal` from Step 3.1 |
| 7 | M9 calls `async fn get_embedding` without `.await` | Missed await at a call site | Call site now `.await`s it (M9); confirmed `get_embedding` is already `async` in the real repo |
| 8 | `find_superseded_chain` passes `Uuid` where `get()` wants `&str` | Type mismatch at a call site | `self.get(&next_id.to_string())` (M7, Step 7.2) |
| 9 | M4 test uses `update()` before M5 defines it | **Snippet written before checking what earlier milestones actually have** | The include-superseded/expired test that needs `update()` moved to M5 (Step 5.6+); M4's own version (Step 4.4) uses only test-only SQL |
| 10 | M7 builder methods reference `RecallQuery` fields M4 never added | Same root as #9 | `created_after`/`created_before`/`only_stale` fields and their builder methods and validation are added together, in M7, Step 7.1 — not split |
| 11 | `insert`/`search` set a timeout; `delete`/`contains`/`clear` don't | Duplicated per-call config instead of client-level config | `reqwest::Client::builder().timeout(...)` set once in `GenericHttpVectorStore::new` (M11) |
| 12 | HTTP `search()` doesn't enforce dimension, a result cap, or a deterministic tie-break | Missing input/output validation | Dimension check before the call, `MAX_RESULTS` cap after, `total_cmp` + id tie-break sort + `truncate(k)` (M11) |
| 13 | New modules never explicitly registered in `lib.rs` | Implicit module wiring | Every new module states its `mod`/`pub use` line in the same step that creates it (0.7, M8, M9, M11) |
| 14 | Dependencies (`async-trait`, `tokio-util`, `tracing`, `wiremock`, etc.) never fully enumerated | Scattered dependency additions | One authoritative `Cargo.toml` block, Step 0.1, listing everything used anywhere in this document |
| 15 | `what_changed_since` actually means "created or accessed"; migration "self-heals any schema" oversold | Two separate honesty gaps | Renamed to `created_or_accessed_since` with an exact doc comment (M7); migration scope stated precisely and split into two named repairs (0.7 + 6.2) |
| 16 | `update()` silently revives an expired memory with a 1-second TTL | Missing guard on an edge case | `update()` now returns `Err(InvalidArgument)` for an expired memory unless `new_expiry` is explicitly supplied (M5, Step 5.5) |
| 17 | `recall()` returns pre-increment access stats | Returned a stale snapshot instead of the post-write state | `recall()` refetches the memory after bumping its access stats, before returning it (M3, Step 3.3) |
| 18 | Compression's summary-then-supersede sequence isn't crash-atomic | Overstated/unstated atomicity | Left as two writes (compensation already covers the error-returned case); the true gap is now named explicitly in `ARCHITECTURE.md`/Risks (M9, M12), with a transactional redesign as a stated backlog item, not a hidden gap |
| — | "Fully self-contained" claim was inaccurate — relied on "unchanged from v3" | Document structure | Every type, trait, and non-trivial function used anywhere in this document is written out in full, in the step that introduces it — no external reference to a prior version required to implement this one |

v5's ordering guarantee, unchanged in spirit from v4 but now actually true throughout: **every
code block, at every milestone, only calls functions and references fields that were fully defined
at or before that point in this document — and this document never assumes anything about the
codebase that wasn't independently verified against the real repository first.**



#### SHORTCOMINGS OF THIS BUILDING PLAN ###

V5 is the strongest plan so far, but it still is not fully executable as written. The central architecture is now mostly sound; the remaining problems are primarily milestone ordering, missing definitions, incomplete module wiring, and a few correctness gaps.

I would rate it around 88% ready.

No repository files were modified.

## Compile blockers

### 1. Step 0 changes `MemoryEngine` before replacing `open()`

At [plan line 309](C:\Users\Mayan\.codex\attachments\88c8fdc9-75e8-4a81-9a50-7742be453400\pasted-text.txt:309), Step 0 changes the engine to:

```rust
pub struct MemoryEngine {
    conn: Mutex<Connection>,
    embedder: Mutex<Embedder>,
    vector_store: RwLock<Arc<dyn VectorStore>>,
    maintenance_running: Arc<AtomicBool>,
}
```

But the compatible `open()` implementation is not introduced until M3.

Immediately after changing the struct, the current [engine.rs](C:\Users\Mayan\Desktop\memolite\src\engine.rs:320) still has an `open()` that constructs:

```rust
Self {
    conn,
    embedder: Mutex::new(embedder),
}
```

That no longer matches the struct. Therefore Step 0’s claimed checkpoint cannot pass:

```text
cargo build && cargo test green
```

Move Step 3.1’s `open()` implementation into Step 0.5, or delay changing `MemoryEngine` until M3.

### 2. `InvalidConfidence` is referenced but never defined

At [plan line 993](C:\Users\Mayan\.codex\attachments\88c8fdc9-75e8-4a81-9a50-7742be453400\pasted-text.txt:993):

```rust
pub fn parse_str(s: &str) -> Result<Self, InvalidConfidence>
```

But V5 never defines `InvalidConfidence`.

Add:

```rust
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid confidence value: {0}")]
pub struct InvalidConfidence(pub String);
```

Also explicitly import:

```rust
use serde::{Deserialize, Serialize};
```

When parsing confidence inside `row_to_memory`, convert this error into `rusqlite::Error::FromSqlConversionFailure`, matching the existing memory-type conversion pattern.

### 3. New modules are not actually registered

The cross-reference says every module is registered, but the plan only explicitly registers `migrations` and mentions `streaming`.

It does not clearly add all required module declarations:

```rust
pub mod vector_store;
pub mod recall;
pub mod ranking;
pub mod requests;
pub mod confidence;
pub mod compression;
pub mod streaming;
pub mod maintenance;

#[cfg(feature = "generic-http")]
pub mod generic_http;
```

Nor does it define the corresponding public exports consistently.

Add one exact `lib.rs` block and update it in the milestone where each public type becomes available.

## The plan is still not fully self-contained

### 4. M8 contains no implementation

M8 only describes `StreamIngestor`, cancellation behavior, `SentenceBuffer`, reports and tests. It does not provide the actual types or methods.

Missing concrete definitions include:

```rust
pub struct IngestChunk
pub struct IngestReport
pub struct StreamIngestor
pub struct SentenceBuffer
StreamIngestor::spawn
StreamIngestor::sender
StreamIngestor::shutdown_now
StreamIngestor::finish
SentenceBuffer::feed
SentenceBuffer::finish
```

A developer cannot implement M8 from V5 alone.

### 5. Compression contains unresolved missing functions and types

M9 calls:

```rust
self.get_episodic_memories_older_than(14)?
compression::greedy_cluster(...)
compression::summarize_cluster(...)
```

but V5 does not define these implementations or their supporting types:

```rust
Cluster
CompressionResult
COMPRESSION_ALGORITHM_VERSION
MAX_SUMMARY_CHARS
```

The plan also does not provide the complete `compression.rs` module or its registration.

### 6. `MemoryStats` and `stats()` disappeared

Earlier plans included an observable statistics API. V5 neither implements it nor explicitly removes it from project scope.

Choose one:

- Add a complete `MemoryStats` definition and `stats()`.
- Explicitly state it has been removed from the final project requirements.

## Correctness faults

### 7. `InMemoryVectorStore::search()` does not validate the query

The implementation validates inserted/replacement vectors, but `search()` accepts any query length and any non-finite values.

Because cosine uses `zip`, a wrong-length query is silently truncated rather than rejected.

Add before reading the map:

```rust
if query.len() != self.dim {
    return Err(MemoliteError::VectorStore(format!(
        "query has dimension {}, expected {}",
        query.len(),
        self.dim
    )));
}

if !query.iter().all(|x| x.is_finite()) {
    return Err(MemoliteError::VectorStore(
        "query contains non-finite values".into(),
    ));
}
```

`insert()` should also reject non-finite vectors, not only `replace_all()`.

### 8. `forget()` validates the UUID after deleting SQLite

It currently performs the database deletion and only then does:

```rust
let uuid = Uuid::parse_str(id)?;
```

Validate first:

```rust
let uuid = Uuid::parse_str(id)?;
```

Then mutate SQLite.

This also forces a public API decision: previously, forgetting a nonexistent arbitrary string was effectively idempotent. V5 changes malformed IDs into errors. That change should be documented and tested.

### 9. M4 reintroduces pre-increment result values

M3 correctly refetches memories after incrementing access statistics. M4’s `recall_query()` constructs `RecallItem`s, then increments the database afterward:

```rust
for item in &scored {
    self.update_access_stats(item.memory.id)?;
}

Ok(RecallResult { items: scored })
```

The returned `Memory` values again contain the old `access_count` and `last_accessed`.

Choose one consistent behavior:

- Update/refetch every final result before returning.
- Mutate the in-memory items after the SQL update.
- Explicitly document that recall results represent pre-access state.

The existing direction established in M3 suggests refetching or mutating to post-access state.

### 10. M6 leaves conflicting access-stat helpers

M4’s `recall()` delegates to `recall_query()`. M6 replaces the `recall_query()` call with the promotion-aware helper, so the old `update_access_stats()` helper is no longer needed.

The plan incorrectly says the M3 plain recall path still uses it, even though that body was replaced in M4.

Remove the obsolete helper in M6 or Clippy may report it as dead code.

### 11. Compression silently skips missing embeddings

M9 does:

```rust
if let Some(v) = self.get_embedding(...).await? {
    with_vectors.push(...);
}
```

A missing embedding makes a candidate silently disappear from compression. Earlier plans correctly required this to fail loudly.

Use:

```rust
let vector = self
    .get_embedding(&id.to_string())
    .await?
    .ok_or_else(|| MemoliteError::VectorStore(
        format!("memory {id} has no persisted embedding")
    ))?;
```

Also validate dimension and finiteness before clustering. Validation inside `replace_all()` does not protect the clustering path because compression does not call `replace_all()` before clustering.

### 12. `resync_vector_index()` accepts missing embeddings silently

The inner join:

```sql
FROM memories m
JOIN embeddings e ON e.memory_id = m.id
```

omits memory rows without embeddings. `replace_all()` then makes the vector index “exactly” match only the join result, not all memories.

Either:

- Treat memories without embeddings as database corruption and fail `open()`.
- Regenerate their embeddings.
- Explicitly document that embedding-less memories are allowed but never recallable.

For the project’s current design, failing loudly is the safest policy. Use a `LEFT JOIN` plus a missing-embedding check or compare row counts.

### 13. Remote opening destructively replaces the entire remote index

`open_with_store_internal()` always calls:

```rust
resync_vector_index(...);
```

which calls remote `replace_all()`.

Opening a local database against a shared remote endpoint will delete every vector not represented in that local SQLite database.

Add an explicit policy:

```rust
pub enum BackfillPolicy {
    ExistingOnly,
    UpsertLocal,
    ReplaceAll,
}
```

Or clearly require that every remote vector-store endpoint/collection is exclusively dedicated to one Memolite database.

Silently issuing destructive `replace_all` during `open_with_store()` is too dangerous.

### 14. `replace_all()` on the HTTP backend lacks finite-value validation

It validates dimensions, but not NaN/infinity.

Add:

```rust
if !e.vector.iter().all(|x| x.is_finite()) {
    return Err(MemoliteError::VectorStore(format!(
        "entry for {} contains a non-finite value",
        e.id
    )));
}
```

The same applies to HTTP `insert()`.

## Migration concerns

### 15. Confidence is not recorded as migration version 2

The confidence repair runs separately but the schema migration table still records only version 1.

That makes the version table an incomplete representation of schema history.

Prefer:

```text
1 = baseline memories/embeddings
2 = confidence column
```

Run migration 2 transactionally and record it with `INSERT OR IGNORE`.

A repair check can still run defensively, but migration history should describe the real schema.

### 16. `row_to_memory` transition needs an exact implementation

When confidence is added, V5 says to add the field and extend `MEMORY_COLUMNS`, but does not provide the updated decoder.

The exact addition should include:

```rust
let confidence_str: String = row.get(10)?;
let confidence = ConfidenceLevel::parse_str(&confidence_str)
    .map_err(|e| to_sql_conversion_err(10, e))?;
```

and:

```rust
confidence,
```

inside the returned `Memory`.

Every explicit `SELECT` used after M6 must use the updated 11-column constant.

## Benchmark/release gap

### 17. The plan still has no benchmark implementation

You previously stated the finished project should pass:

```text
cargo test
cargo bench
cargo clippy
cargo fmt
```

V5 contains no benchmark target, Criterion dependency, benchmark dataset, or `cargo bench` release check.

Add:

```toml
[dev-dependencies]
criterion = "0.5"

[[bench]]
name = "memolite_bench"
harness = false
```

Suggested benchmarks:

- In-memory vector search at 1k, 10k and 100k vectors.
- SQLite `get()`.
- Store without model-load time.
- Recall at 1k and 10k memories.
- Index resynchronization.
- Compression candidate clustering.

Then include:

```powershell
cargo bench
```

in final validation.

## Verdict

V5 successfully fixes most V4 root causes:

- Correct Working TTL.
- `replace_all()` provides a real reconciliation primitive.
- Rebuild preserves the configured backend.
- Expired and superseded rows remain indexable.
- The remote backend is publicly reachable.
- Temporal naming is honest.
- Dependencies are centralized.
- Expiry handling is materially better.

However, it still has three direct execution blockers:

1. Step 0 changes `MemoryEngine` before providing its compatible `open()`.
2. `InvalidConfidence` is undefined.
3. M8/M9 are not actually self-contained implementations.

It also needs a safer remote-backfill policy and a real benchmark milestone.

My rating:

- Architecture: 9/10
- Persistence design: 8.5/10
- Compile readiness by checkpoint: 8/10
- Self-containedness: 7/10
- Fully executable as written: not yet

This is now close enough that one disciplined correction pass should be sufficient; the remaining issues are concrete rather than foundational.