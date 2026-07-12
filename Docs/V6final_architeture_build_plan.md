# Memolite — Final Master Build Plan (v6, all v5-review findings closed)

> **What changed from v5:** v5 was reviewed and rated ~88% executable, with 17 findings. Every
> finding is closed in v6. As in the v4→v5 pass, several findings shared one root cause, so v6
> fixes each root cause once rather than patching symptoms:
>
> - **Finding #1** (Step 0 rewrites `MemoryEngine`'s fields but the compatible `open()` isn't
>   written until M3, so `cargo build` fails at the Step-0 checkpoint) is fixed by **merging Step
>   0.5 and old Step 3.1 into one step**: the struct and its only constructor are now introduced
>   together. There is no longer a window where the struct exists without a constructor that
>   matches it.
> - **Findings #9 and #10** (M4's `recall_query()` reintroduces the exact pre-increment bug M3 had
>   fixed; the plain `update_access_stats` helper is left as dead code once M4 replaces its only
>   call site) are the same root cause — **the access-stat-and-refetch logic was defined twice,
>   once in M3 and once in M4, and drifted**. v6 defines it exactly once, at the point
>   `recall_query()` is introduced (M4), and M3's `recall()` is written from the start as a thin
>   wrapper around it — so there is only ever one bump/refetch code path, and M6 modifies that one
>   path in place instead of overriding a second one.
> - **Findings #11 and #12** (compression silently drops candidates with no embedding; index
>   resync silently drops memories with no embedding) are the same root cause — **treating a
>   missing embedding as "skip it" instead of "the invariant that every memory row has exactly one
>   embedding row has been violated."** v6 fixes this once: `store_with_options_id` is the only
>   writer of memory+embedding rows and writes both in the same transaction, so a memory without an
>   embedding is always corruption, never a normal state. Every reader that joins the two tables —
>   resync, rebuild, compression — now fails loudly on a mismatch instead of silently filtering.
> - **Findings #3, #7, #14** (query validation missing in the in-memory backend's `search`; finite
>   checks missing on `insert`/HTTP `replace_all`) are the same root cause — **validation was
>   written once, in `replace_all`, and assumed to cover `insert`/`search` by proximity, which it
>   doesn't, because they're separate methods.** v6 adds one private helper,
>   `validate_vector(v, dim)`, per backend, and every method that accepts a vector — `insert`,
>   `search`'s query, `replace_all`'s entries — calls it. No method is exempt because "the other
>   one already checks."
> - This document is written against the actual current state of
>   `github.com/mayanpathak/memolite` (`main`, 4 commits, `src/`, `tests/`, `examples/`,
>   `Changelogs/`, Rust 100%). Per v5's verified baseline: `MemoryEngine` currently holds a plain
>   `rusqlite::Connection` (not yet a `Mutex`), `MemoryType::default_ttl()` already exists and is
>   correct (Working = 4 hours), `get()` takes `&str`, and `get_embedding()` is already declared
>   `async`. v6 builds forward from that baseline, unchanged from v5's premise.
>
> Rule zero, unchanged: everything already working in the repository stays as-is, with the Step-0
> repairs applied first. Every code block in this document only calls things defined at or before
> that point in this document, and every module used anywhere is registered in `lib.rs` in the
> step that introduces it. A cross-reference table mapping every one of the 17 v5 findings to its
> fix is at the very end.

---

## Step 0 — Foundations, correctly ordered, struct and constructor merged

### 0.1 — Cargo.toml, one authoritative dependency step

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
criterion = "0.5"                      # fixes finding #17

[features]
generic-http = ["dep:reqwest", "dep:urlencoding"]

[[bench]]
name = "memolite_bench"
harness = false
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

/// The database has a memory row with no matching embedding row, or vice
/// versa. store_with_options_id() always writes both in one transaction,
/// so this can only mean the on-disk file was corrupted or hand-edited
/// outside the library. Fixes v5 findings #11/#12: this is never silently
/// filtered, it always surfaces.
#[error("data invariant violated: {0}")]
Corruption(String),

#[error("invalid confidence value: {0}")]
InvalidConfidence(String),
```

`InvalidConfidence` (v5 finding #2) is folded directly into the crate's existing error enum rather
than introduced as a second, separate error type — this avoids a `From<InvalidConfidence>`
conversion needing to be written and kept in sync at every call site. `ConfidenceLevel::parse_str`
(Step 6.1) returns `Result<Self>` (the crate's own `Result`), not a bespoke error struct.

### 0.3 — The `VectorStore` trait, with `replace_all` and a shared validation contract

**File:** `src/vector_store/mod.rs` (new module).

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use serde_json::Value;
use uuid::Uuid;
use crate::error::{MemoliteError, Result};

pub mod in_memory;
pub use in_memory::InMemoryVectorStore;

#[cfg(feature = "generic-http")]
pub mod generic_http;
#[cfg(feature = "generic-http")]
pub use generic_http::GenericHttpVectorStore;

#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: Uuid,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct VectorEntry {
    pub id: Uuid,
    pub vector: Vec<f32>,
    pub metadata: HashMap<String, Value>,
}

/// Shared by every backend's `insert`/`search`/`replace_all` — fixes v5
/// findings #3/#7/#14. There is exactly one validation function; no method
/// on any backend is allowed to skip calling it just because a sibling
/// method already validates something similar.
pub fn validate_vector(label: &str, v: &[f32], dim: usize) -> Result<()> {
    if v.len() != dim {
        return Err(MemoliteError::VectorStore(format!(
            "{label} has dimension {} but store expects {dim}", v.len()
        )));
    }
    if !v.iter().all(|x| x.is_finite()) {
        return Err(MemoliteError::VectorStore(format!("{label} contains a non-finite value")));
    }
    Ok(())
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    /// MUST be an idempotent upsert. MUST call `validate_vector` on `vector`
    /// before storing anything.
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    /// MUST call `validate_vector` on `query` before searching.
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn contains(&self, id: Uuid) -> Result<bool>;
    async fn clear(&self) -> Result<()>;
    /// Replaces the *entire* contents of this store with exactly `entries`.
    /// Any id currently present but absent from `entries` MUST be gone
    /// afterward; every id in `entries` MUST be present and correct
    /// afterward. MUST call `validate_vector` on every entry before storing
    /// anything (all-or-nothing: a bad entry rejects the whole call).
    ///
    /// This is the single reconciliation primitive used everywhere the
    /// engine needs to make the vector index agree with SQLite: restart
    /// backfill, forget-time cleanup after a partial failure, and M9's
    /// index rebuild all call this one method. The engine never constructs
    /// a new backend instance itself, so a rebuild can never silently swap
    /// a remote backend out for an in-memory one.
    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

### 0.4 — `InMemoryVectorStore`, fully inlined, `search`/`insert` now validate (fixes finding #7)

**File:** `src/vector_store/in_memory.rs`.

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use serde_json::Value;
use uuid::Uuid;
use crate::error::{MemoliteError, Result};
use super::{validate_vector, VectorEntry, VectorHit, VectorStore};

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

    fn lock_read(&self) -> Result<std::sync::RwLockReadGuard<'_, HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>> {
        self.data.read().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))
    }
    fn lock_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>> {
        self.data.write().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))
    }
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        validate_vector(&format!("vector for {id}"), vector, self.dim)?;
        self.lock_write()?.insert(id, (vector.to_vec(), metadata));
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        validate_vector("query", query, self.dim)?;
        let guard = self.lock_read()?;
        let mut hits: Vec<VectorHit> = guard.iter()
            .map(|(id, (v, _))| VectorHit { id: *id, score: Self::cosine(query, v) })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(k);
        Ok(hits)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        self.lock_write()?.remove(&id);
        Ok(())
    }

    async fn contains(&self, id: Uuid) -> Result<bool> {
        Ok(self.lock_read()?.contains_key(&id))
    }

    async fn clear(&self) -> Result<()> {
        self.lock_write()?.clear();
        Ok(())
    }

    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()> {
        let mut replacement = HashMap::with_capacity(entries.len());
        for e in entries {
            validate_vector(&format!("entry for {}", e.id), &e.vector, self.dim)?;
            replacement.insert(e.id, (e.vector, e.metadata));
        }
        *self.lock_write()? = replacement;
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
        assert_eq!(store.search(&[1.0, 0.0], 1).await.unwrap()[0].id, a);
    }

    #[tokio::test]
    async fn insert_is_an_upsert() {
        let store = InMemoryVectorStore::new(2);
        let id = Uuid::new_v4();
        store.insert(id, &[1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert(id, &[0.0, 1.0], HashMap::new()).await.unwrap();
        assert_eq!(store.data.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn wrong_dimension_insert_is_rejected() {
        let store = InMemoryVectorStore::new(3);
        assert!(store.insert(Uuid::new_v4(), &[1.0, 0.0], HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn non_finite_insert_is_rejected() {
        let store = InMemoryVectorStore::new(2);
        assert!(store.insert(Uuid::new_v4(), &[f32::NAN, 0.0], HashMap::new()).await.is_err());
    }

    // fixes finding #7: search() now validates the query, not just replace_all's entries
    #[tokio::test]
    async fn wrong_dimension_query_is_rejected_not_silently_truncated() {
        let store = InMemoryVectorStore::new(3);
        store.insert(Uuid::new_v4(), &[1.0, 0.0, 0.0], HashMap::new()).await.unwrap();
        assert!(store.search(&[1.0, 0.0], 1).await.is_err());
    }

    #[tokio::test]
    async fn non_finite_query_is_rejected() {
        let store = InMemoryVectorStore::new(2);
        assert!(store.search(&[f32::INFINITY, 0.0], 1).await.is_err());
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
        let bad = VectorEntry { id: Uuid::new_v4(), vector: vec![1.0], metadata: HashMap::new() };
        assert!(store.replace_all(vec![bad]).await.is_err());
        assert!(store.contains(original).await.unwrap());
    }
}
```

### 0.5 — `MemoryEngine`'s final shape **and** its only `open()`/`open_with_store()` constructor,
introduced together — the fix for finding #1

v5 changed the struct in Step 0.5 but didn't write a matching constructor until M3, so the Step-0
checkpoint (`cargo build`) failed. v6 closes that gap by never letting the struct exist without a
constructor that builds it. **`open()` and the internal constructor move here, permanently — M3
no longer redefines them, it only adds `store`/`recall`/`forget` on top of what this step
creates.**

```rust
pub struct MemoryEngine {
    conn: std::sync::Mutex<rusqlite::Connection>,
    embedder: std::sync::Mutex<crate::embedder::Embedder>,
    vector_store: std::sync::RwLock<std::sync::Arc<dyn crate::vector_store::VectorStore>>,
    maintenance_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Controls what `open_with_store()` does to a caller-supplied backend's
/// *existing* remote contents at open time. Fixes finding #13: v5's
/// `open_with_store_internal` unconditionally called `replace_all` on
/// whatever backend it was given, which would silently wipe out any vector
/// belonging to another database sharing the same remote collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillPolicy {
    /// Do not touch the remote store's contents at all. Local SQLite rows
    /// with no matching remote vector will simply fail to recall until a
    /// later `rebuild_vector_index()` call is made explicitly. Safe default
    /// for a backend shared with other data.
    ExistingOnly,
    /// Upsert every local row into the store via `insert`, but never
    /// delete anything the store already has that SQLite doesn't know
    /// about. Safe for a shared backend; brings this database's own rows
    /// up to date without touching anyone else's.
    UpsertLocal,
    /// Call `replace_all` so the store's contents become *exactly* this
    /// database's rows — anything else present is deleted. Only correct
    /// when this backend/collection is dedicated exclusively to this one
    /// Memolite database. This is the only mode that behaves like v5's
    /// unconditional resync did.
    ReplaceAll,
}

impl MemoryEngine {
    /// Opens (or creates) the local SQLite file, backed by the default
    /// in-memory vector store. Equivalent to
    /// `open_with_store_internal(path, None, BackfillPolicy::ReplaceAll)` —
    /// `ReplaceAll` is correct and safe here specifically because the
    /// in-memory store is private to this engine instance; nothing else
    /// could ever be sharing it.
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open_with_store_internal(path, None, BackfillPolicy::ReplaceAll).await
    }

    async fn open_with_store_internal(
        path: impl AsRef<std::path::Path>,
        store_override: Option<std::sync::Arc<dyn crate::vector_store::VectorStore>>,
        backfill: BackfillPolicy,
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
        reconcile_vector_index(&conn, &vector_store, backfill).await?;

        Ok(Self {
            conn,
            embedder: std::sync::Mutex::new(embedder),
            vector_store: std::sync::RwLock::new(vector_store),
            maintenance_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
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
```

### 0.6 — Column-order constant and candidate-pool sizing

```rust
// 10 columns through M5; M6 (Step 6.1) extends this to 11 when `confidence` is added —
// there is only ever one live definition, edited in place, never a second copy.
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

### 0.7 — Migration runner, confidence repair now recorded as its own schema version (fixes #15)

**File:** `src/migrations.rs`, registered in `src/lib.rs` as `mod migrations;` in this same step.

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
    fn has_migration(tx: &rusqlite::Transaction, version: i64) -> crate::error::Result<bool> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
            rusqlite::params![version], |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    // --- migration 1: baseline memories/embeddings ---
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
    if !has_migration(&tx, 1)? {
        tx.execute("INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
            rusqlite::params![chrono::Utc::now().timestamp()])?;
    }
    tx.commit()?;

    // --- migration 2: confidence column (introduced at Step 6.2, called from here on every open) ---
    crate::confidence::repair_confidence_column(conn)?;

    Ok(())
}
```

**Honesty note (unchanged from v5, still accurate):** this runner repairs exactly two named
things — missing expected tables/indexes (migration 1) and the missing `confidence` column
(migration 2, Step 6.2). It does not verify column types, constraints, or repair an arbitrarily
hand-edited schema, and never claims to.

### 0.8 — Vector-index reconciliation — now policy-aware and corruption-aware (fixes #12, #13)

Replaces v5's `resync_vector_index` with `reconcile_vector_index`, which takes a `BackfillPolicy`
(0.5) and treats a memory row with no embedding row as `Corruption`, not as a row to silently
skip — closing finding #12 at its root, the same way `replace_all` closed findings #3/#4/#5 in v5.

```rust
/// Reads every memory row and, via a LEFT JOIN, every embedding row that
/// should exist for it. A NULL on the embedding side means a memory row
/// exists with no embedding — since `store_with_options_id` (M5, Step 5.4)
/// always writes both in the same transaction, this is only reachable via
/// external corruption of the SQLite file, and is reported as such rather
/// than silently dropped from the index (fixes finding #12).
///
/// Read phase is fully synchronous and collects into a `Vec` before any
/// `.await`, so no SQLite borrow is ever held across an await point.
async fn reconcile_vector_index(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    store: &std::sync::Arc<dyn crate::vector_store::VectorStore>,
    policy: crate::BackfillPolicy,
) -> Result<()> {
    use crate::vector_store::VectorEntry;

    let entries: Vec<VectorEntry> = {
        let conn = conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector, e.dimension, m.metadata
             FROM memories m LEFT JOIN embeddings e ON e.memory_id = m.id"
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
            let vector: Vec<f32> = bincode::deserialize(&bytes).map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
            if vector.len() != stored_dim as usize {
                return Err(MemoliteError::Corruption(format!(
                    "stored vector for {id} has dimension {} but its row says {}", vector.len(), stored_dim
                )));
            }
            let metadata: HashMap<String, Value> = serde_json::from_str(&metadata_json)?;
            out.push(VectorEntry { id, vector, metadata });
        }
        out
    }; // MutexGuard dropped here — before any await below

    match policy {
        crate::BackfillPolicy::ExistingOnly => Ok(()),
        crate::BackfillPolicy::UpsertLocal => {
            for e in entries {
                store.insert(e.id, &e.vector, e.metadata).await?;
            }
            Ok(())
        }
        crate::BackfillPolicy::ReplaceAll => store.replace_all(entries).await,
    }
}
```

`open()` (0.5) always passes `BackfillPolicy::ReplaceAll`, which is safe there because it only
ever opens the default private `InMemoryVectorStore`. `open_with_store()` (M11) requires the
caller to choose explicitly among all three (see M11).

**Checkpoint 0:** `cargo build && cargo test` green. `VectorStore`, `InMemoryVectorStore`,
`migrations::run_migrations`, `reconcile_vector_index`, `BackfillPolicy`, and the **final** shape
of `MemoryEngine` — including its only `open()` — all exist and compile together, closing finding
#1: there is no point in this document where the struct and its constructor disagree.

---

## `lib.rs` — the one authoritative module-registration block (fixes finding #3 of the v5 review's
"module wiring" concern)

Rather than registering modules piecemeal and trusting the cross-reference table, v6 states the
**entire final `lib.rs` module list up front**, then marks, in each milestone below, exactly when
each line's contents become non-empty. No milestone below is allowed to add a type that isn't
reachable through one of these lines.

```rust
// src/lib.rs
pub mod error;                          // exists in repo already
pub mod embedder;                       // exists in repo already
mod migrations;                         // Step 0.7
pub mod vector_store;                   // Step 0.3 (+ generic_http submodule, feature-gated, M11)
pub mod engine;                         // exists in repo; grows through every milestone below
pub mod recall;                         // Step 0.6 (MAX_CANDIDATES) → M4 (RecallQuery/Item/Result)
pub mod ranking;                        // M4, Step 4.1
pub mod requests;                       // M5, Step 5.1–5.3 (StoreRequest, MemoryUpdate, ExpiryPolicy)
pub mod confidence;                     // M6, Step 6.1–6.2 (ConfidenceLevel, repair_confidence_column)
pub mod streaming;                      // M8 (StreamIngestor, SentenceBuffer, IngestReport)
pub mod compression;                    // M9 (Cluster, CompressionResult, greedy_cluster, summarize_cluster)
pub mod maintenance;                    // M10 (MaintenanceConfig, MaintenanceHandle)
pub mod stats;                          // M9.5 (MemoryStats) — fixes finding #6

pub use engine::{MemoryEngine, BackfillPolicy};
pub use error::{MemoliteError, Result};
pub use recall::{RecallQuery, RecallItem, RecallResult};
pub use requests::{StoreRequest, MemoryUpdate, ExpiryPolicy};
pub use confidence::ConfidenceLevel;
pub use vector_store::{VectorStore, VectorEntry, VectorHit, InMemoryVectorStore};
pub use streaming::{StreamIngestor, IngestReport, IngestChunk};
pub use maintenance::{MaintenanceConfig, MaintenanceHandle};
pub use stats::MemoryStats;

#[cfg(feature = "generic-http")]
pub use vector_store::GenericHttpVectorStore;
```

Every struct/fn introduced in every milestone below lives in the module named on its line here.

---

## Corrected build order

| Order | Milestone | What it adds |
|---|---|---|
| 0 | Step 0 | `VectorStore` trait + `replace_all` + shared `validate_vector`, `InMemoryVectorStore`, migrations, `BackfillPolicy`, `reconcile_vector_index`, **and the engine's final struct + its only `open()`, introduced together** |
| 1 | M3 | `store()`/`recall()`/`forget()` layered on top of Step 0's engine — no struct/constructor changes here anymore |
| 2 | M4 | Ranking (confidence stubbed at `1.0`), `RecallQuery`/`RecallItem`/`RecallResult`, **the single bump-and-refetch helper both `recall()` and `recall_query()` use from here on** |
| 3 | M5 | `StoreRequest`/`MemoryUpdate`/`ExpiryPolicy`, `update()` |
| 4 | M6 | `ConfidenceLevel`, confidence-column repair (migration 2), ranking's stub replaced, the bump helper upgraded in place (not duplicated) |
| 5 | M7 | Temporal querying |
| 6 | M6.5 | `Send + Sync` + no-lock-across-await audit |
| 7 | M8 | Streaming ingestion — fully inlined |
| 8 | M9 | Compression + index rebuild via `replace_all` — fully inlined, missing embeddings fail loudly |
| 9 | M9.5 | `MemoryStats` / `stats()` — fixes finding #6 |
| 10 | M10 | Maintenance controller |
| 11 | M11 | `generic-http` backend, `open_with_store(path, store, BackfillPolicy)` |
| 12 | M12 | Docs, benchmarks, packaging, release gate |

---

# M3 — `store()` / `recall()` / `forget()`, layered on Step 0's engine

Nothing in this milestone touches the struct or `open()` again — both are final as of Step 0.5.

### Step 3.1 — `store()`

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
    let expires_at = now + memory_type.default_ttl();
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
    } // conn guard dropped before the await below — both rows land in one transaction,
      // which is exactly what makes the "missing embedding = corruption" rule in Step 0.8 sound.

    let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
    if let Err(e) = store.insert(id, &vector, HashMap::new()).await {
        let compensation = { let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
            conn.and_then(|c| c.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id.to_string()]).map_err(Into::into)).err() };
        if let Some(compensation_err) = compensation {
            return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: compensation_err.to_string() });
        }
        return Err(e);
    }
    Ok(id)
}
```

### Step 3.2 — `forget()` — validates the id *before* mutating SQLite (fixes finding #8)

```rust
pub async fn forget(&self, id: &str) -> Result<()> {
    // v5 parsed the UUID after already deleting from SQLite, so a malformed
    // id string would fail only after a (no-op) delete had already run.
    // Validate first, so a malformed id is rejected with zero side effects.
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;

    {
        let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])?;
    }
    let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
    if let Err(e) = store.delete(uuid).await {
        if let Err(resync_err) = reconcile_vector_index(&self.conn, &store, BackfillPolicy::ReplaceAll).await {
            return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: resync_err.to_string() });
        }
        return Err(e);
    }
    Ok(())
}
```

**Documented behavior change from pre-v5 code, explicit per finding #8's own note:** `forget()` on
a syntactically invalid id is now always `Err(InvalidArgument)` rather than a silent no-op.
`forget()` on a syntactically *valid* id that simply doesn't exist remains a silent no-op (SQLite
`DELETE` affects 0 rows, no error) — that half of the old idempotency is unchanged and is tested
explicitly (Step 3.4).

### Step 3.3 — `recall()` is a thin wrapper from the start (fixes findings #9/#10 at the root)

v5 wrote a real `recall()` in M3, then M4 replaced its innards with `recall_query()` and
reintroduced the exact pre-increment bug M3 had just fixed, while M3's now-orphaned
`update_access_stats` helper became dead code by M6. v6 removes the duplication instead of
creating and later cleaning it up: **M3's `recall()` body is nothing but a call into M4's
`recall_query()`**, so there is only ever one bump-and-refetch implementation to get right, and
M6 edits that one implementation in place.

```rust
pub async fn recall(&self, query_text: &str) -> Result<Vec<Memory>> {
    Ok(self.recall_query(crate::RecallQuery::new(query_text)).await?.items.into_iter().map(|i| i.memory).collect())
}
```

`recall_query()` itself — including the single bump/refetch helper — is defined in M4, Step 4.3.
No standalone `update_access_stats` is ever written in M3; there is nothing to later find dead.

### Steps 3.4–3.7 — Tests + checkpoint
- `store()` + `recall()` round-trip: 3 unrelated facts + 1 relevant, the relevant one is found.
- `recall()` on an empty engine → `Ok(vec![])`.
- **Restart test** (exercises Step 0.7 + 0.8 + Step 0.5's `open()`): store 3, drop the engine,
  `open()` the same path again, `recall()` immediately — all 3 findable.
- `forget()` on a malformed id string → `Err(InvalidArgument)`, and a subsequent `open()` shows no
  row was touched (fixes finding #8, both directions tested).
- `forget()` on a well-formed but nonexistent id → `Ok(())`, no error (idempotency preserved).
- `forget()` removes the memory from SQLite *and* the vector store.
- Corrupt-row restart test: garbage blob in one `embeddings` row → `open()` returns `Err`.
- **New — corruption test (fixes finding #12):** insert a memory row via raw test-only SQL with no
  matching embeddings row, call `open()` (or any `reconcile_vector_index` path) → `Err(Corruption)`,
  not a silent skip.
- `cargo clippy` / `cargo fmt` clean.
- **Checkpoint:** `cargo test` green; nothing above referenced a field or method not defined at or
  before this point in the document.

---

# M4 — Ranking + `recall_query()` — the one bump-and-refetch implementation

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

### Step 4.2 — `RecallQuery`/`RecallItem`/`RecallResult`

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
M7 adds `created_after`/`created_before`/`only_stale` to this same struct, in the same step that
adds their builder methods.

### Step 4.3 — `recall_query()` — bumps access stats *and reflects the bump in what it returns*,
in a single pass, fixing finding #9 for good (this is the only version of this logic that will
ever exist — M6 edits this exact function body, it does not add a second one)

```rust
pub async fn recall_query(&self, query: RecallQuery) -> Result<RecallResult> {
    if query.limit == 0 { return Err(MemoliteError::InvalidArgument("limit must be > 0".into())); }
    if !query.min_importance.is_finite() { return Err(MemoliteError::InvalidArgument("min_importance must be finite".into())); }
    if query.query_text.trim().is_empty() { return Err(MemoliteError::InvalidArgument("query_text must not be empty".into())); }

    let query_vec = {
        let mut embedder = self.embedder.lock().map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(&query.query_text)?
    };
    let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
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

    // Bump every returned item's access stats, then REFETCH so the Memory
    // this function returns reflects the very bump it just made — fixes
    // finding #9. This refetch-after-bump pattern is written exactly once,
    // here; M6 Step 6.5 swaps the SQL this calls, not this loop's shape.
    for item in &mut scored {
        self.bump_access_stats(item.memory.id)?;
        if let Some(refetched) = self.get(&item.memory.id.to_string()).await? {
            item.memory = refetched;
        }
    }
    Ok(RecallResult { items: scored })
}

/// The single access-stats-bump implementation used by recall_query() from
/// M4 onward. M6 Step 6.5 edits this function's SQL in place to also handle
/// confidence promotion — it does not introduce a second function, so there
/// is nothing left over for clippy to flag as dead code (fixes finding #10).
fn bump_access_stats(&self, id: Uuid) -> Result<()> {
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    conn.execute(
        "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp(), id.to_string()],
    )?;
    Ok(())
}
```

### Steps 4.4–4.8 — Tests + checkpoint
- `.memory_types(...)` filter test; `.metadata_equals(...)` filter test.
- `include_superseded`/`include_expired`: written using only test-only raw SQL to backdate
  `superseded_by`/`expires_at` (not `update()`, which doesn't exist until M5) — assert the default
  (`false`) hides them and `true` reveals them.
- **The exact assertion finding #9 named:** call `recall_query()` once, assert the returned
  `RecallItem.memory.access_count` is already incremented and `last_accessed` already bumped —
  provable now because the refetch happens before the function returns.
- `limit(0)` → `Err`; NaN `min_importance` → `Err`; `recall()`/`recall_query()` agree.
- `cargo clippy` clean with **zero dead-code warnings** for anything in `engine.rs` (closes finding
  #10 concretely — there is no orphaned `update_access_stats` to warn about).
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
(`confidence` and `.with_confidence()` are added to this struct in M6, Step 6.1.)

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
(`new_confidence: Option<ConfidenceLevel>` added in M6, Step 6.1.) `id` is never a field here.

### Step 5.4 — `store_with_options()` — the **only** writer of a memory+embedding pair, always in
one transaction (this is what makes Step 0.8's "missing embedding = corruption" rule sound)

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
        ExpiryPolicy::TypeDefault => Some(now + request.memory_type.default_ttl()),
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
        // memories and embeddings are always inserted in the same transaction —
        // this is the invariant Step 0.8's reconcile_vector_index relies on.
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

### Step 5.5 — `update()` — rejects reviving an expired memory unless `new_expiry` is given

```rust
pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<String> {
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    let old = self.get(id).await?.ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;

    let now = Utc::now();
    let is_expired = old.expires_at.map(|e| e <= now).unwrap_or(false);
    if is_expired && update.new_expiry.is_none() {
        return Err(MemoliteError::InvalidArgument("memory is expired; supply new_expiry explicitly to revive it".into()));
    }

    let mut request = StoreRequest::new(
        &update.new_content.unwrap_or_else(|| old.content.clone()),
        update.new_memory_type.unwrap_or(old.memory_type),
        update.new_importance.unwrap_or(old.importance),
    );
    request.expiry = update.new_expiry.unwrap_or(match old.expires_at {
        None => ExpiryPolicy::Never,
        Some(expiry) => ExpiryPolicy::Custom(expiry.signed_duration_since(now)), // unreachable while expired+no new_expiry, per the guard above
    });
    request.metadata = update.new_metadata.unwrap_or_else(|| old.metadata.clone());

    let new_uuid = self.store_with_options_id(request).await?;
    if let Err(e) = self.mark_superseded(&uuid, &new_uuid.to_string()) {
        let del_err = { let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
            conn.and_then(|c| c.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![new_uuid.to_string()]).map_err(Into::into)).err() };
        let store = { let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?; std::sync::Arc::clone(&*guard) };
        let vec_err = store.delete(new_uuid).await.err();
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
- Basic `store_with_options` round-trip; `update()` with `new_metadata: None` preserves metadata.
- `update()` on `ExpiryPolicy::Never` memory: replacement also has `expires_at: None`.
- `update()` on nearly-expired-but-not-yet: remaining TTL carried forward.
- Expired-revival guard: no `new_expiry` → `Err`; `Some(Never)` → succeeds, non-expiring.
- `include_superseded`/`include_expired` integration test using real `update()` this time.
- Compensation test: force `mark_superseded` to fail — replacement gone from both stores, original
  untouched.
- `Custom(<= 0)` → `Err`; update of nonexistent id → `Err(NotFound)`; superseded chain resolves.
- **Checkpoint:** `cargo test` green, zero regressions M3–M4.

---

# M6 — Confidence scoring

### Step 6.1 — `ConfidenceLevel` (fixes finding #2: fully defined, uses the crate's own `Result`,
no separate error type)

```rust
// src/confidence.rs
use serde::{Deserialize, Serialize};
use crate::error::{MemoliteError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceLevel { Explicit, Inferred, Reinforced }

impl ConfidenceLevel {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Explicit => "explicit", Self::Inferred => "inferred", Self::Reinforced => "reinforced" }
    }
    pub fn parse_str(s: &str) -> Result<Self> {
        match s {
            "explicit" => Ok(Self::Explicit),
            "inferred" => Ok(Self::Inferred),
            "reinforced" => Ok(Self::Reinforced),
            other => Err(MemoliteError::InvalidConfidence(other.to_string())),
        }
    }
    pub fn weight(&self) -> f32 { match self { Self::Explicit | Self::Reinforced => 1.0, Self::Inferred => 0.7 } }
    pub fn maybe_promote(self, access_count: u32) -> Self {
        if self == Self::Inferred && access_count >= 5 { Self::Reinforced } else { self }
    }
}
```
Add to `StoreRequest` (Step 5.2): `pub confidence: ConfidenceLevel`, defaulted to `Explicit` in
`::new`, plus `.with_confidence(c)`. Add to `MemoryUpdate` (Step 5.3):
`pub new_confidence: Option<ConfidenceLevel>`, defaulting to `Inferred` when unset inside
`update()`'s body. Add `confidence: ConfidenceLevel` to `Memory`. Extend `MEMORY_COLUMNS`
(Step 0.6) to 11 columns, appending `, confidence`.

### Step 6.2 — Confidence-column repair, recorded as migration version 2 (fixes finding #15)

```rust
// src/confidence.rs (continued)
pub fn repair_confidence_column(conn: &mut rusqlite::Connection) -> Result<()> {
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
    let already_recorded: i64 = tx.query_row(
        "SELECT COUNT(*) FROM schema_migrations WHERE version = 2", [], |r| r.get(0),
    )?;
    if already_recorded == 0 {
        tx.execute("INSERT INTO schema_migrations (version, applied_at) VALUES (2, ?1)",
            rusqlite::params![chrono::Utc::now().timestamp()])?;
    }
    tx.commit()?;
    Ok(())
}
```
Called from `run_migrations()` (0.7) on every open, transactionally, and now correctly reflected
in `schema_migrations` as version 2 — the migration table is an accurate history, not just an
incomplete v1 marker with a silent side-repair.

### Step 6.3 — `row_to_memory`, the exact updated decoder (fixes finding #16)

```rust
fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let type_str: String = row.get(2)?;
    let memory_type = MemoryType::parse_str(&type_str).map_err(|e| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e)))?;
    let created_at = chrono::DateTime::from_timestamp(row.get::<_, i64>(5)?, 0).unwrap_or_default();
    let last_accessed = chrono::DateTime::from_timestamp(row.get::<_, i64>(6)?, 0).unwrap_or_default();
    let expires_at = row.get::<_, Option<i64>>(7)?.and_then(|t| chrono::DateTime::from_timestamp(t, 0));
    let superseded_by = row.get::<_, Option<String>>(8)?.and_then(|s| Uuid::parse_str(&s).ok());
    let metadata_json: String = row.get(9)?;
    let metadata: HashMap<String, Value> = serde_json::from_str(&metadata_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e)))?;
    let confidence_str: String = row.get(10)?;
    let confidence = ConfidenceLevel::parse_str(&confidence_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e)))?;

    Ok(Memory {
        id, content: row.get(1)?, memory_type, importance: row.get(3)?, access_count: row.get(4)?,
        created_at, last_accessed, expires_at, superseded_by, metadata, confidence,
    })
}
```
Every explicit `SELECT {MEMORY_COLUMNS}` used after this step (M7's `query_by_time_range`,
`created_or_accessed_since`, `get_active_memories`) reads all 11 columns through this one function
— there is no second decoder to fall out of sync.

### Step 6.4 — Swap the ranking stub
```rust
let confidence_weight = memory.confidence.weight(); // replaces `1.0_f32` in recall_query() (Step 4.3)
```

### Step 6.5 — `bump_access_stats` (Step 4.3) is edited in place, not duplicated (fixes #10)
```rust
// engine.rs — the SAME function Step 4.3 defined, body replaced, name unchanged and still the
// only call site inside recall_query()'s loop:
fn bump_access_stats(&self, id: Uuid) -> Result<()> {
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
No second helper is introduced, and none is left over — `cargo clippy` has nothing to flag.

### Steps 6.6–6.10 — Tests + checkpoint
- Confidence repair test on a hand-created `memories` table without the column; assert the column
  and its `CHECK` now exist, **and** `schema_migrations` now contains both version 1 and version 2
  (fixes finding #15, tested).
- Idempotent reopen: run migrations twice, no error, one row per version in `schema_migrations`.
- Round-trip per `ConfidenceLevel` through `row_to_memory` (fixes finding #16, tested directly).
- `Inferred` scores lower than an otherwise-identical `Explicit` memory.
- Recalling an `Inferred` memory via `recall_query()` exactly 5 times promotes it to `Reinforced`.
- `cargo clippy`/`fmt` clean, **zero dead-code warnings**. **Checkpoint:** `cargo test` green
  through M6.

---

# M7 — Temporal querying

### Step 7.1 — `RecallQuery` fields land with their builder methods, in the same step
```rust
// added fields on the existing struct (Step 4.2), initialized to None/None/false in ::new():
pub created_after: Option<DateTime<Utc>>,
pub created_before: Option<DateTime<Utc>>,
pub only_stale: bool,

impl RecallQuery {
    pub fn created_after(mut self, t: DateTime<Utc>) -> Self { self.created_after = Some(t); self }
    pub fn created_before(mut self, t: DateTime<Utc>) -> Self { self.created_before = Some(t); self }
    pub fn only_stale(mut self, b: bool) -> Self { self.only_stale = b; self }
}
```
`recall_query()` (Step 4.3) gains, after its existing filters:
```rust
if let (Some(after), Some(before)) = (query.created_after, query.created_before) {
    if after > before { return Err(MemoliteError::InvalidArgument("created_after must not exceed created_before".into())); }
}
// inside the per-hit loop:
if let Some(after) = query.created_after { if memory.created_at < after { continue; } }
if let Some(before) = query.created_before { if memory.created_at > before { continue; } }
if query.only_stale {
    let stale_cutoff_days = ranking::decay_half_life_days(memory.memory_type) * 2.0;
    let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
    if days_since_access < stale_cutoff_days { continue; }
}
```

### Step 7.2 — Time-range and audit queries
```rust
pub async fn query_by_time_range(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Memory>> {
    if start > end { return Err(MemoliteError::InvalidArgument("start must not be after end".into())); }
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE created_at >= ?1 AND created_at <= ?2 ORDER BY created_at ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![start.timestamp(), end.timestamp()], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Every memory *created or accessed* since `since`. Recalling a memory
/// bumps `last_accessed`, so a pure read legitimately makes a memory show
/// up here — documented as intentional, not a bug. A true "what was
/// edited" query needs a future `updated_at` column (bumped only by
/// `update()`), which does not exist yet — left as a stated follow-up.
pub async fn created_or_accessed_since(&self, since: DateTime<Utc>) -> Result<Vec<Memory>> {
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE created_at >= ?1 OR last_accessed >= ?1 ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![since.timestamp()], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub async fn find_stale_memories(&self) -> Result<Vec<Memory>> {
    let now = Utc::now();
    let active = self.get_active_memories()?;
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
        let Some(next) = self.get(&next_id.to_string()).await? else { break };
        chain.push(next.clone());
        current = next;
    }
    Ok(chain)
}
```

### Tests + checkpoint
- `start > end` → `Err`; `created_after > created_before` → `Err`.
- Backdated fixtures (test-only SQL, never `sleep`) return in correct order.
- `created_or_accessed_since` picks up both newly-created and newly-re-accessed memories.
- `find_stale_memories`/`only_stale` agree on the same cutoff rule.
- Superseded chain of length 3 resolves; a synthetic cyclic `superseded_by` trips the guard.
- **Checkpoint:** `cargo test` green through M7.

---

# M6.5 — Concurrency proof
```rust
fn assert_send_sync<T: Send + Sync>() {}
#[test]
fn memory_engine_is_send_sync() { assert_send_sync::<memolite::MemoryEngine>(); }
```
Audit every function above for a lock guard held across an `.await`. `cargo clippy` clean.
**Checkpoint:** `Send + Sync` proven.

---

# M8 — Streaming ingestion, fully inlined (fixes finding #4)

**File:** `src/streaming.rs`, registered as `pub mod streaming;` in `lib.rs` in this step.

```rust
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use crate::engine::MemoryEngine;
use crate::requests::StoreRequest;
use crate::error::{MemoliteError, Result};
use crate::MemoryType;

/// One unit of text handed to a `StreamIngestor`.
#[derive(Debug, Clone)]
pub struct IngestChunk {
    pub text: String,
    pub memory_type: MemoryType,
    pub importance: f32,
}

#[derive(Debug, Default, Clone)]
pub struct IngestReport {
    pub received: usize,
    pub stored: usize,
    pub failed: usize,
}

/// Splits incoming text on sentence boundaries (`.`, `!`, `?` followed by
/// whitespace or end-of-input), unicode-safe (operates on `char`s, not
/// bytes, so it never splits inside a multi-byte codepoint).
#[derive(Default)]
pub struct SentenceBuffer {
    pending: String,
}
impl SentenceBuffer {
    pub fn new() -> Self { Self { pending: String::new() } }

    /// Feed more text in; returns any complete sentences found so far.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        self.pending.push_str(text);
        let mut out = Vec::new();
        loop {
            let Some(boundary) = self.pending.char_indices().find_map(|(i, c)| {
                if matches!(c, '.' | '!' | '?') {
                    let next = self.pending[i + c.len_utf8()..].chars().next();
                    if next.is_none() || next.map(|n| n.is_whitespace()).unwrap_or(false) {
                        return Some(i + c.len_utf8());
                    }
                }
                None
            }) else { break };
            let sentence: String = self.pending[..boundary].trim().to_string();
            self.pending = self.pending[boundary..].trim_start().to_string();
            if !sentence.is_empty() { out.push(sentence); }
        }
        out
    }

    /// Flushes whatever partial sentence remains (e.g. at stream end).
    pub fn finish(mut self) -> Option<String> {
        let rest = self.pending.trim().to_string();
        self.pending.clear();
        if rest.is_empty() { None } else { Some(rest) }
    }
}

pub struct IngestorSender {
    tx: mpsc::Sender<IngestChunk>,
}
impl IngestorSender {
    pub async fn send(&self, chunk: IngestChunk) -> Result<()> {
        self.tx.send(chunk).await.map_err(|_| MemoliteError::Internal("ingest channel closed".into()))
    }
    pub fn clone_handle(&self) -> Self { Self { tx: self.tx.clone() } }
}

pub struct StreamIngestor {
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<IngestReport>,
    sender: IngestorSender,
}

impl StreamIngestor {
    /// Spawns a background task that consumes `IngestChunk`s from an
    /// internal mpsc channel of capacity `buffer_size` and stores each one
    /// via `engine.store_with_options`. `buffer_size == 0` is rejected —
    /// a zero-capacity channel can never be fed without an immediate
    /// receiver ready, which is not how this is used.
    pub fn spawn(engine: Arc<MemoryEngine>, buffer_size: usize) -> Result<Self> {
        if buffer_size == 0 {
            return Err(MemoliteError::InvalidArgument("buffer_size must be > 0".into()));
        }
        let (tx, mut rx) = mpsc::channel::<IngestChunk>(buffer_size);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let join = tokio::spawn(async move {
            let mut report = IngestReport::default();
            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    maybe_chunk = rx.recv() => {
                        let Some(chunk) = maybe_chunk else { break }; // channel closed, drain complete
                        report.received += 1;
                        let request = StoreRequest::new(&chunk.text, chunk.memory_type, chunk.importance);
                        match engine.store_with_options(request).await {
                            Ok(_) => report.stored += 1,
                            Err(_) => report.failed += 1,
                        }
                    }
                }
            }
            report
        });

        Ok(Self { cancel, join, sender: IngestorSender { tx } })
    }

    pub fn sender(&self) -> IngestorSender { self.sender.clone_handle() }

    /// Cancels immediately; any chunks still queued in the channel are
    /// never processed. Prompt return, does not wait for a drain.
    pub async fn shutdown_now(self) -> Result<IngestReport> {
        self.cancel.cancel();
        self.join.await.map_err(|e| MemoliteError::Internal(e.to_string()))
    }

    /// Drops this ingestor's own sender handle and waits for the channel to
    /// close naturally — i.e. once every cloned `IngestorSender` the caller
    /// is holding is also dropped, standard mpsc close semantics. Drains
    /// the full backlog before the task exits.
    pub async fn finish(self) -> Result<IngestReport> {
        drop(self.sender);
        self.join.await.map_err(|e| MemoliteError::Internal(e.to_string()))
    }
}
```

### Tests + checkpoint
- `SentenceBuffer::feed` unicode/boundary tests (multi-byte chars, no boundary, mid-word periods
  like "e.g." treated as a boundary — documented as a known simplification, not a bug).
- `IngestReport` accuracy, including a forced per-chunk failure (empty content) leaving
  `failed == 1` while the loop keeps consuming.
- `finish()`: send 5, drop every sender clone, `received == 5 && stored == 5`.
- `shutdown_now()` against a slow storage test-double: prompt return, `received <= 5`.
- Backpressure test, `buffer_size = 1`, all 5 eventually land via `finish()`.
- `spawn(engine, 0)` → `Err(InvalidArgument)`.
- **Checkpoint:** streamed content retrievable end-to-end; both shutdown modes verified by test;
  every type/method listed above is now concretely defined (fixes finding #4).

---

# M9 — Compression + index rebuild, fully inlined (fixes finding #5), missing embeddings fail
loudly (fixes finding #11)

**File:** `src/compression.rs`, registered as `pub mod compression;` in `lib.rs` in this step.

```rust
use uuid::Uuid;
use crate::engine::Memory;
use crate::error::{MemoliteError, Result};

pub const COMPRESSION_ALGORITHM_VERSION: u32 = 1;
pub const MAX_SUMMARY_CHARS: usize = 2000;

pub struct Cluster {
    pub member_ids: Vec<Uuid>,
}

pub struct CompressionResult {
    pub summary_content: String,
    pub original_ids: Vec<Uuid>,
}

pub fn is_compression_eligible(mem: &Memory) -> bool {
    let age_days = (chrono::Utc::now() - mem.created_at).num_days();
    let not_expired = mem.expires_at.map(|e| e >= chrono::Utc::now()).unwrap_or(true);
    mem.memory_type == crate::MemoryType::Episodic && age_days > 14 && mem.importance < 0.3
        && mem.superseded_by.is_none() && not_expired
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// Simple greedy single-linkage clustering: for each not-yet-assigned
/// vector, start a new cluster and pull in every remaining vector within
/// `threshold` cosine similarity of it. O(n^2), intentionally simple —
/// documented as extractive, not ML clustering.
pub fn greedy_cluster(vectors: &[(Uuid, Vec<f32>)], threshold: f32) -> Vec<Cluster> {
    let mut assigned = vec![false; vectors.len()];
    let mut clusters = Vec::new();
    for i in 0..vectors.len() {
        if assigned[i] { continue; }
        assigned[i] = true;
        let mut member_ids = vec![vectors[i].0];
        for j in (i + 1)..vectors.len() {
            if assigned[j] { continue; }
            if cosine(&vectors[i].1, &vectors[j].1) >= threshold {
                assigned[j] = true;
                member_ids.push(vectors[j].0);
            }
        }
        clusters.push(Cluster { member_ids });
    }
    clusters
}

/// Extractive summary: concatenates each member's content, truncated to
/// `MAX_SUMMARY_CHARS` total, joined with " | ". Not LLM-abstractive —
/// stated plainly, matching the "Risks and Honest Limitations" section.
pub fn summarize_cluster(members: &[Memory], _threshold: f32) -> Result<CompressionResult> {
    if members.is_empty() {
        return Err(MemoliteError::InvalidArgument("cannot summarize an empty cluster".into()));
    }
    let mut summary = String::new();
    for (i, m) in members.iter().enumerate() {
        if i > 0 { summary.push_str(" | "); }
        summary.push_str(&m.content);
        if summary.len() >= MAX_SUMMARY_CHARS {
            summary.truncate(MAX_SUMMARY_CHARS);
            break;
        }
    }
    Ok(CompressionResult { summary_content: summary, original_ids: members.iter().map(|m| m.id).collect() })
}
```

### `MemoryEngine::get_episodic_memories_older_than` — the missing query M9 called

```rust
// engine.rs
fn get_episodic_memories_older_than(&self, days: i64) -> Result<Vec<Memory>> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).timestamp();
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE type = 'episodic' AND created_at <= ?1");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![cutoff], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
```

### `rebuild_vector_index` — via `replace_all`, backend-agnostic
```rust
async fn rebuild_vector_index(&self) -> Result<()> {
    reconcile_vector_index(&self.conn, &{
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    }, BackfillPolicy::ReplaceAll).await
}
```

### `compress_old_memories()` — missing embedding is now an `Err`, not a silent drop (fixes #11)
```rust
pub async fn compress_old_memories(&self) -> Result<usize> {
    let candidates: Vec<Memory> = self.get_episodic_memories_older_than(14)?.into_iter().filter(compression::is_compression_eligible).collect();

    let mut with_vectors = Vec::with_capacity(candidates.len());
    for m in &candidates {
        // A candidate came straight out of `memories`, which — per Step
        // 5.4's invariant — always has a matching embeddings row. A miss
        // here means the database is corrupted, so this fails loudly
        // instead of silently excluding the candidate from clustering.
        let vector = self.get_embedding(&m.id.to_string()).await?
            .ok_or_else(|| MemoliteError::Corruption(format!("memory {} has no persisted embedding", m.id)))?;
        crate::vector_store::validate_vector(&format!("embedding for {}", m.id), &vector, vector.len().max(1))?; // finiteness only; dim is self-consistent here
        if !vector.iter().all(|x| x.is_finite()) {
            return Err(MemoliteError::Corruption(format!("embedding for {} contains a non-finite value", m.id)));
        }
        with_vectors.push((m.id, vector));
    }

    let clusters = compression::greedy_cluster(&with_vectors, 0.85);
    let mut compressed_count = 0;

    for cluster in clusters.into_iter().filter(|c| c.member_ids.len() >= 3) {
        let members: Vec<Memory> = candidates.iter().filter(|m| cluster.member_ids.contains(&m.id)).cloned().collect();
        let result = compression::summarize_cluster(&members, 0.85)?;

        let mut metadata = HashMap::new();
        metadata.insert("compression.original_ids".into(), serde_json::json!(result.original_ids.iter().map(Uuid::to_string).collect::<Vec<_>>()));
        metadata.insert("compression.algorithm_version".into(), serde_json::json!(compression::COMPRESSION_ALGORITHM_VERSION));

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
original superseded are two separate SQLite writes; a hard process kill between them is not
crash-atomic — compensation only covers the failure-returns-an-error case. Stated as a known
backlog item, not implied solved.

### Tests + checkpoint
- Eligibility boundary, greedy-clustering, empty-cluster → `Err`.
- Integration: 3 similar low-importance episodic memories → `compress_old_memories()` returns `3`.
- **New — the exact test finding #11 asked for:** manually delete one candidate's embeddings row
  via test-only SQL, call `compress_old_memories()` → `Err(Corruption)`, not a silently-shrunk
  cluster.
- Rebuild-consistency test: after compression, `rebuild_vector_index()`, then
  `recall_query().include_superseded(true)` still finds the originals.
- Backend-preservation test: test-double `VectorStore`, `rebuild_vector_index()` calls
  `replace_all` on that same instance (call-count instrumented), never constructs a new backend.
- Compressed summaries are `MemoryType::Semantic`; recallable past the episodic TTL window.
- Restart test: after compression, reopen, vector-store contents match SQLite's full row set.
- **Checkpoint:** `get_embedding` calls all correctly awaited; missing-embedding candidates fail
  loudly; rebuild works identically across backends; every type M9 uses (`Cluster`,
  `CompressionResult`, `COMPRESSION_ALGORITHM_VERSION`, `MAX_SUMMARY_CHARS`, `greedy_cluster`,
  `summarize_cluster`) is concretely defined above (fixes finding #5).

---

# M9.5 — `MemoryStats` / `stats()` (fixes finding #6)

**File:** `src/stats.rs`, registered as `pub mod stats;` in `lib.rs` in this step.

```rust
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MemoryStats {
    pub total_memories: usize,
    pub by_type: std::collections::HashMap<String, usize>,
    pub by_confidence: std::collections::HashMap<String, usize>,
    pub superseded_count: usize,
    pub expired_count: usize,
    pub average_importance: f32,
    pub average_access_count: f32,
}
```
```rust
// engine.rs
pub async fn stats(&self) -> Result<crate::stats::MemoryStats> {
    let now = Utc::now().timestamp();
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;

    let total_memories: usize = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get::<_, i64>(0))? as usize;
    let superseded_count: usize = conn.query_row("SELECT COUNT(*) FROM memories WHERE superseded_by IS NOT NULL", [], |r| r.get::<_, i64>(0))? as usize;
    let expired_count: usize = conn.query_row("SELECT COUNT(*) FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1", rusqlite::params![now], |r| r.get::<_, i64>(0))? as usize;

    let mut by_type = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT type, COUNT(*) FROM memories GROUP BY type")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))?;
        for row in rows { let (t, c) = row?; by_type.insert(t, c); }
    }
    let mut by_confidence = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT confidence, COUNT(*) FROM memories GROUP BY confidence")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))?;
        for row in rows { let (c, n) = row?; by_confidence.insert(c, n); }
    }
    let (average_importance, average_access_count) = if total_memories == 0 {
        (0.0, 0.0)
    } else {
        let avg_imp: f64 = conn.query_row("SELECT AVG(importance) FROM memories", [], |r| r.get(0))?;
        let avg_acc: f64 = conn.query_row("SELECT AVG(access_count) FROM memories", [], |r| r.get(0))?;
        (avg_imp as f32, avg_acc as f32)
    };

    Ok(crate::stats::MemoryStats { total_memories, by_type, by_confidence, superseded_count, expired_count, average_importance, average_access_count })
}
```

### Tests + checkpoint
- Empty engine: `total_memories == 0`, both averages `0.0`, no panic on the `AVG` of an empty
  table.
- Mixed fixture: counts per type/confidence match manually-computed expectations.
- After `update()` (creating a superseded original) and one expired fixture: `superseded_count`
  and `expired_count` both reflect it.
- **Checkpoint:** `stats()` reachable and tested — finding #6 closed by adding the type, not by
  documenting its removal.

---

# M10 — Maintenance controller

```rust
// src/maintenance.rs
pub struct MaintenanceConfig { pub purge_interval: std::time::Duration, pub compress_interval: std::time::Duration }
pub struct MaintenanceHandle { cancel: tokio_util::sync::CancellationToken, join: tokio::task::JoinHandle<()>, running_flag: std::sync::Arc<std::sync::atomic::AtomicBool> }

impl crate::MemoryEngine {
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
    pub async fn shutdown(self) -> Result<()> {
        self.cancel.cancel();
        let result = self.join.await;
        self.running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        result.map_err(|e| MemoliteError::Internal(e.to_string()))
    }
}
```
`purge_expired()` follows `forget()`'s shape (Step 3.2): delete from SQLite, best-effort delete
from the vector store, `reconcile_vector_index(..., BackfillPolicy::ReplaceAll)` on failure.

### Tests + checkpoint
- Paused-clock interval tests; cancellation exits promptly.
- Zero-interval config → `Err`, no panic.
- Second `start_maintenance` while running → `Err`; succeeds after `shutdown()`.
- Panic-recovery test: force a panic inside the loop, `shutdown()` returns `Err`, a subsequent
  `start_maintenance()` then succeeds.
- Engine-drop test: drop the last `Arc<MemoryEngine>`, advance time, `shutdown()` — task had
  already exited via `upgrade()` failure.
- **Checkpoint:** purge/compression opt-in only, fallible start, single controller, recoverable.

---

# M11 — `generic-http` vector backend, `open_with_store` with an explicit `BackfillPolicy`

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

### Adapter — one client, timeout set once, every method validates via `validate_vector` (fixes
findings #11/#12/#14 as applied to the HTTP backend)
```rust
// src/vector_store/generic_http.rs
use async_trait::async_trait;
use std::collections::HashMap;
use serde_json::Value;
use uuid::Uuid;
use crate::error::{MemoliteError, Result};
use super::{validate_vector, VectorEntry, VectorHit, VectorStore};

pub struct GenericHttpVectorStore { client: reqwest::Client, base_url: String, dim: usize }

impl GenericHttpVectorStore {
    pub fn new(base_url: impl Into<String>, dim: usize) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(Self { client, base_url: base_url.into(), dim })
    }
}

impl std::fmt::Debug for GenericHttpVectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenericHttpVectorStore").field("base_url", &self.base_url).field("dim", &self.dim).finish()
    }
}

#[async_trait]
impl VectorStore for GenericHttpVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        validate_vector(&format!("vector for {id}"), vector, self.dim)?;
        self.client.put(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .json(&serde_json::json!({ "vector": vector, "metadata": metadata })).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        validate_vector("query", query, self.dim)?;
        #[derive(serde::Deserialize)] struct RawHit { id: String, score: f32 }
        const MAX_RESULTS: usize = 10_000;

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
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(k);
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
            validate_vector(&format!("entry for {}", e.id), &e.vector, self.dim)?; // dim + finiteness, fixes #14
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

### Public constructor — explicit `BackfillPolicy` required (fixes finding #13)
```rust
impl crate::MemoryEngine {
    /// Opens the engine backed by a caller-supplied `VectorStore`. `backfill`
    /// is mandatory (no default): the caller must state whether this
    /// backend/collection is dedicated to this database (`ReplaceAll`, safe
    /// to reconcile destructively) or shared with other data
    /// (`ExistingOnly`/`UpsertLocal`, never deletes anything this database
    /// didn't write). This closes finding #13 — v5's version always
    /// destructively replaced the remote store's contents with no way to
    /// opt out.
    pub async fn open_with_store(
        path: impl AsRef<std::path::Path>,
        store: std::sync::Arc<dyn crate::vector_store::VectorStore>,
        backfill: crate::BackfillPolicy,
    ) -> Result<Self> {
        Self::open_with_store_internal(path, Some(store), backfill).await
    }
}
```

### Tests + checkpoint
- `wiremock`-based unit tests, including `search()` against a real JSON body.
- Dimension-mismatch and non-finite tests: `search()`/`insert()`/`replace_all()` called with bad
  vectors → `Err` before any HTTP call is made (fixes findings #7/#14, tested on this backend too).
- Over-length response test: a mocked response with more than `MAX_RESULTS` entries → `Err`.
- `contains()`: a `500` response surfaces as `Err`, not `Ok(false)`.
- `replace_all()` integration test against wiremock: insert 3 via `insert`, `replace_all` with 2 of
  them plus a new one, confirm the dropped one is gone and the new one is present.
- **New — `BackfillPolicy` test (fixes finding #13):** open with `ExistingOnly` against a wiremock
  server that already has vectors SQLite doesn't know about; assert no `replace_all`/`delete` call
  was made (call-count instrumented). Open with `UpsertLocal`; assert only `insert` calls were made
  and no `replace_all`. Open with `ReplaceAll`; assert exactly one `replace_all` call was made.
- `open_with_store()` end-to-end test with `BackfillPolicy::ReplaceAll` against a wiremock server:
  `store()`, `recall()` round-trips through the HTTP backend.
- `cargo build` (default features) does not pull in `reqwest` — verified via `cargo tree`.
- `cargo test --all-features` passes with `search()` fully implemented, no `todo!()` anywhere.
- **Checkpoint:** `--all-features` passes; `open_with_store()` requires and respects an explicit
  `BackfillPolicy`; the HTTP backend is provably usable through the public API.

---

# M12 — Final polish, docs, benchmarks, release gate

### `ARCHITECTURE.md` states, in these exact terms:
- Concurrency model: `Mutex<Connection>` + `Mutex<Embedder>` + `RwLock<Arc<dyn VectorStore>>`,
  introduced together with `open()` at Step 0.5 — never a window where the struct outpaces its
  constructor. Lock-then-clone-then-drop-then-await discipline, uniform throughout; `Send + Sync`
  proven at M6.5.
- **Single reconciliation primitive:** `VectorStore::replace_all`, invoked through
  `reconcile_vector_index`'s three `BackfillPolicy` modes — used identically by `open()` (always
  `ReplaceAll`, safe because the default backend is private), `forget`/`purge_expired` failure
  recovery (`ReplaceAll`, same reasoning), M9's rebuild (`ReplaceAll`), and `open_with_store`
  (caller-chosen, because a remote backend may not be exclusively owned).
- **Missing-embedding invariant:** `store_with_options_id` is the only writer of memory+embedding
  pairs and always writes both in one transaction; every reader that joins the two tables
  (`reconcile_vector_index`, `compress_old_memories`) treats a mismatch as `Corruption`, never as a
  row to silently skip.
- **Validation contract:** `vector_store::validate_vector` is the one function that checks
  dimension and finiteness; every backend method that accepts a vector (`insert`, `search`'s
  query, `replace_all`'s entries) calls it — no method is exempt by proximity to another that does.
- Migration scope: two named, transactional migrations — table/index existence (version 1) and the
  `confidence` column (version 2) — both recorded in `schema_migrations`. Nothing else is verified
  or repaired.
- Vector-index policy: holds every memory row with an embedding, including superseded/expired,
  until the row is actually deleted; filtering is `recall_query`'s job.
- `replace_all`'s atomicity is exact for `InMemoryVectorStore`; for `GenericHttpVectorStore` it is
  only as atomic as the remote `/vectors:replace_all` endpoint actually is.
- `ExpiryPolicy`'s three states; `update()`'s explicit refusal to silently revive an expired
  memory; the typed-`Uuid`-core pattern; `created_or_accessed_since`'s exact semantics.
- `MemoryStats`/`stats()` as the crate's observability surface.
- Compression's storage-type policy (`Semantic`, not `Episodic`), its loud failure on a missing
  embedding, and its one honestly-documented cross-transaction gap between summary creation and
  marking originals superseded.
- Maintenance's single-controller enforcement, fallible start, panic-recovery contract.
- The `generic-http` backend's real, tested `search()`/`replace_all()` contract, and
  `open_with_store(path, store, backfill)` — with `BackfillPolicy` explained and its default-risk
  (`ReplaceAll` on a shared collection) called out explicitly.

### "Risks and Honest Limitations" states:
- Compression is extractive/concatenation-based, not LLM-abstractive.
- `as_prompt_context()` delimits content; it does not sanitize against prompt injection.
- `replace_all`'s atomicity guarantee is backend-dependent; only the in-memory backend gets the
  strong guarantee.
- The migration runner's repair scope is intentionally narrow — two named migrations, not a
  general schema-validation tool.
- Compression's two-write sequence (summary, then supersession) is not crash-atomic; a hard
  process kill between them is a known, stated gap, with a transactional redesign left as a
  backlog item.
- `open_with_store` with `BackfillPolicy::ReplaceAll` against a backend/collection shared with
  other data will delete those other vectors — this is stated as the caller's responsibility to
  avoid by choosing `ExistingOnly`/`UpsertLocal`, not something the library can detect on its own.

### Benchmarks (fixes finding #17)
**File:** `benches/memolite_bench.rs`, wired via the `[[bench]]` entry added in Step 0.1.
```rust
use criterion::{criterion_group, criterion_main, Criterion, BatchSize};
use memolite::{MemoryEngine, MemoryType, InMemoryVectorStore, VectorStore};
use std::sync::Arc;

fn bench_in_memory_search(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    for &n in &[1_000usize, 10_000, 100_000] {
        let store = rt.block_on(async {
            let store = InMemoryVectorStore::new(384);
            for i in 0..n {
                let v: Vec<f32> = (0..384).map(|j| ((i * 31 + j) % 97) as f32 / 97.0).collect();
                store.insert(uuid::Uuid::new_v4(), &v, Default::default()).await.unwrap();
            }
            store
        });
        let query: Vec<f32> = (0..384).map(|j| (j % 97) as f32 / 97.0).collect();
        c.bench_function(&format!("in_memory_search_{n}"), |b| {
            b.iter(|| rt.block_on(store.search(&query, 10)))
        });
    }
}

fn bench_store_and_recall(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("store_single_memory", |b| {
        b.iter_batched(
            || rt.block_on(MemoryEngine::open(":memory:")).unwrap(),
            |engine| { rt.block_on(engine.store("benchmark content", MemoryType::Episodic, 0.5)).unwrap(); },
            BatchSize::SmallInput,
        );
    });
    let engine = Arc::new(rt.block_on(MemoryEngine::open(":memory:")).unwrap());
    rt.block_on(async { for i in 0..1000 { engine.store(&format!("fact number {i}"), MemoryType::Semantic, 0.5).await.unwrap(); } });
    c.bench_function("recall_against_1000_memories", |b| {
        b.iter(|| rt.block_on(engine.recall("fact number 500")))
    });
}

criterion_group!(benches, bench_in_memory_search, bench_store_and_recall);
criterion_main!(benches);
```
Additional benches follow the same `iter_batched`/pre-populated-engine shape for: SQLite `get()`,
index resynchronization (`reconcile_vector_index` at 1k/10k rows), and compression candidate
clustering (`greedy_cluster` at varying candidate counts).

### Final validation
- `cargo fmt --check`; `cargo clippy --all-targets --all-features -- -D warnings`.
- `cargo test --all-targets --all-features` **and** the default-feature test run, separately.
- `cargo bench` — all benchmark targets run to completion.
- `cargo doc --no-deps --all-features` — zero warnings.
- Fresh clone → fresh build → fresh `open()`, exercising Step 0's migration + reconcile path with
  no prior state.
- **Final checkpoint — release gate, not automatic:** after the user reviews the diff, release
  notes, and semver choice, the user may explicitly authorize a git tag. No automatic tagging.

---

## Cross-reference: every v5-review finding and where v6 fixes it

| # | Finding | Root cause it was folded into | Fix in v6 |
|---|---|---|---|
| 1 | Step 0 changes `MemoryEngine`'s fields before a compatible `open()` exists; Step-0 checkpoint fails | Struct and constructor defined in different milestones | Step 0.5 now introduces the final struct **and** its only `open()`/`open_with_store_internal` together; M3 never touches either again |
| 2 | `InvalidConfidence` referenced but never defined | Missing type | Folded into the crate's existing `MemoliteError` enum (Step 0.2) as `InvalidConfidence(String)`; `ConfidenceLevel::parse_str` returns the crate's own `Result`, no second error type, `serde::{Serialize, Deserialize}` imported explicitly (Step 6.1) |
| 3 | New modules not consistently registered | Implicit/partial module wiring | One authoritative final `lib.rs` block stated up front, annotated with which step fills in each line; every type used anywhere lives on a named line |
| 4 | M8 describes but doesn't implement `StreamIngestor` etc. | Narrative milestone instead of code | M8 fully inlines `IngestChunk`, `IngestReport`, `StreamIngestor`, `SentenceBuffer`, `IngestorSender`, `spawn`/`sender`/`shutdown_now`/`finish` |
| 5 | M9 calls undefined `greedy_cluster`/`summarize_cluster`/`Cluster`/`CompressionResult`/etc. | Same as #4 | M9 fully inlines `compression.rs`: `Cluster`, `CompressionResult`, `COMPRESSION_ALGORITHM_VERSION`, `MAX_SUMMARY_CHARS`, `greedy_cluster`, `summarize_cluster`, plus `get_episodic_memories_older_than` |
| 6 | `MemoryStats`/`stats()` vanished without a decision | Undecided scope | New M9.5 adds a complete `MemoryStats` struct and `stats()` method, tested |
| 7 | `InMemoryVectorStore::search()` doesn't validate the query; `insert()` doesn't reject non-finite | Validation wasn't shared across sibling methods | One `validate_vector(label, v, dim)` helper (Step 0.3) called by `insert`, `search`, and `replace_all` on every backend — no method is exempt |
| 8 | `forget()` parses the UUID after already deleting from SQLite | Ordering bug | Step 3.2 parses first; malformed ids are now always rejected with zero side effects, tested both directions |
| 9 | M4's `recall_query()` reintroduces the pre-increment bug M3 had fixed | Bump/refetch logic defined twice and drifted | Defined exactly once, in M4 Step 4.3 (`bump_access_stats` + refetch loop); M3's `recall()` is a thin wrapper from the start, so there was never a second copy to drift |
| 10 | Old `update_access_stats` becomes dead code once M4 replaces its call site | Same root as #9 | Same fix as #9 — since there's only ever one bump helper, M6 edits it in place instead of orphaning a predecessor; `cargo clippy` has nothing to flag |
| 11 | Compression silently skips candidates with no persisted embedding | Missing-embedding treated as "skip" instead of "corruption" | `compress_old_memories()` now `ok_or_else`s into `Err(Corruption)` on a missing embedding, and validates finiteness before clustering |
| 12 | `resync_vector_index`'s INNER JOIN silently drops memories with no embedding row | Same root as #11 | Renamed `reconcile_vector_index`, now a LEFT JOIN that raises `Err(Corruption)` on any NULL embedding side — sound because Step 5.4 guarantees the pair is always written together |
| 13 | `open_with_store`'s unconditional `resync`/`replace_all` can destroy a shared remote index | Missing policy control | New `BackfillPolicy` enum (`ExistingOnly` / `UpsertLocal` / `ReplaceAll`), mandatory parameter on `open_with_store` (M11); `open()` always uses `ReplaceAll` safely because its backend is always private |
| 14 | HTTP `replace_all()`/`insert()` don't check for NaN/infinity | Same root as #7, applied to the HTTP backend | The same shared `validate_vector` helper is called by every `GenericHttpVectorStore` method that accepts a vector (M11) |
| 15 | Confidence repair not recorded as its own migration version | Migration history incomplete | `run_migrations` now records migration 1 (baseline) and migration 2 (confidence) separately and transactionally in `schema_migrations` (Step 0.7 / 6.2) |
| 16 | `row_to_memory`'s post-confidence decoder never fully written | Missing implementation | Step 6.3 gives the exact 11-column decoder, including the `confidence` column and its `FromSqlConversionFailure` mapping |
| 17 | No benchmark target/dependency/dataset | Missing scope | `criterion` dev-dependency and `[[bench]]` entry added in Step 0.1; `benches/memolite_bench.rs` fully written in M12, `cargo bench` added to final validation |

v6's ordering guarantee, unchanged in spirit from v5 but now actually true at every point,
including Step 0 itself: **every code block, at every milestone, only calls functions and
references fields that were fully defined at or before that point in this document — and no
milestone leaves a type described-but-undefined for a later milestone to invent.**




#### SHORTCOMINGS OF THIS BUILDING PLAN ###

V6 closes many V5 findings, but it still is not fully executable milestone-by-milestone. The remaining faults are now mostly sequencing and several concrete compile/runtime bugs.

No repository files were modified.

## Critical compile blockers

### 1. Step 0 migration code references the future M6 confidence module

At [plan line 541](C:\Users\Mayan\.codex\attachments\e7bdf8e4-1ec7-4d93-a491-2fa2874fcf22\pasted-text.txt:541), the Step 0 migration runner calls:

```rust
crate::confidence::repair_confidence_column(conn)?;
```

But `src/confidence.rs` and `repair_confidence_column()` are not created until M6.

Therefore Step 0 cannot compile.

Correct sequencing:

```rust
// Step 0 run_migrations():
run_baseline_migration(conn)?;
```

Then M6 edits it to:

```rust
run_baseline_migration(conn)?;
crate::confidence::repair_confidence_column(conn)?;
```

Alternatively, create the confidence migration infrastructure in Step 0 and only add the typed ranking behavior in M6.

### 2. The “authoritative final lib.rs” breaks every early milestone

At [plan line 635](C:\Users\Mayan\.codex\attachments\e7bdf8e4-1ec7-4d93-a491-2fa2874fcf22\pasted-text.txt:635), Step 0 declares modules whose files do not exist yet:

```rust
pub mod ranking;
pub mod requests;
pub mod confidence;
pub mod streaming;
pub mod compression;
pub mod maintenance;
pub mod stats;
```

It also exports types that do not exist until later milestones.

Rust requires every declared module file and exported item to exist immediately. The Step 0 checkpoint will fail.

Keep the final block as a reference, but add module lines incrementally. At Step 0, only register files that actually exist:

```rust
pub mod embedder;
pub mod engine;
pub mod error;
pub mod memory;
pub mod recall;
pub mod vector_store;
mod migrations;

pub use engine::{BackfillPolicy, MemoryEngine};
pub use error::{MemoliteError, Result};
pub use memory::{Memory, MemoryType};
pub use vector_store::{
    InMemoryVectorStore, VectorEntry, VectorHit, VectorStore,
};
```

Then add later modules in their corresponding milestones.

### 3. The final `lib.rs` accidentally removes the existing memory module

The actual crate currently has:

```rust
pub mod memory;
pub use memory::{Memory, MemoryType};
```

V6’s authoritative block omits both lines. That breaks existing imports and the public API.

Preserve them exactly.

### 4. M3 `recall()` references M4 APIs

At [plan line 783](C:\Users\Mayan\.codex\attachments\e7bdf8e4-1ec7-4d93-a491-2fa2874fcf22\pasted-text.txt:783), M3 implements:

```rust
self.recall_query(crate::RecallQuery::new(query_text))
```

But neither `RecallQuery` nor `recall_query()` exists until M4. Consequently the M3 checkpoint cannot compile or run its recall tests.

Use one of these options:

- Keep a temporary M3 cosine-only `recall()` implementation, then replace it with delegation in M4.
- Move `RecallQuery`, `RecallResult`, ranking and `recall_query()` into M3.

The first option is simpler and matches the natural milestone progression.

### 5. `open_with_store_internal()` is inaccessible from M11

It is defined as a private associated function inside the `engine` module:

```rust
async fn open_with_store_internal(...)
```

M11 implements `open_with_store()` from another module and calls it:

```rust
Self::open_with_store_internal(...)
```

Rust module privacy prevents that.

Change it to:

```rust
pub(crate) async fn open_with_store_internal(...)
```

or define the public `open_with_store()` inside `engine.rs`.

## Additional compile/correctness faults

### 6. Compression imports `Memory` from the wrong module

V6 uses:

```rust
use crate::engine::Memory;
```

But `Memory` is defined in `crate::memory`, and `engine` does not publicly re-export it.

Use:

```rust
use crate::memory::Memory;
```

or, after restoring the crate-root export:

```rust
use crate::Memory;
```

### 7. M9 does not validate embedding dimension correctly

This line is ineffective:

```rust
validate_vector(
    &format!("embedding for {}", m.id),
    &vector,
    vector.len().max(1),
)?;
```

It compares the vector length to itself, so every nonempty length passes. It only indirectly checks finiteness.

Validate against the active backend dimension:

```rust
let expected_dim = {
    let guard = self.vector_store
        .read()
        .map_err(|_| MemoliteError::Internal(
            "vector-store lock poisoned".into()
        ))?;
    guard.dimension()
};

validate_vector(
    &format!("embedding for {}", m.id),
    &vector,
    expected_dim,
)?;
```

Also read and verify the persisted `embeddings.dimension` value. The current repository’s `get_embedding()` ignores that column after fetching it.

### 8. Compression summary truncation can panic on Unicode

V6 does:

```rust
summary.truncate(MAX_SUMMARY_CHARS);
```

`String::truncate()` expects a byte index at a UTF-8 character boundary. `MAX_SUMMARY_CHARS == 2000` may land inside a multibyte character and panic.

Use character-aware truncation:

```rust
summary = summary.chars().take(MAX_SUMMARY_CHARS).collect();
```

The constant is named “chars,” so character counting is the expected behavior.

### 9. `row_to_memory()` silently accepts corrupt timestamps and UUIDs

V6 replaces the repository’s careful conversion logic with:

```rust
DateTime::from_timestamp(...).unwrap_or_default()
Uuid::parse_str(&s).ok()
```

That silently turns corrupt timestamps into defaults and corrupt `superseded_by` values into `None`. This is a regression from the current typed-error behavior.

Retain the existing helpers:

```rust
let created_at = timestamp_to_datetime(created_at_ts, 5)?;
let last_accessed = timestamp_to_datetime(last_accessed_ts, 6)?;

let superseded_by = superseded_by_str
    .map(|s| Uuid::parse_str(&s))
    .transpose()
    .map_err(|e| to_sql_conversion_err(8, e))?;
```

Only append confidence parsing at column 10.

### 10. M6 migration ordering is conceptually reversed

V6 says confidence is “introduced at M6,” but Step 0’s `run_migrations()` already requires it. Besides the compile problem, a Step 0 database immediately lands at schema version 2 before the M6 code understands that column.

Choose a consistent model:

- Recommended: Step 0 only applies version 1. M6 introduces and records version 2.
- Alternative: create the confidence column and enum in Step 0, while M6 only activates weighting/promotion.

Do not mix those models.

## Runtime/behavioral issues

### 11. `shutdown_now()` is not guaranteed to stop promptly

The streaming loop uses an unbiased `tokio::select!` between cancellation and `rx.recv()`. When cancellation is ready and the queue is also continuously ready, Tokio may select receive repeatedly.

For immediate shutdown, bias cancellation:

```rust
tokio::select! {
    biased;

    _ = cancel_clone.cancelled() => break,
    maybe_chunk = rx.recv() => {
        // process chunk
    }
}
```

This is correct here because `finish()` is the separate drain operation. Earlier plans could not use biased cancellation because they incorrectly promised draining from the same method; V6 now has two methods, so biasing `shutdown_now()` is appropriate.

### 12. `IngestReport` loses failure details

It reports only counts:

```rust
received
stored
failed
```

Earlier versions included per-item errors. Without them, callers know something failed but not what or why.

Add:

```rust
pub errors: Vec<IngestFailure>

pub struct IngestFailure {
    pub preview: String,
    pub error: String,
}
```

This is not a compile blocker, but it makes the “observable errors” feature substantially more useful.

### 13. `ExistingOnly` still performs full local reconciliation validation

`reconcile_vector_index()` reads and validates every SQLite memory/embedding pair before matching on:

```rust
BackfillPolicy::ExistingOnly
```

That policy says “do not touch the remote store,” but opening can still fail because a local embedding is corrupt. This may be intentional, but the documentation should state it.

If `ExistingOnly` truly means “do not reconcile at all,” return before loading entries:

```rust
if policy == BackfillPolicy::ExistingOnly {
    return Ok(());
}
```

### 14. `UpsertLocal` can leave stale vectors for deleted local rows

That is inherent in its nondestructive design and acceptable for a shared backend, but recall may repeatedly receive stale remote hits that SQLite then rejects. Document candidate-pool pollution and recommend per-database namespaces/collections when possible.

## Benchmark faults

### 15. The store benchmark primarily measures model initialization

This benchmark creates a new engine inside every iteration:

```rust
|| MemoryEngine::open(":memory:")
```

`MemoryEngine::open()` loads the FastEmbed model, which is expensive. The benchmark named `store_single_memory` therefore measures model loading plus engine initialization plus storing.

Separate these:

- `engine_open_with_model_load`
- `store_single_memory` using a pre-opened engine or batched temporary databases with one already-loaded embedder
- `recall_against_N_memories`

Model loading may dominate so heavily that the actual store performance becomes invisible.

### 16. Benchmarking 100,000 vectors may make `cargo bench` very slow

Each Criterion benchmark performs many full linear scans over 100k × 384 floats. That is useful as a scale test, but default `cargo bench` could take a long time.

Use a benchmark group with reduced sample size and measurement duration for 100k:

```rust
let mut group = c.benchmark_group("vector_search");
group.sample_size(10);
group.measurement_time(Duration::from_secs(10));
```

Or keep 100k as an ignored/manual benchmark profile.

## Scope consistency issue

### 17. `clear()` is now redundant but still mandatory

Once `replace_all()` is the sole reconciliation primitive, `clear()` is no longer required by the engine. Keeping both expands every backend’s required API and can introduce inconsistent semantics.

Consider removing `clear()` from `VectorStore`. Empty replacement already expresses the operation:

```rust
store.replace_all(Vec::new()).await?;
```

If retained for convenience, give it a default implementation:

```rust
async fn clear(&self) -> Result<()> {
    self.replace_all(Vec::new()).await
}
```

## Verdict

V6 materially improves V5:

- `replace_all()` is well designed.
- Backfill policy is explicit.
- Missing embeddings are treated as corruption.
- Streaming and compression are now concretely specified.
- Stats and benchmarks exist.
- Vector validation is centralized.
- Access-stat refetch behavior is consistent in M4.
- Generic HTTP is much more complete.

But four direct blockers remain:

1. Step 0 migration runner references the future confidence module.
2. The upfront final `lib.rs` references future nonexistent modules and omits `memory`.
3. M3 recall references M4 types and methods.
4. M11 cannot call private `open_with_store_internal()`.

Once those four are fixed, the plan will be close to genuinely executable. The remaining items are smaller correctness and quality improvements.

My rating:

- Architecture: 9.2/10
- Persistence and reconciliation: 9/10
- Compile readiness by milestone: 7.5/10
- Self-containedness: 9/10
- Overall executable as written: not yet

This version needs a small sequencing-focused V7 rather than another architectural redesign.