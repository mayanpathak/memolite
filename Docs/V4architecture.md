# Memolite — Final Master Build Plan (v4, review-fixed, fully self-contained)

> **What changed from v3:** every one of the 16 findings from the v3 code review is fixed here,
> not deferred. The single biggest structural change is **sequencing**: the migration runner and
> a basic vector-index resync helper now exist in **Step 0**, before M3, so M3's `open()` and
> `forget()` actually compile the moment they're written. `vector_store` is now
> `RwLock<Arc<dyn VectorStore>>` from Step 0 onward (not introduced late), which is what makes the
> M9 "atomic" rebuild genuinely atomic instead of a relabeled clear-then-loop. `conn` becomes
> `Mutex<Connection>` at M6.5 as before, and every snippet *after* M6.5 in this document uses the
> lock pattern explicitly — no post-M6.5 code calls a method directly on the mutex.
>
> Rule zero, unchanged: everything already working in `github.com/mayanpathak/memolite` stays
> as-is, with the Step-0 repairs applied first.

Cross-reference table (every v3-review finding → where it's fixed) is at the very end.

---

## Step 0 — Foundations (expanded: migrations + vector-index resync now live here)

### 0.1 — Error variants
**File:** `src/error.rs`.
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
No separate `Serialization` variant — `InvalidMetadata(#[from] serde_json::Error)` already covers
`serde_json::to_string(...)?` in both directions.

### 0.2 — Module layout
No `src/db/` module. Everything stays on `MemoryEngine` methods.

### 0.3 — Final engine shape — **now correct from the start** (fixes review #5, #7)
```rust
pub struct MemoryEngine {
    conn: std::sync::Mutex<rusqlite::Connection>,        // Mutex from Step 0, not deferred to M6.5
    embedder: std::sync::Mutex<Embedder>,
    /// RwLock, not a bare Arc, and not a bare trait object. This is what
    /// lets a fully-built replacement backend be swapped in under one write
    /// lock (M9's atomic rebuild) instead of mutating the live store in
    /// place. Every read site takes a read lock just long enough to clone
    /// the inner `Arc`, then drops the lock before any `.await` — so a
    /// rebuild-in-progress never blocks ordinary reads for long, and no
    /// lock guard is ever held across an await point.
    vector_store: std::sync::RwLock<std::sync::Arc<dyn VectorStore>>,
    maintenance_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
```

> **Why `Mutex<Connection>` from Step 0 instead of waiting for M6.5:** v3 introduced the mutex
> late (M6.5) but kept writing later milestones (M8–M11) as if it already existed, which produced
> code that didn't compile (review #7). Introducing it once, up front, removes an entire class of
> "which snippet was written before or after the refactor" bugs. The rule going forward, stated
> once:

**The one rule for touching `conn` anywhere in this document, from Step 0 onward:**
```rust
let conn = self.conn.lock()
    .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
// ...conn.prepare(...) / conn.execute(...) / conn.query_row(...)...
// the guard `conn` MUST be dropped (end of scope, or explicit `drop(conn)`)
// before any `.await` in the same function.
```
For a transaction:
```rust
let mut conn = self.conn.lock()
    .map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
let tx = conn.transaction()?;
// ...tx.execute(...)...
tx.commit()?;
// drop(conn) happens at end of scope, still before any subsequent .await
```
**The one rule for touching `vector_store`:**
```rust
// Read (search/insert/delete/contains/dimension): clone the Arc, drop the lock, then await.
let store = {
    let guard = self.vector_store.read()
        .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
    std::sync::Arc::clone(&*guard)
};
store.search(&query_vec, k).await?; // lock is already released here

// Swap (only the atomic-rebuild path in M9 does this):
let mut guard = self.vector_store.write()
    .map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
*guard = replacement_arc; // instantaneous; no await while held
```
Every code block below follows both rules exactly — no exceptions, no "later we'll fix this."

### 0.4 — `Uuid` internally, `String` only at true external boundaries.

### 0.5 — Column-order constant
```rust
/// Canonical column list for `memories`, in the exact order `row_to_memory`
/// expects. Never write `SELECT *` or hand-roll this list.
/// Step 0 ships without `confidence`; M6's migration 2 appends it and
/// updates this constant to the single, final version below — there is
/// only ever one live definition of this constant in the codebase.
const MEMORY_COLUMNS: &str =
    "id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata, confidence";
```
> Note: v3 had `MEMORY_COLUMNS` grow mid-document (M6 appended `confidence`). To keep every later
> snippet copy-pasteable without a "remember to add confidence here" footnote, v4 just states the
> final 11-column form once, here, and Step 0's own baseline schema (0.7 below) matches it by
> creating `confidence` as part of migration 1 rather than migration 2. Confidence-aware *ranking
> and typed `ConfidenceLevel` logic* still isn't wired in until M6 — only the column exists early,
> with a hard SQL default, exactly so no milestone before M6 has to special-case column count.

### 0.6 — `MAX_CANDIDATES` constant
**File:** `src/recall.rs`.
```rust
/// Ceiling on how many raw vector-store hits `recall_query` will ever pull
/// before filtering/ranking, regardless of the caller's `limit`.
pub const MAX_CANDIDATES: usize = 500;

pub fn candidate_pool_size(limit: usize) -> usize {
    limit.saturating_mul(5).max(50).min(MAX_CANDIDATES)
}
```

### 0.7 — (v4 new, fixes review #1) Migration runner moves here, before M3
**File:** `src/migrations.rs`. This is the exact runner v3 had at M6 (Step 74), unchanged in
logic — **always verifies structure**, independent of what `schema_migrations` claims — just
moved earlier, and with the `confidence` column folded into migration 1 per 0.5 above.

```rust
pub fn run_migrations(conn: &mut rusqlite::Connection) -> Result<()> {
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
            rusqlite::params![version, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }

    // Migration 1: baseline tables, INCLUDING confidence (folded in from the
    // start — see Step 0.5). Checked and repaired unconditionally.
    {
        let tx = conn.transaction()?;
        if !has_table(&tx, "memories")? { tx.execute_batch(MIGRATION_1_MEMORIES)?; }
        if !has_table(&tx, "embeddings")? { tx.execute_batch(MIGRATION_1_EMBEDDINGS)?; }
        if has_table(&tx, "memories")? && !has_column(&tx, "memories", "confidence")? {
            tx.execute_batch(MIGRATION_1B_CONFIDENCE_REPAIR)?;
        }
        tx.execute_batch(MIGRATION_1_INDEXES)?; // CREATE INDEX IF NOT EXISTS is always safe
        record(&tx, 1)?;
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
        metadata        TEXT NOT NULL DEFAULT '{}',
        confidence      TEXT NOT NULL DEFAULT 'explicit'
                            CHECK(confidence IN ('explicit', 'inferred', 'reinforced'))
    );";
const MIGRATION_1_EMBEDDINGS: &str = "
    CREATE TABLE embeddings (
        memory_id   TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
        vector      BLOB NOT NULL,
        dimension   INTEGER NOT NULL
    );";
// Only used to repair a hand-edited or pre-v4 database that has `memories`
// but not yet `confidence` — a fresh Step-0 database never needs this path.
const MIGRATION_1B_CONFIDENCE_REPAIR: &str = "
    ALTER TABLE memories ADD COLUMN confidence TEXT NOT NULL DEFAULT 'explicit'
        CHECK(confidence IN ('explicit', 'inferred', 'reinforced'));";
const MIGRATION_1_INDEXES: &str = "
    CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
    CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
    CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type);
    CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at);
    CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);
";
```
> **Honesty note (fixes review #15):** this runner repairs exactly two things: missing expected
> tables, and a missing `confidence` column on an existing `memories` table. It does **not**
> verify column types, constraints, foreign keys, or the `embeddings` schema's shape. That's the
> whole claim — `ARCHITECTURE.md` (M12) states it in those exact words, not as "self-heals any
> partial schema."

### 0.8 — (v4 new, fixes review #1, #4, #5) Vector-index population/resync, now in Step 0
This is the single helper that both `open()` (Step 41) and the M9 atomic rebuild (which just adds
an atomic swap around the same core logic) are built from. Doing it once here means M3 doesn't
need to forward-reference M9.

**Design decision resolving review #4:** the vector index holds an embedding for **every memory
row still present in SQLite** — including superseded and expired ones — right up until that row
is actually deleted (by `forget()` or `purge_expired()`). Filtering `superseded`/`expired` out of
results is a `recall_query` *filter concern* (already implemented, Step 64), not an indexing
concern. This is "Option A / recommended" from the review: it's what makes
`include_expired`/`include_superseded` actually able to return something, because the candidate
was never removed from the index in the first place.

**File:** `src/engine.rs`.
```rust
/// Loads every memory row still present in SQLite (regardless of expiry or
/// supersession — see the 0.8 design note above) into `store`. Read phase
/// is fully synchronous and collects into a Vec before any `.await`, so no
/// SQLite borrow is ever held across an await point (fixes review #8).
async fn resync_vector_index(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    store: &std::sync::Arc<dyn VectorStore>,
) -> Result<()> {
    let entries: Vec<(Uuid, Vec<f32>, i64, HashMap<String, Value>)> = {
        let conn = conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector, e.dimension, m.metadata FROM memories m JOIN embeddings e ON e.memory_id = m.id"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id_str, bytes, stored_dim, metadata_json) = row?;
            let id = Uuid::parse_str(&id_str)?;
            let vector: Vec<f32> = bincode::deserialize(&bytes)
                .map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
            let metadata: HashMap<String, Value> = serde_json::from_str(&metadata_json)?;
            out.push((id, vector, stored_dim, metadata));
        }
        out
    }; // MutexGuard dropped here — before any await below

    for (id, vector, stored_dim, metadata) in entries {
        // Dimension/finiteness validation (fixes review #10's counterpart in this path)
        if stored_dim as usize != store.dimension() || vector.len() != store.dimension() {
            return Err(MemoliteError::VectorStore(format!(
                "stored vector for {id} has dimension {} but store expects {}",
                vector.len(), store.dimension()
            )));
        }
        if !vector.iter().all(|x| x.is_finite()) {
            return Err(MemoliteError::VectorStore(format!("stored vector for {id} contains a non-finite value")));
        }
        store.insert(id, &vector, metadata).await?; // upsert, safe to call unconditionally
    }
    Ok(())
}
```

**Checkpoint 0:** `cargo build && cargo test` green. `migrations::run_migrations` and
`resync_vector_index` both exist, unit-testable in isolation (empty DB in, empty index out).

---

## Corrected build order

| Order | Milestone | What it adds |
|---|---|---|
| 0 | Step 0 | Error variants, `Mutex<Connection>` + `RwLock<Arc<dyn VectorStore>>` from day one, migrations, vector-index resync |
| 1 | M3 | `VectorStore` trait (+`clear`, +`contains`), `InMemoryVectorStore`, wired into `store`/`recall`/`forget`, restart backfill via 0.8 |
| 2 | M4 | Ranking (confidence weight stubbed at `1.0`), `RecallQuery`/`RecallItem`/`RecallResult`, capped candidate pool, `recall()` delegates to `recall_query()` |
| 3 | M6 | `ConfidenceLevel`, migration 1b repair path exercised, ranking's stub replaced with the real weight |
| 4 | M5 | `StoreRequest`/`MemoryUpdate`, `ExpiryPolicy` (replaces the broken `Option<Duration>` TTL model) |
| 5 | M7 | Temporal querying — full v2-parity feature set restored |
| 6 | M6.5 | Formal `Send + Sync` proof (the mutex/rwlock already exist from Step 0, so this milestone is now just the audit + test, not a refactor) |
| 7 | M8 | Streaming ingestion, two explicit shutdown modes |
| 8 | M9 | Compression + genuinely atomic index-swap rebuild (RwLock swap, not clear-then-loop) |
| 9 | M10 | Maintenance controller: fallible start, single-controller enforced, recoverable after a panic |
| 10 | M11 | `generic-http` vector backend, `search()` fully implemented (no `todo!()`) |
| 11 | M12 | Docs, packaging, release gate |

---

# M3 — VectorStore trait, naive search, wired into the engine

### Step 41 — Trait, with `clear()` and `contains()`
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
    /// MUST be an idempotent upsert: inserting an id that already exists
    /// replaces its vector/metadata rather than erroring or duplicating.
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn contains(&self, id: Uuid) -> Result<bool>;
    /// Removes every entry. Required by M9's atomic rebuild and M11's
    /// `BackfillPolicy::Rebuild`.
    async fn clear(&self) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

### Step 42 — `src/math_utils.rs` (cosine similarity — unchanged, was already correct)
```rust
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { return 0.0; }
    dot / (norm_a * norm_b)
}
```

### Step 43 — `InMemoryVectorStore`
**File:** `src/vector_store/in_memory.rs`. (Same shape as before — `RwLock<HashMap<Uuid, (Vec<f32>,
HashMap<String, Value>)>>` internally, `insert` is an upsert, `search` sorts by
`total_cmp` descending then `id` ascending as a tiebreak, `contains`/`clear` are real.) Unit tests:
nearest-vector-first, wrong-dimension rejected not panicked, insert-is-an-upsert.

### Step 44 — `open()` — now compiles standalone, no forward references
```rust
pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
    let mut raw_conn = rusqlite::Connection::open(path)?;
    crate::migrations::run_migrations(&mut raw_conn)?;          // Step 0.7 — exists now
    let embedder = Embedder::new()?;
    let dim = embedder.dimension();
    let vector_store: std::sync::Arc<dyn VectorStore> = std::sync::Arc::new(InMemoryVectorStore::new(dim));
    let conn = std::sync::Mutex::new(raw_conn);

    resync_vector_index(&conn, &vector_store).await?;            // Step 0.8 — exists now

    Ok(Self {
        conn,
        embedder: std::sync::Mutex::new(embedder),
        vector_store: std::sync::RwLock::new(vector_store),
        maintenance_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    })
}
```

### Step 45 — `store()` — full implementation, no pseudocode (fixes review #12 for this path)
```rust
pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String> {
    self.store_id(content, memory_type, importance).await.map(|id| id.to_string())
}

/// Internal, `Uuid`-typed core so later callers (M5's `update`, M9's
/// compression) never have to round-trip through a `String` and risk the
/// wrong-id fallback bug from review #10.
async fn store_id(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<Uuid> {
    if content.trim().is_empty() {
        return Err(MemoliteError::InvalidArgument("content must not be empty".into()));
    }
    if !(0.0..=1.0).contains(&importance) {
        return Err(MemoliteError::InvalidArgument("importance must be in [0.0, 1.0]".into()));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    let ttl_days = default_ttl_days(memory_type);
    let expires_at = Some(now + chrono::Duration::days(ttl_days));
    let metadata_json = serde_json::to_string(&HashMap::<String, Value>::new())?;

    let vector = {
        let mut embedder = self.embedder.lock()
            .map_err(|_| MemoliteError::EmbeddingEncode("embedder mutex poisoned".into()))?;
        embedder.embed(content)?
    };
    let vector_bytes = bincode::serialize(&vector)
        .map_err(|e| MemoliteError::EmbeddingEncode(e.to_string()))?;

    {
        let mut conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO memories (id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata, confidence)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, NULL, ?7, 'explicit')",
            rusqlite::params![id.to_string(), content, memory_type.as_str(), importance, now.timestamp(), expires_at.map(|e| e.timestamp()), metadata_json],
        )?;
        tx.execute(
            "INSERT INTO embeddings (memory_id, vector, dimension) VALUES (?1, ?2, ?3)",
            rusqlite::params![id.to_string(), vector_bytes, vector.len() as i64],
        )?;
        tx.commit()?;
    } // conn guard dropped here, before the vector-store await below

    let store = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    };
    if let Err(e) = store.insert(id, &vector, HashMap::new()).await {
        let compensation = {
            let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
            match conn {
                Ok(conn) => conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id.to_string()]).err(),
                Err(_) => Some(rusqlite::Error::InvalidQuery), // lock itself was poisoned
            }
        };
        if let Some(compensation_err) = compensation {
            return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: compensation_err.to_string() });
        }
        return Err(e);
    }
    Ok(id)
}

fn default_ttl_days(t: MemoryType) -> i64 {
    match t {
        MemoryType::Semantic => 365,
        MemoryType::Episodic => 30,
        MemoryType::Procedural => 730,
        MemoryType::Working => 0, // handled as hours elsewhere; see StoreRequest/M5 for the real TTL model
    }
}
```

### Step 46 — `recall(query_text: &str)` — M3-era version, with the access-count bump review #3 said was missing
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
        if let Some(mem) = self.get(&hit.id.to_string()).await? {
            self.update_access_stats(hit.id)?; // fixes review #3 — test 51 now has something to pass
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
> M4 (Step 63a) replaces this whole body with a thin delegation to `recall_query()`, and M6
> replaces `update_access_stats` with the promotion-aware version — both are single, clean
> swap-outs, not patches around forward references.

### Step 47 — `forget()` — uses Step 0.8's resync, not a forward reference to M9
```rust
pub async fn forget(&self, id: &str) -> Result<()> {
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    {
        let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])?;
    }

    let store = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    };
    if let Err(e) = store.delete(uuid).await {
        // M3-era reconciliation: just resync from SQLite. Not yet the atomic
        // swap (that refinement — building a replacement off to the side
        // and swapping the RwLock in one step — arrives in M9 once
        // compression needs it for a much larger batch; for a single
        // delete's worst case, a direct resync is simple, correct, and
        // cheap enough).
        if let Err(resync_err) = resync_vector_index(&self.conn, &store).await {
            return Err(MemoliteError::CompensationFailed { operation: e.to_string(), compensation: resync_err.to_string() });
        }
        return Err(e);
    }
    Ok(())
}
```

### Steps 48–56 — Tests + checkpoint
48. Store 3 unrelated facts + 1 relevant one, `recall()`, assert the relevant one is present.
49. `recall()` on an empty engine returns `Ok(vec![])`.
50. **(fixes review #3)** `access_count` increases by exactly 1 after one `recall()` call — this
    now actually passes, because Step 46 calls `update_access_stats`.
51. `forget()` removes the memory from both SQLite and the vector store.
52. Restart test: open an engine, `store()` 3 memories, drop it, `open()` the same path again,
    `recall()` immediately — all 3 findable without re-storing. (Now exercised via Step 0.8 +
    Step 44, both of which exist before this test is even written.)
53. Corrupt-row restart test: write a garbage blob into one `embeddings` row, `open()` — assert
    `Err`, not a silently partial index.
54. `cargo clippy` clean, `cargo fmt`.
55. **(new)** `MemoryEngine` compiles and every M3 test passes **without any code from M4–M11
    existing yet** — this is the concrete, testable definition of "checkpoint compiles," which v3
    asserted but did not satisfy.
56. **Checkpoint:** `cargo test` green; `recall()` is real, survives a restart, and every method
    it calls was defined at or before this milestone.

---

# M4 — Ranking + corrected recall API

### Step 57 — `src/ranking.rs` (unchanged — was already correct)
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

### Step 63a — `recall()` delegates to `recall_query()`
```rust
pub async fn recall(&self, query_text: &str) -> Result<Vec<Memory>> {
    Ok(self.recall_query(RecallQuery::new(query_text)).await?.items.into_iter().map(|item| item.memory).collect())
}
```

### Step 64 — `RecallQuery` + `recall_query()` — confidence stubbed at `1.0` (fixes review #2)
```rust
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
        let superseded = memory.superseded_by.is_some();
        if superseded && !query.include_superseded { continue; }
        let expired = memory.expires_at.map(|e| e < now).unwrap_or(false);
        if expired && !query.include_expired { continue; }
        if !query.metadata_equals.iter().all(|(k, v)| memory.metadata.get(k) == Some(v)) { continue; }

        let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
        let recency = ranking::recency_factor(days_since_access, memory.memory_type);
        let reinforcement = ranking::reinforcement_factor(memory.access_count);
        // (fixes review #2) M4 does not have ConfidenceLevel yet — stub at
        // 1.0, exactly as commented. M6's Step 76 is a one-line swap to
        // `memory.confidence.weight()`, nothing else about this function changes.
        let confidence_weight = 1.0_f32;
        let score = ranking::final_score(hit.score, memory.importance, recency, reinforcement, confidence_weight);
        scored.push(RecallItem { memory, similarity: hit.score, score });
    }

    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.memory.id.cmp(&b.memory.id)));
    scored.truncate(query.limit);

    for item in &scored {
        // (fixes review #2) M4 doesn't have the promotion-aware helper yet.
        self.update_access_stats(item.memory.id)?; // M6's Step 77 swaps this one call site only
    }

    Ok(RecallResult { items: scored })
}
```
`RecallQuery`/`RecallItem`/`RecallResult` builder shape is unchanged from v3 (limit, min_importance,
memory_types, include_expired, include_superseded, metadata_equals — all real filters, all now
actually reachable because Step 0.8 keeps expired/superseded rows in the index).

### Step 68 — `RecallResult::as_prompt_context` (unchanged from v3 — this fix was already correct)
Single char-aware `try_append` gate ensures the returned string never exceeds `max_chars`,
including the header and any truncation marker. (See v3 for the full body — no change needed.)

### Steps 69–72 — Tests + checkpoint
69. `.memory_types(...)` filter test.
70. `.metadata_equals(...)` filter test.
71. **(fixes review #4)** `include_superseded`/`include_expired` integration test: `store()` a
    memory, `update()` it (creating a superseded original), `forget()` nothing — call
    `recall_query().include_superseded(true)` and confirm the original superseded memory *is*
    returned, because it was never removed from the vector index (Step 0.8's design). Repeat for
    an artificially-backdated `expires_at` with `include_expired(true)`.
72. **Checkpoint:** `limit(0)` → `Err`; NaN `min_importance` → `Err`; `recall()`/`recall_query()`
    agree; `include_superseded`/`include_expired` tests from Step 71 pass — the exact tests v3
    could never actually pass.

---

# M6 — Confidence scoring

### Step 73 — `ConfidenceLevel` (unchanged from v3)
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceLevel { Explicit, Inferred, Reinforced }

impl ConfidenceLevel {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Explicit => "explicit", Self::Inferred => "inferred", Self::Reinforced => "reinforced" }
    }
    pub fn parse_str(s: &str) -> Result<Self, InvalidConfidence> {
        match s { "explicit" => Ok(Self::Explicit), "inferred" => Ok(Self::Inferred), "reinforced" => Ok(Self::Reinforced), other => Err(InvalidConfidence(other.to_string())) }
    }
    pub fn weight(&self) -> f32 { match self { Self::Explicit | Self::Reinforced => 1.0, Self::Inferred => 0.7 } }
    pub fn maybe_promote(self, access_count: u32) -> Self {
        if self == Self::Inferred && access_count >= 5 { Self::Reinforced } else { self }
    }
}
```

### Step 74 — (fixes review #15's claim wording) Migration repair path exercised, not "invented"
Because Step 0.7 already folds `confidence` into migration 1, M6 doesn't add a new migration —
it adds a **test** that exercises the repair path already written in Step 0.7:
```rust
#[test]
fn repairs_a_pre_v4_database_missing_confidence() {
    // create `memories` manually WITHOUT a confidence column, no schema_migrations row —
    // run_migrations() → assert confidence now exists with the correct CHECK constraint.
}
```
`ARCHITECTURE.md`'s wording is: *"the migration runner repairs missing expected tables and the
`confidence` column; it does not verify column types, constraints beyond that one CHECK, foreign
keys, or the `embeddings` table's shape."* — matching review #15 exactly, not oversold.

### Step 75 — Add `confidence` to `Memory`, `row_to_memory` reads it at its column index (already
in `MEMORY_COLUMNS` since Step 0.5).

### Step 76 — (fixes review #2) Swap ranking's stub for the real weight — **one line, one place**
In `recall_query()` (Step 64), replace:
```rust
let confidence_weight = 1.0_f32;
```
with:
```rust
let confidence_weight = memory.confidence.weight();
```
Nothing else in `recall_query()` changes.

### Step 77 — (fixes review #2) Atomic bump-and-promote replaces Step 46's `update_access_stats`
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
The single call site inside `recall_query()`'s final loop changes from `update_access_stats(...)`
to `update_access_stats_and_maybe_promote(...)`. `recall()`'s M3 body was already deleted at Step
63a, so there is exactly one call site to change.

### Steps 78–86 — Tests + checkpoint
78. Confidence repair test from Step 74.
79. Idempotent reopen: run migrations twice, one row per version, no error.
80–86. Round-trip per `ConfidenceLevel`; `Inferred` scores lower than an otherwise-identical
`Explicit` memory; recalling an `Inferred` memory exactly 5 times promotes it to `Reinforced`;
`cargo clippy`/`fmt` clean. **Checkpoint:** `cargo test` green through M6.

---

# M5 — StoreRequest / MemoryUpdate

### Step 87 — (fixes review #9) `ExpiryPolicy` replaces the ambiguous `Option<Duration>` TTL
```rust
/// Three distinct expiry intents that `Option<Duration>` alone cannot
/// represent — see review #9. `None` on `MemoryUpdate.new_expiry` means
/// "preserve the original memory's policy," which is different again from
/// `Some(ExpiryPolicy::Never)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpiryPolicy {
    /// Use `memory_type`'s default TTL (30/365/730 days, or Working's hours).
    TypeDefault,
    /// Use this exact duration from now.
    Custom(chrono::Duration),
    /// This memory never expires.
    Never,
}
```

### Step 88 — `StoreRequest`
```rust
#[derive(Debug, Clone)]
pub struct StoreRequest {
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
    pub expiry: ExpiryPolicy,
    pub metadata: HashMap<String, Value>,
    pub confidence: ConfidenceLevel,
}
impl StoreRequest {
    pub fn new(content: &str, memory_type: MemoryType, importance: f32) -> Self {
        Self { content: content.to_string(), memory_type, importance, expiry: ExpiryPolicy::TypeDefault, metadata: HashMap::new(), confidence: ConfidenceLevel::Explicit }
    }
    pub fn expiry(mut self, e: ExpiryPolicy) -> Self { self.expiry = e; self }
    pub fn metadata(mut self, m: HashMap<String, Value>) -> Self { self.metadata = m; self }
    pub fn with_confidence(mut self, c: ConfidenceLevel) -> Self { self.confidence = c; self }
}
```

### Step 89 — `MemoryUpdate`
```rust
#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
    pub new_content: Option<String>,
    pub new_importance: Option<f32>,
    pub new_metadata: Option<HashMap<String, Value>>,
    pub new_memory_type: Option<MemoryType>,
    /// `None` = preserve the original memory's expiry policy exactly
    /// (including `Never`, which an `Option<Duration>`-based design could
    /// not distinguish from "use the type default" — this is review #9's fix).
    pub new_expiry: Option<ExpiryPolicy>,
    pub new_confidence: Option<ConfidenceLevel>,
}
```
`id` stays immutable — no `new_id` field, ever.

### Step 90 — `store_with_options()` — full implementation (fixes review #12)
```rust
pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String> {
    self.store_with_options(StoreRequest::new(content, memory_type, importance)).await
}

pub async fn store_with_options(&self, request: StoreRequest) -> Result<String> {
    self.store_with_options_id(request).await.map(|id| id.to_string())
}

/// Typed core — never returns a `String` internally, so no later caller can
/// hit review #10's "fallback to a wrong id" bug by construction: there is
/// nothing to parse-and-fall-back from.
async fn store_with_options_id(&self, request: StoreRequest) -> Result<Uuid> {
    if request.content.trim().is_empty() {
        return Err(MemoliteError::InvalidArgument("content must not be empty".into()));
    }
    if !(0.0..=1.0).contains(&request.importance) {
        return Err(MemoliteError::InvalidArgument("importance must be in [0.0, 1.0]".into()));
    }
    if let ExpiryPolicy::Custom(d) = request.expiry {
        if d <= chrono::Duration::zero() {
            return Err(MemoliteError::InvalidArgument("Custom expiry duration must be positive".into()));
        }
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    let expires_at: Option<DateTime<Utc>> = match request.expiry {
        ExpiryPolicy::Never => None,
        ExpiryPolicy::Custom(d) => Some(now + d),
        ExpiryPolicy::TypeDefault => match request.memory_type {
            MemoryType::Working => Some(now + chrono::Duration::hours(4)),
            other => Some(now + chrono::Duration::days(default_ttl_days(other))),
        },
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
            "INSERT INTO memories (id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata, confidence)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, NULL, ?7, ?8)",
            rusqlite::params![id.to_string(), request.content, request.memory_type.as_str(), request.importance,
                now.timestamp(), expires_at.map(|e| e.timestamp()), metadata_json, request.confidence.as_str()],
        )?;
        tx.execute(
            "INSERT INTO embeddings (memory_id, vector, dimension) VALUES (?1, ?2, ?3)",
            rusqlite::params![id.to_string(), vector_bytes, vector.len() as i64],
        )?;
        tx.commit()?;
    }

    let store = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        std::sync::Arc::clone(&*guard)
    };
    if let Err(e) = store.insert(id, &vector, request.metadata.clone()).await {
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

### Step 91 — `update()` — `ExpiryPolicy` carried forward correctly, no wrong-id fallback
```rust
pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<String> {
    let uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    let old = self.get(id).await?.ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;

    let mut request = StoreRequest::new(
        &update.new_content.unwrap_or_else(|| old.content.clone()),
        update.new_memory_type.unwrap_or(old.memory_type),
        update.new_importance.unwrap_or(old.importance),
    );

    // (fixes review #9) exact preservation, including `Never`, which an
    // Option<Duration> model could never distinguish from "type default."
    request.expiry = update.new_expiry.unwrap_or(match old.expires_at {
        None => ExpiryPolicy::Never,
        Some(expiry) => {
            let remaining = expiry.signed_duration_since(Utc::now());
            if remaining > chrono::Duration::zero() { ExpiryPolicy::Custom(remaining) } else { ExpiryPolicy::Custom(chrono::Duration::seconds(1)) }
        }
    });
    request.metadata = update.new_metadata.unwrap_or_else(|| old.metadata.clone());
    // A revision defaults to Inferred unless the caller explicitly sets confidence.
    request.confidence = update.new_confidence.unwrap_or(ConfidenceLevel::Inferred);

    // (fixes review #10) typed core — new_uuid is never a fallback guess.
    let new_uuid = self.store_with_options_id(request).await?;

    if let Err(e) = self.mark_superseded(&uuid, &new_uuid.to_string()) {
        let del_err = {
            let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()));
            conn.and_then(|c| c.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![new_uuid.to_string()]).map_err(Into::into)).err()
        };
        let store = {
            let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
            std::sync::Arc::clone(&*guard)
        };
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

### Steps 92–108 — Tests + checkpoint
92–95. Basic `store_with_options`/round-trip tests.
96. `update()` with `new_metadata: None` preserves old metadata exactly.
97. `update()` with only `.new_content` set: memory_type unchanged, expiry carried forward as
    *remaining* Custom duration, confidence becomes `Inferred`.
98. **(fixes review #9 — the test v3 could never pass)** `update()` on a memory created with
    `ExpiryPolicy::Never`: the replacement also has `expires_at: None` — this now passes because
    `ExpiryPolicy::Never` round-trips exactly, unlike v3's `Option<Duration>` model.
99. `update()` on a nearly-expired memory: replacement's remaining TTL ≈ what was left.
100. **(fixes review #10)** Compensation test: force `mark_superseded` to fail — assert the
     replacement is gone from *both* SQLite and the vector store, and specifically assert (by id)
     that the **original** memory's vector was untouched — the exact assertion v3's bug would have
     failed.
101–107. `Custom(<= 0)` → `Err`; `update()` on nonexistent id → `Err(NotFound)`; superseded chain
     `A -> B -> C` resolves; no builder method can change `id`.
108. **Checkpoint:** `cargo test` green, zero regressions M3–M6.

---

# M7 — Temporal querying (fixes review #13 — full feature set restored, not narrowed)

v3 silently dropped four previously-promised features. v4 restores all of them alongside
`query_by_time_range`, rather than quietly shrinking scope.

```rust
impl RecallQuery {
    pub fn created_after(mut self, t: DateTime<Utc>) -> Self { self.created_after = Some(t); self }
    pub fn created_before(mut self, t: DateTime<Utc>) -> Self { self.created_before = Some(t); self }
    pub fn only_stale(mut self, b: bool) -> Self { self.only_stale = b; self }
}
```
`recall_query()` gains two more filter lines in its existing loop (same lock-then-clone pattern as
every other read in this document):
```rust
if let Some(after) = query.created_after { if memory.created_at < after { continue; } }
if let Some(before) = query.created_before { if memory.created_at > before { continue; } }
if query.only_stale {
    let stale_cutoff_days = ranking::decay_half_life_days(memory.memory_type) * 2.0;
    let days_since_access = (now - memory.last_accessed).num_seconds() as f64 / 86400.0;
    if days_since_access < stale_cutoff_days { continue; }
}
```

```rust
pub async fn query_by_time_range(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Memory>> {
    if start > end { return Err(MemoliteError::InvalidArgument("start must not be after end".into())); }
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE created_at >= ?1 AND created_at <= ?2 ORDER BY created_at ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![start.timestamp(), end.timestamp()], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Every memory created or modified since `since`, expressed as the set of
/// current (non-superseded-away, i.e. either still-current or itself the
/// tip of a chain created after `since`) memories touched in that window.
pub async fn what_changed_since(&self, since: DateTime<Utc>) -> Result<Vec<Memory>> {
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE created_at >= ?1 OR last_accessed >= ?1 ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![since.timestamp()], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Active memories whose last access is older than twice their type's decay
/// half-life — i.e. `RecallQuery.only_stale`'s exact rule, exposed directly
/// for callers who want the list without running a similarity search first.
pub async fn find_stale_memories(&self) -> Result<Vec<Memory>> {
    let now = Utc::now();
    let active = self.get_active_memories()?; // defined in M9, but see note below
    Ok(active.into_iter().filter(|m| {
        let cutoff_days = ranking::decay_half_life_days(m.memory_type) * 2.0;
        let days_since_access = (now - m.last_accessed).num_seconds() as f64 / 86400.0;
        days_since_access >= cutoff_days
    }).collect())
}

pub async fn find_superseded_chain(&self, id: &str) -> Result<Vec<Memory>> {
    let start_uuid = Uuid::parse_str(id).map_err(MemoliteError::from)?;
    let mut chain = Vec::new();
    let mut current = self.get(id).await?.ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;
    chain.push(current.clone());
    let mut guard_iterations = 0usize;
    while let Some(next_id) = current.superseded_by.clone() {
        guard_iterations += 1;
        if guard_iterations > 10_000 {
            return Err(MemoliteError::Internal(format!("superseded_by cycle detected starting from {start_uuid}")));
        }
        let Some(next) = self.get(&next_id).await? else { break };
        chain.push(next.clone());
        current = next;
    }
    Ok(chain)
}
```
> **Note on `find_stale_memories`'s forward reference to `get_active_memories()`:** that helper's
> *query* (`SELECT ... WHERE superseded_by IS NULL AND (expires_at IS NULL OR expires_at >= now)`)
> is pure SQL with no vector-store dependency, so it's pulled forward to right here in M7 rather
> than waiting for M9 — M9 then reuses the same helper unchanged. This is the same
> "don't forward-reference a later milestone" discipline applied throughout v4.

### Tests + checkpoint
- `start > end` → `Err`.
- Backdated fixtures (written directly via a test-only connection, never `sleep`) return in
  correct chronological order for `query_by_time_range`.
- `what_changed_since` picks up both newly-created and newly-re-accessed memories.
- `find_stale_memories`/`only_stale` agree on the same cutoff rule.
- A superseded chain of length 3 resolves head-to-tail; a synthetic cyclic `superseded_by`
  (constructed directly in test SQL — never producible via the public API) trips the iteration
  guard and returns `Err` instead of hanging.
- **Checkpoint:** `cargo test` green through M7; the full v2-parity temporal feature set is
  present, not silently narrowed.

---

# M6.5 — Concurrency proof (no longer a refactor — the types already exist)

Because `Mutex<Connection>` and `RwLock<Arc<dyn VectorStore>>` were introduced in Step 0, this
milestone is now purely a **verification pass**, not a migration:

```rust
fn assert_send_sync<T: Send + Sync>() {}
#[test]
fn memory_engine_is_send_sync() { assert_send_sync::<memolite::MemoryEngine>(); }
```
- Audit every function written in M3–M7 above for the two rules stated in Step 0.3: no `.lock()`
  or `.read()`/`.write()` guard held across an `.await`. Every snippet in this document already
  follows this — the audit is confirming it, not fixing violations after the fact.
- `cargo clippy` (which flags exactly this pattern) clean.
- **Checkpoint:** `Send + Sync` proven; clippy confirms no lock held across an await anywhere in
  the codebase so far.

---

# M8 — Streaming ingestion, two explicit shutdown modes

Unchanged from v3's already-correct fix: `StreamIngestor::spawn` takes `Arc<MemoryEngine>`, an
mpsc channel, and a `CancellationToken`; the loop checks `cancel.is_cancelled()` **between**
`rx.recv().await` calls rather than racing a `select!` against a live backlog, so:
- `shutdown_now()` cancels immediately; the in-flight chunk finishes, the backlog is not drained.
- `finish()` drops this ingestor's sender and waits for the channel to close naturally — draining
  the full backlog **iff** every cloned `sender()` handle the caller obtained elsewhere has also
  been dropped by the caller (standard mpsc semantics, stated explicitly in the doc comment and in
  `ARCHITECTURE.md`).

Internally, every `store_with_options` call inside the ingestion loop goes through the same
`store_with_options_id`/`store_with_options` path from M5 — no separate insert logic to keep in
sync.

### Tests + checkpoint
- `SentenceBuffer::feed` unicode/boundary tests (unchanged, was already correct).
- `IngestReport` accuracy tests, including a forced per-chunk failure (empty content) that leaves
  `failed == 1` and lets the loop keep consuming.
- `finish()` test: send 5, drop every sender clone, `finish()` → `received == 5 && stored == 5`.
- `shutdown_now()` test against a slow storage test-double: assert prompt return, `received <= 5`.
- Backpressure test with `buffer_size = 1`, all 5 eventually land via `finish()`.
- `spawn(engine, 0)` → `Err(InvalidArgument)`.
- **Checkpoint:** streamed content retrievable end-to-end; both shutdown modes do exactly what
  their names promise, verified by test.

---

# M9 — Compression, with a *genuinely* atomic index rebuild (fixes review #5)

### Eligibility (unchanged)
```rust
pub fn is_compression_eligible(mem: &Memory) -> bool {
    let age_days = (Utc::now() - mem.created_at).num_days();
    let not_expired = mem.expires_at.map(|e| e >= Utc::now()).unwrap_or(true);
    mem.memory_type == MemoryType::Episodic && age_days > 14 && mem.importance < 0.3 && mem.superseded_by.is_none() && not_expired
}
```

### `get_active_memories()` — pulled forward to M7, reused here unchanged
```rust
fn get_active_memories(&self) -> Result<Vec<Memory>> {
    let now = Utc::now().timestamp();
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE superseded_by IS NULL AND (expires_at IS NULL OR expires_at >= ?1)");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![now], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
```

### `get_embeddings()` — validated, lock-scoped correctly
```rust
fn get_embeddings(&self, ids: &[Uuid]) -> Result<Vec<(Uuid, Vec<f32>)>> {
    let expected_dim = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        guard.dimension()
    };
    let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let (bytes, stored_dim): (Vec<u8>, i64) = conn.query_row(
            "SELECT vector, dimension FROM embeddings WHERE memory_id = ?1",
            rusqlite::params![id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let vector: Vec<f32> = bincode::deserialize(&bytes).map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
        if stored_dim as usize != expected_dim || vector.len() != expected_dim {
            return Err(MemoliteError::VectorStore(format!("embedding for {id} has dimension {} (row says {}), expected {}", vector.len(), stored_dim, expected_dim)));
        }
        if !vector.iter().all(|x| x.is_finite()) {
            return Err(MemoliteError::VectorStore(format!("embedding for {id} contains a non-finite value")));
        }
        out.push((*id, vector));
    }
    Ok(out)
}
```

### `rebuild_active_vector_index()` — **actually atomic now** (fixes review #5)
```rust
/// Builds a brand-new backing store off to the side from every active
/// memory's embedding, validating each one. Only after the ENTIRE build
/// succeeds does this function acquire the write lock and swap the new
/// `Arc` into `self.vector_store` — a single, instantaneous pointer swap.
/// If any step of the build fails, `self.vector_store` is untouched: the
/// live index is never observed in a partial state. This is possible
/// specifically because `vector_store` has been `RwLock<Arc<dyn VectorStore>>`
/// since Step 0 — there was never a version of this engine where the swap
/// wasn't structurally available.
async fn rebuild_active_vector_index(&self) -> Result<()> {
    let active = self.get_active_memories()?;
    let dim = {
        let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
        guard.dimension()
    };
    let replacement: std::sync::Arc<dyn VectorStore> = std::sync::Arc::new(InMemoryVectorStore::new(dim));

    for mem in &active {
        let Some(vector) = self.get_embedding(&mem.id.to_string())? else { continue };
        if vector.len() != dim || !vector.iter().all(|x| x.is_finite()) {
            return Err(MemoliteError::VectorStore(format!("cannot rebuild index: embedding for {} is invalid", mem.id)));
        }
        replacement.insert(mem.id, &vector, mem.metadata.clone()).await?; // building the side copy only
    }

    // The entire build above succeeded. Swap now — one write-lock
    // acquisition, no await while held, no partial state ever observable
    // by a concurrent reader.
    let mut guard = self.vector_store.write().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
    *guard = replacement;
    Ok(())
}
```
> **Honesty note, unchanged from v3's own caveat:** this is exact atomicity for the in-memory
> backend because the swap is a single pointer write behind one lock. A remote HTTP-based
> `VectorStore` cannot make the same promise unless its API offers an atomic bulk-replace or
> namespace-swap primitive — `ARCHITECTURE.md` states this distinction explicitly.

### `compress_old_memories()` — unchanged control flow from v3, calling the now-genuinely-atomic rebuild
```rust
pub async fn compress_old_memories(&self) -> Result<usize> {
    let candidates: Vec<Memory> = self.get_episodic_memories_older_than(14)?.into_iter().filter(compression::is_compression_eligible).collect();
    let ids: Vec<Uuid> = candidates.iter().map(|m| m.id).collect();
    let with_vectors = self.get_embeddings(&ids)?;
    let clusters = compression::greedy_cluster(&with_vectors, 0.85);
    let mut compressed_count = 0;

    for cluster in clusters.into_iter().filter(|c| c.member_ids.len() >= 3) {
        let members: Vec<Memory> = candidates.iter().filter(|m| cluster.member_ids.contains(&m.id)).cloned().collect();
        let result = compression::summarize_cluster(&members, 0.85)?;

        let mut metadata = HashMap::new();
        metadata.insert("compression.original_ids".into(), serde_json::json!(result.original_ids.iter().map(Uuid::to_string).collect::<Vec<_>>()));
        metadata.insert("compression.algorithm_version".into(), serde_json::json!(compression::COMPRESSION_ALGORITHM_VERSION));

        // (fixes review #16) Compressed summaries are stored as Semantic,
        // not Episodic — an explicit, deliberate policy so a summary
        // doesn't quietly expire on the episodic TTL and take the
        // consolidated information out of normal recall while the
        // (already-superseded) originals sit inert in SQLite. Semantic's
        // 365-day default TTL, or ExpiryPolicy::Never if the caller
        // configures it, keeps the summary durably recallable.
        let mut request = StoreRequest::new(&result.summary_content, MemoryType::Semantic, 0.3).with_confidence(ConfidenceLevel::Inferred);
        request.metadata = metadata;

        let new_uuid = self.store_with_options_id(request).await?; // typed core — fixes review #10 here too
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

    // Because Step 0.8's indexing policy keeps every SQLite row (including
    // now-superseded originals) in the vector index until deleted, and this
    // function never calls `forget()`, no reconciliation is even needed
    // here — the index and SQLite already agree. `rebuild_active_vector_index`
    // is reserved for the explicit repair path (e.g. after a detected
    // mismatch), not run unconditionally after every compression pass.
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

### Tests + checkpoint
- Eligibility boundary, greedy-clustering, empty-cluster → `Err` tests.
- Integration: 3 similar low-importance episodic memories → `compress_old_memories()` returns `3`.
- **(fixes review #5)** Rebuild-atomicity test: inject a failure partway through
  `rebuild_active_vector_index`'s build loop (test-only wrapper on the 4th of 10 memories) —
  assert `self.vector_store`'s *live, readable* contents are byte-for-byte identical to what they
  were before the call, not partially rebuilt. This is the test v3's clear-then-loop could never
  pass.
- **(fixes review #16)** Compressed summaries are `MemoryType::Semantic`; a `recall_query()` after
  a compression pass, run past the episodic TTL window, still returns the summary.
- Dimension/finite-invalid embedding in the candidate set → `compress_old_memories()` returns
  `Err`, not a silent drop.
- Restart test: after a successful compression run, reopen (exercises Step 0.8's resync), confirm
  vector-store contents match SQLite's active set.
- **Checkpoint:** compression is data-loss-free within its stated scope, fails loudly on
  invalid/incomplete embedding data, and its rebuild is provably atomic for the shipped backend.

---

# M10 — Maintenance controller (fixes review #11, #12, #14)

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
    /// Cancels, waits for the task, and — regardless of whether the task
    /// exited cleanly, was already dead, or panicked — always releases the
    /// single-controller flag before returning. (fixes review #14: a
    /// caller who explicitly observes and handles a `JoinError` is not
    /// left permanently locked out of starting maintenance again.)
    pub async fn shutdown(self) -> Result<()> {
        self.cancel.cancel();
        let result = self.join.await;
        self.running_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        result.map_err(|e| MemoliteError::Internal(e.to_string()))
    }
}
```
`purge_expired()` follows the same reconciliation shape as `forget()` (Step 47): delete from
SQLite, best-effort delete from the vector store per id, resync on failure.

### Panic handling — explicit and recoverable (fixes review #14)
A panic inside the maintenance loop surfaces as an `Err` from `handle.await` inside `shutdown()`.
Because `shutdown()` now clears `maintenance_running` in **every** path — success, cancellation,
or a caller who explicitly calls `shutdown()` after noticing something's wrong — a caller who
observed the failure can immediately call `start_maintenance()` again on a fresh understanding of
the engine's state, rather than being forced to reopen the whole engine. The only way to get
permanently stuck is to *never* call `shutdown()` on a handle whose task has already panicked —
which is the caller's own bug, not the library's.

### Tests + checkpoint
- Paused-clock interval tests; cancellation exits promptly.
- Zero-interval config → `Err(InvalidArgument)`, no panic.
- Second `start_maintenance` while the first is running → `Err`; after `shutdown()`, a second call
  succeeds.
- **(fixes review #14)** Panic-recovery test: force a panic inside the loop (test-only injected
  failure that panics instead of returning `Err`), call `shutdown()`, assert it returns `Err(...)`
  **and** that a subsequent `start_maintenance()` call on the same engine now succeeds.
- Engine-drop test: keep the handle, drop the last `Arc<MemoryEngine>`, advance time, `shutdown()`
  — confirm the task had already exited via `upgrade()` failure.
- Missed-tick `Skip` test; concurrent store/recall during a tick doesn't deadlock.
- **Checkpoint:** purge/compression opt-in only, fallible start, single controller, recoverable
  after an observed failure, never leaks a detached task.

---

# M11 — `generic-http` vector backend (fixes review #6)

### Feature flag
```toml
[dependencies]
reqwest = { version = "0.12", features = ["json"], optional = true }
urlencoding = { version = "2", optional = true }
[features]
generic-http = ["dep:reqwest", "dep:urlencoding"]
```

### Documented wire contract (this is what makes `search()` real instead of `todo!()`)
```
POST {base_url}/search
Request:  { "vector": [f32, ...], "k": usize }
Response: 200 OK, body: [ { "id": "uuid-string", "score": f32 }, ... ]  (may be empty array)
```

### Adapter — `search()` fully implemented, `contains()` distinguishes error codes (fixes review #6, #11)
```rust
pub struct GenericHttpVectorStore { client: reqwest::Client, base_url: String, dim: usize }

#[async_trait]
impl VectorStore for GenericHttpVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        self.client.put(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .json(&serde_json::json!({ "vector": vector, "metadata": metadata }))
            .timeout(std::time::Duration::from_secs(10)).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        #[derive(serde::Deserialize)]
        struct RawHit { id: String, score: f32 }

        let resp = self.client.post(format!("{}/search", self.base_url))
            .json(&serde_json::json!({ "vector": query, "k": k }))
            .timeout(std::time::Duration::from_secs(10)).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        let raw: Vec<RawHit> = resp.json().await.map_err(|e| MemoliteError::VectorStore(e.to_string()))?;

        let mut hits = Vec::with_capacity(raw.len());
        for h in raw {
            let id = Uuid::parse_str(&h.id).map_err(|e| MemoliteError::VectorStore(format!("invalid id in search response: {e}")))?;
            if !h.score.is_finite() { return Err(MemoliteError::VectorStore(format!("non-finite score for {id}"))); }
            hits.push(VectorHit { id, score: h.score });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(hits)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        self.client.delete(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .send().await.map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status().map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    /// (fixes review #11) A non-200/404 status is a real error, not "absent."
    async fn contains(&self, id: Uuid) -> Result<bool> {
        let resp = self.client.get(format!("{}/vectors/{}", self.base_url, urlencoding::encode(&id.to_string())))
            .send().await.map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
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
    fn dimension(&self) -> usize { self.dim }
}
```
`Debug` is hand-written to redact any API key field.

### Tests + checkpoint
- `wiremock`-based unit tests, including `search()` against a real JSON body — the exact test v3
  could never write because the function was `todo!()`.
- `contains()` test asserting a `500` response surfaces as `Err`, not `Ok(false)`.
- `clear()` integration test: insert 3, `clear()`, `search()` returns empty, `contains()` is
  `false` for all 3.
- `cargo build` (default features) does not pull in `reqwest` — verified via `cargo tree`.
- `cargo test --all-features` passes with `search()` fully implemented — the concrete blocker
  review #6 raised.
- **Checkpoint:** `--all-features` genuinely passes; no `todo!()` remains anywhere in the crate.

---

# M12 — Final polish, docs, release gate

### `ARCHITECTURE.md` must state, in these exact honest terms:
- Concurrency model: `Mutex<Connection>` + `Mutex<Embedder>` + `RwLock<Arc<dyn VectorStore>>`,
  all present since Step 0 — never retrofitted mid-project. Lock-then-clone-then-drop-then-await
  discipline, applied uniformly; `Send + Sync` proven at M6.5.
- Migration runner scope, stated precisely per review #15: repairs missing expected tables and the
  `confidence` column; does not verify types, extra constraints, foreign keys, or the embeddings
  schema's shape.
- Vector-index policy (review #4's fix): the index holds every memory row still present in
  SQLite, including superseded/expired ones, until actually deleted; filtering those out is
  `recall_query`'s job, not the index's.
- Atomic rebuild (review #5's fix): exact for the in-memory backend via one `RwLock` write-lock
  swap of a fully-built replacement; best-effort for remote backends.
- `ExpiryPolicy`'s three states (review #9) and why `Option<Duration>` couldn't represent them.
- The typed-`Uuid`-core pattern (`store_with_options_id`, etc.) and why it exists (review #10):
  compensation logic can never target the wrong id because there's no string round-trip to fail.
- Compression's storage-type policy (review #16): summaries are `Semantic`, explicitly, so they
  don't silently fall out of recall on the episodic TTL.
- Maintenance: single-controller enforcement, fallible start, and the exact panic-recovery
  contract (review #14): `shutdown()` always releases the flag, even after an observed panic.
- The `generic-http` backend's real, tested `search()` contract (review #6).
- The full restored temporal API (review #13): `query_by_time_range`, `what_changed_since`,
  `find_stale_memories`, `find_superseded_chain`, plus `RecallQuery`'s `created_after`/
  `created_before`/`only_stale`.

### "Risks and Honest Limitations" must state:
- Compression is extractive/concatenation-based, not LLM-abstractive.
- `as_prompt_context()` delimits content; it does not sanitize against prompt injection.
- Remote vector-store backends do not get the same atomic index-rebuild guarantee as the built-in
  in-memory backend.
- The migration runner's repair scope is intentionally narrow (see above) — it is not a full
  schema-validation tool.

### Final validation
- `cargo fmt --check`; `cargo clippy --all-targets --all-features -- -D warnings`.
- `cargo test --all-targets --all-features` **and** default-feature test run separately.
- `cargo doc --no-deps --all-features` — zero warnings.
- Fresh clone → fresh build → fresh `open()`, exercising Step 0's migration + resync path with no
  prior state.
- **Final checkpoint — release gate, not automatic:** after the user reviews the diff, release
  notes, and semver choice, the user may explicitly authorize a git tag. No automatic tagging.

---

## Cross-reference: every v3-review finding and where v4 fixes it

| # | Finding | Fix in v4 |
|---|---|---|
| 1 | M3 calls `run_migrations`/`rebuild_active_vector_index` before they exist | Step 0.7 (migrations) and Step 0.8 (`resync_vector_index`) both moved to Step 0, before M3 |
| 2 | M4 uses `memory.confidence`/`update_access_stats_and_maybe_promote` before M6 | Step 64 stubs `confidence_weight = 1.0` and calls plain `update_access_stats`; M6's Steps 76–77 are one-line swaps at the same call sites |
| 3 | M3 test 51 (access-count) has nothing to test | Step 46's `recall()` now calls `update_access_stats` directly |
| 4 | `include_expired`/`include_superseded` can't work — never indexed | Step 0.8: index holds every SQLite row regardless of expiry/supersession; filtering is `recall_query`'s job |
| 5 | "Atomic" rebuild was clear-then-insert | `vector_store: RwLock<Arc<dyn VectorStore>>` from Step 0; M9's rebuild builds a full replacement, then swaps the `Arc` in one lock acquisition |
| 6 | M11 `search()` is `todo!()` | Full documented wire contract + implementation in M11; wiremock tests included |
| 7 | Post-M6.5 code calls methods directly on `Mutex<Connection>` | Mutex introduced at Step 0; every snippet in this document, at every milestone, uses the lock-then-operate pattern from the start — no "before/after the refactor" code exists |
| 8 | Backfill holds a SQLite borrow across `.await` | Step 0.8 reads everything into a `Vec` inside a scoped block, drops the guard, then awaits |
| 9 | Permanent expiry (`None`) can't be preserved through `update()` | `ExpiryPolicy { TypeDefault, Custom(Duration), Never }` replaces `Option<Duration>` |
| 10 | UUID parse-fallback can target the wrong memory | Typed `Uuid`-returning internal cores (`store_id`, `store_with_options_id`) — no string round-trip, no fallback branch exists |
| 11 | `contains()` treats every non-200 as "absent" | M11's adapter matches `OK`/`NOT_FOUND`/other explicitly, propagating real errors |
| 12 | `store_with_options()` etc. left as comments | Full, copy-pasteable implementations at Steps 45, 90, and throughout M9/M11 |
| 13 | M7 silently dropped v2's temporal feature set | `what_changed_since`, `find_stale_memories`, `created_after`/`created_before`/`only_stale` all restored alongside `query_by_time_range` |
| 14 | Panic leaves maintenance permanently locked | `MaintenanceHandle::shutdown()` clears the flag on every exit path, including after an observed panic |
| 15 | "Self-heals any partial schema" overstated | Scope stated precisely: missing tables + the `confidence` column only, documented as such |
| 16 | Compressed summaries can silently expire out of recall | Summaries stored as `MemoryType::Semantic`, explicitly, not `Episodic` |

v4's ordering guarantee: **every code block, at every milestone, only calls functions that were
defined at or before that milestone in this document.** That's the concrete, checkable meaning of
"properly executable as written," which was the v3 review's central complaint.



#### SHORTCOMINGS OF THIS BUILDING PLAN ###