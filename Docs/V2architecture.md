# Memolite — Final Master Build Plan (Steps 41 onward, patched)

> **Rule zero, unchanged:** everything built in Steps 1–40 stays exactly as it is, except the
> specific "Step-40 repair" items called out below (foreign keys, validation, embed-before-write,
> atomic SQLite rows). This document is the single source of truth from here on — it replaces
> both the original milestone plan *and* the loose "Replacement" patch notes that were floating
> on top of it. Project name is **Memolite**, crate root is `memolite/`, and the error type is
> **`MemoliteError`** everywhere (not `ContextMemoryError`, not `ContextMemory`).

Everything named in the README gets built: temporal querying, streaming ingestion, memory
compression, confidence scoring, plus the two public-API pieces the old plan quietly dropped —
`StoreRequest`/`store_with_options()` and `MemoryUpdate`/`update()`. Nothing here is optional
unless marked `(OPTIONAL)`.

---

## Corrected build order (follow this, not the milestone numbers)

The milestone *labels* (M3, M4, M5...) are kept for traceability against earlier discussion, but
they are **not** built in numeric order. Confidence (M6) must exist before requests/updates (M5)
or you'll have a temporary non-compiling state where `StoreRequest` references a type that
doesn't exist yet.

| Build order | Milestone | What it adds |
|---|---|---|
| 0 | Step-40 repair | foreign keys, validation, embed-before-write, atomic SQLite rows |
| 1 | M3 | `VectorStore` trait, in-memory store, backfill, recall, synchronized delete/purge |
| 2 | M4 | ranked query/result API (`RecallQuery`, `RecallItem`, `RecallResult`), filters |
| 3 | **M6** | `ConfidenceLevel` enum + real migration runner/infrastructure |
| 4 | **M5** | `StoreRequest`/`MemoryUpdate` API, using confidence from day one |
| 5 | M7 | temporal querying (`what_changed_since`, staleness) |
| 6 | Concurrency refactor | `Mutex<Connection>`, `Send + Sync` gate — done *before* spawning anything |
| 7 | M8 | streaming ingestion with observable errors and explicit shutdown |
| 8 | M9 | compression with provenance, bounded summaries, index recovery |
| 9 | M10 | explicit maintenance controller with cancellation |
| 10 | M11 | optional VecLite backend — only after the real protocol is pinned |
| 11 | M12 | docs, compatibility, packaging, benchmarks, user-authorized release |

This order keeps every intermediate commit compiling and prevents later concurrency/migration
work from forcing a rewrite of the public API.

---

## Step-40 repair (do this first, before M3)

Before adding anything new, fix these in the existing Steps 1–40 code so the rest of the plan has
a sound foundation to build on:

- Enforce foreign keys (`PRAGMA foreign_keys = ON` on every connection open).
- Add validation at the boundary of every existing public method (reject empty content, non-finite
  floats, non-positive TTLs) instead of trusting callers.
- Embed-before-write: compute the embedding vector *before* opening the SQLite write transaction,
  so a slow/failing embedder never leaves a half-committed row.
- Make every existing multi-statement write (row + embedding blob) atomic — wrap in a single
  SQLite transaction, not sequential `execute()` calls that can leave partial state on failure.

Checkpoint: `cargo test` still green after the repair, with new tests covering each fix above.

---

# M3 — VectorStore trait + naive search (Steps 41–55)

**Goal:** `recall("some query")` returns *something* real, using pure cosine similarity. No ranking
formula yet — that's M4.

### Step 41 — Create the vector_store module folder
**File:** `src/vector_store/mod.rs` (new), **also create** `src/vector_store/in_memory.rs` (empty
for now, filled in Step 42).

```rust
// src/vector_store/mod.rs
use async_trait::async_trait;
use std::collections::HashMap;
use serde_json::Value;
use crate::error::Result;

pub mod in_memory;

#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: String,
    pub score: f32,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn insert(&self, id: &str, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: &str) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

In `src/lib.rs`:
```rust
pub mod vector_store;
pub use vector_store::{VectorStore, VectorHit};
```

### Step 42 — Implement `InMemoryVectorStore`
**File:** `src/vector_store/in_memory.rs` (new). Thread-safe `HashMap` behind `RwLock` — reads
(searches) are far more common than writes (inserts), so `RwLock` lets many reads run concurrently.

```rust
use std::collections::HashMap;
use std::sync::RwLock;
use async_trait::async_trait;
use serde_json::Value;
use crate::error::Result;
use super::{VectorStore, VectorHit};
use crate::math_utils::cosine_similarity; // shared helper — see M9 note below

pub struct InMemoryVectorStore {
    data: RwLock<HashMap<String, (Vec<f32>, HashMap<String, Value>)>>,
    dim: usize,
}

impl InMemoryVectorStore {
    pub fn new(dim: usize) -> Self {
        Self { data: RwLock::new(HashMap::new()), dim }
    }
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn insert(&self, id: &str, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        let mut guard = self.data.write().unwrap();
        guard.insert(id.to_string(), (vector.to_vec(), metadata));
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        let guard = self.data.read().unwrap();
        let mut scored: Vec<VectorHit> = guard
            .iter()
            .map(|(id, (vec, _))| VectorHit { id: id.clone(), score: cosine_similarity(query, vec) })
            .collect();
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        scored.truncate(k);
        Ok(scored)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let mut guard = self.data.write().unwrap();
        guard.remove(id);
        Ok(())
    }

    fn dimension(&self) -> usize { self.dim }
}
```

> **Fix folded in from patch:** create `src/math_utils.rs` *now*, with one tested
> `cosine_similarity(a: &[f32], b: &[f32]) -> f32` function, and import it everywhere instead of
> redefining it later in `ranking.rs` and `compression.rs`. This avoids the duplicate-helper drift
> the original plan introduced.

### Step 43 — Unit test `InMemoryVectorStore` in isolation
**File:** `src/vector_store/in_memory.rs` (edit — append).
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nearest_vector_comes_back_first() {
        let store = InMemoryVectorStore::new(3);
        store.insert("far", &[0.0, 1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert("close", &[1.0, 0.0, 0.0], HashMap::new()).await.unwrap();
        let hits = store.search(&[0.9, 0.1, 0.0], 1).await.unwrap();
        assert_eq!(hits[0].id, "close");
    }
}
```
Run `cargo test in_memory` — green before continuing.

### Step 44 — Wire `MemoryEngine` to own a `Box<dyn VectorStore>`
**File:** `src/engine.rs` (edit).
```rust
pub struct MemoryEngine {
    db: Db,
    embedder: Embedder,
    vector_store: Box<dyn VectorStore>,
    // ... fields from Steps 1-40
}
```

### Step 45 — On `store()`, also insert into the vector store
**File:** `src/engine.rs`. This is the "atomic Step 45 pipeline" that `store_with_options` in M5
reuses: SQLite row + embedding blob + vector-store insert must all succeed or the whole store()
call fails and nothing partial is left behind.
```rust
self.vector_store.insert(&memory.id, &vector, HashMap::new()).await?;
```

### Step 46 — Implement real `recall(query: &str)`
```rust
pub async fn recall(&self, query_text: &str) -> Result<Vec<Memory>> {
    let query_vec = self.embedder.embed(query_text)?;
    let hits = self.vector_store.search(&query_vec, 20).await?; // candidate pool
    let mut results = Vec::new();
    for hit in hits {
        if let Some(mem) = self.get(&hit.id).await? {
            results.push(mem);
        }
    }
    Ok(results)
}
```

### Step 47 — Handle the empty-store case
Guard at the top of `recall()`: if `hits.is_empty()`, return `Ok(Vec::new())`.

### Step 48 — Bump `access_count` / `last_accessed` on recall
**File:** `src/db/queries.rs` (new function).
```rust
pub fn update_access_stats(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
        params![chrono::Utc::now().timestamp(), id],
    )?;
    Ok(())
}
```
Call for every memory returned by `recall()`, **before** returning it. Note: in M4/M6 this call
site moves inside the scored-ranking loop, and stats are only updated for the *final, truncated*
set of returned items — not every candidate that was scored and discarded.

### Steps 49–55 — Integration tests + checkpoint
49. `tests/recall_test.rs`: store 3 unrelated facts + 1 relevant one, `recall()`, assert the
    relevant one is present.
50. `recall()` on an empty engine returns `Ok(vec![])`, not an error.
51. `access_count` increases by exactly 1 after one `recall()` call that returns a given memory.
52. `delete()`-ing a memory from SQLite also removes it from `vector_store` (edit `forget()` to
    call `self.vector_store.delete(id)` too).
53. `cargo clippy` clean.
54. `cargo fmt`.
55. **Checkpoint:** `cargo test` all green. You can store 5 facts and recall the right one by
    meaning, not exact words.

---

# M4 — The ranking formula + corrected recall API (Steps 56–70)

**Goal:** replace raw cosine ranking with the four-factor formula, and ship the *corrected*
`RecallQuery` / `RecallItem` / `RecallResult` shapes (fixed from the original single-`memory_type`
version).

### Step 56 — Create the ranking module
**File:** `src/ranking.rs` (new). Uses the shared `math_utils::cosine_similarity`, not a local copy.
```rust
use crate::memory::MemoryType;

pub fn decay_half_life_days(t: MemoryType) -> f64 {
    match t {
        MemoryType::Episodic => 14.0,
        MemoryType::Semantic => 693.0,
        MemoryType::Procedural => 1386.0,
        MemoryType::Working => 0.17, // ~4 hours, hard-expires anyway
    }
}

pub fn recency_factor(days_since_access: f64, memory_type: MemoryType) -> f32 {
    let half_life = decay_half_life_days(memory_type);
    let decay_rate = std::f64::consts::LN_2 / half_life;
    (-decay_rate * days_since_access).exp() as f32
}

pub fn reinforcement_factor(access_count: u32) -> f32 {
    1.0 + ((1.0 + access_count as f32).ln()) * 0.1
}

/// confidence_weight is documented as a multiplier on the combined score (see M6).
pub fn final_score(similarity: f32, importance: f32, recency: f32, reinforcement: f32, confidence_weight: f32) -> f32 {
    similarity * importance * recency * reinforcement * confidence_weight
}
```
> Building M4 before M6 means `final_score` takes `confidence_weight` as a parameter from the
> start (pass `1.0` until M6 lands) — this avoids having to change every call site later.

### Step 57 — Unit test the worked example
**File:** `src/ranking.rs` (edit — append). Same worked example as before, now passing `1.0` as
the confidence weight for both memories (both default to full weight pre-M6).
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_recent_memory_beats_similar_stale_one() {
        let recency_a = recency_factor(30.0, MemoryType::Semantic);
        let reinf_a = reinforcement_factor(5);
        let score_a = final_score(0.82, 0.8, recency_a, reinf_a, 1.0);

        let recency_b = recency_factor(60.0, MemoryType::Episodic);
        let reinf_b = reinforcement_factor(1);
        let score_b = final_score(0.91, 0.5, recency_b, reinf_b, 1.0);

        assert!(score_a > score_b, "A={score_a} should beat B={score_b}");
    }
}
```
If B wins, fix the half-life numbers or exponent sign before continuing.

### Steps 58–63 — Wire ranking into `recall()`
58. Compute `days_since_access`, `recency`, `reinforcement`, `final_score` for each candidate.
59. Sort candidates by `final_score` descending, **then by ID** as a deterministic tiebreaker
    (two equal scores must not reorder between runs).
60. Over-fetch the vector-store candidate pool instead of using a fixed pool of 20: request
    `max(limit * 5, 50)` raw hits (cap and document this constant), *then* apply metadata filters,
    *then* score, *then* truncate to `top_k`. Filtering after a too-small fixed pool silently
    drops valid matches.
61. Return `RecallResult` (Step 68) instead of a bare `Vec<Memory>`.
62. Update `tests/recall_test.rs` to check ordering, not just presence.
63. `cargo test` green before continuing.

### Step 64 — `RecallQuery` (corrected shape)
**File:** `src/recall.rs` (new). Fixes from the patch: `memory_types` is a `Vec` (an "or" filter
across types, not a single type), plus explicit `include_expired`/`include_superseded`/
`metadata_equals` fields so intent is unambiguous at the call site.

```rust
use crate::memory::MemoryType;
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
        self.metadata_equals.insert(key.to_string(), value);
        self
    }
}

/// Ergonomic string recall. Rust cannot overload two methods both named `recall`,
/// so this `From` impl plus `MemoryEngine::recall_text(&self, &str)` (see engine.rs)
/// stand in for "just pass a string" call sites.
impl From<&str> for RecallQuery {
    fn from(s: &str) -> Self { RecallQuery::new(s) }
}
```

On `MemoryEngine` (edit `src/engine.rs`):
```rust
pub async fn recall_text(&self, query: &str) -> Result<RecallResult> {
    self.recall(RecallQuery::new(query)).await
}
```

**Validation, enforced at the top of `recall()`:**
- `limit > 0` — reject `0` with a clear error rather than silently returning nothing.
- `min_importance` and any score thresholds must be finite (`f32::is_finite()`), reject NaN/±inf.
- `query_text` must be non-empty after trimming.

### Steps 65–67 — Apply filters (in the over-fetched pool, before truncation)
65. Drop candidates with `importance < query.min_importance`.
66. Drop candidates whose `memory_type` is not in `query.memory_types` (if `Some`).
67. Drop candidates with `superseded_by.is_some()` unless `include_superseded`, and drop expired
    candidates unless `include_expired`.

### Step 68 — `RecallItem` + `RecallResult` + `as_prompt_context()`
**File:** `src/recall.rs` (edit — append). Corrected shape: each item carries both the raw vector
`similarity` and the final ranked `score`, so callers can distinguish "how semantically close" from
"how it ranked overall."

```rust
use crate::memory::Memory;

pub struct RecallItem {
    pub memory: Memory,
    pub similarity: f32,
    pub score: f32,
}

pub struct RecallResult {
    pub items: Vec<RecallItem>,
}

impl RecallResult {
    /// Bounded, escaped rendering for prompt injection into an LLM context window.
    /// Never concatenates unlimited content — enforces a character budget and
    /// clearly delimits each (untrusted) memory's content.
    pub fn as_prompt_context(&self, max_chars: usize) -> String {
        let mut out = String::from("Relevant memories:\n");
        for (i, item) in self.items.iter().enumerate() {
            let escaped = item.memory.content.replace('\n', " ").replace("---", "- - -");
            let line = format!("{}. [{:.2}] {}\n", i + 1, item.score, escaped);
            if out.len() + line.len() > max_chars {
                out.push_str("...[truncated, budget reached]\n");
                break;
            }
            out.push_str(&line);
        }
        out
    }
}
```

Update `recall()`'s final step: after over-fetching, filtering, scoring, and sorting
(similarity+score+ID tiebreak), truncate to `limit`, call `update_access_stats` (Step 48) **only**
for the final truncated set, and wrap into `RecallResult { items }`.

### Steps 69–70 — Tests + checkpoint
69. `.memory_types(vec![Semantic])` correctly excludes episodic hits; `.memory_types(vec![Semantic, Procedural])` includes both and excludes episodic.
70. **Checkpoint:** run the README's worked example through the *real* `recall()` end-to-end and
    confirm ordering matches. Test `limit(0)` returns an error, not an empty result. Test a NaN
    `min_importance` is rejected. `cargo test` green.

---

# M6 — Confidence scoring (build before M5)

**Goal:** track *how* a memory was learned, weight retrieval by it, and stand up the real
migration runner that M5's `confidence` column depends on.

### Step 71 — Define `ConfidenceLevel`
**File:** `src/confidence.rs` (new).
```rust
use serde::{Serialize, Deserialize};
use std::fmt;
use std::str::FromStr;
use crate::error::MemoliteError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceLevel {
    Explicit,
    Inferred,
    Reinforced,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid confidence value: {0}")]
pub struct InvalidConfidence(pub String);

impl ConfidenceLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfidenceLevel::Explicit => "explicit",
            ConfidenceLevel::Inferred => "inferred",
            ConfidenceLevel::Reinforced => "reinforced",
        }
    }

    pub fn parse_str(s: &str) -> Result<Self, InvalidConfidence> {
        match s {
            "explicit" => Ok(ConfidenceLevel::Explicit),
            "inferred" => Ok(ConfidenceLevel::Inferred),
            "reinforced" => Ok(ConfidenceLevel::Reinforced),
            other => Err(InvalidConfidence(other.to_string())),
        }
    }

    /// Documented multiplier used directly in ranking::final_score.
    /// Inferred = 0.7 (a system guess is trusted less); everything else = 1.0.
    pub fn weight(&self) -> f32 {
        match self {
            ConfidenceLevel::Explicit | ConfidenceLevel::Reinforced => 1.0,
            ConfidenceLevel::Inferred => 0.7,
        }
    }

    /// Promote Inferred -> Reinforced once accessed enough times.
    pub fn maybe_promote(self, access_count: u32) -> Self {
        if self == ConfidenceLevel::Inferred && access_count >= 5 {
            ConfidenceLevel::Reinforced
        } else {
            self
        }
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

### Step 72 — Real migration runner (in `engine.rs`, not `src/db/*`)
This is the corrected version of what the old plan called an "ALTER TABLE with a column check."
Build a genuine, versioned migration runner now, since M5, M7, and M9 all add schema and all
depend on this existing first.

**File:** `src/engine.rs` (edit).
```rust
fn run_migrations(conn: &mut Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )",
        [],
    )?;

    let current_version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations", [], |r| r.get(0),
    )?;

    let migrations: Vec<(i64, &str)> = vec![
        (1, MIGRATION_1_BASELINE),   // represents the existing Steps 1-40 tables
        (2, MIGRATION_2_CONFIDENCE), // adds the confidence column + CHECK constraint
    ];

    for (version, sql) in migrations.into_iter().filter(|(v, _)| *v > current_version) {
        let tx = conn.transaction()?;
        // Inspect pragma_table_info first so an existing pre-M6 database can be safely
        // baselined at migration 1 without re-running CREATE TABLE on tables that exist.
        if version == 1 {
            let has_memories: bool = tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories'",
                [], |r| r.get::<_, i64>(0),
            ).map(|c| c > 0)?;
            if !has_memories {
                tx.execute_batch(sql)?;
            }
        } else {
            tx.execute_batch(sql)?;
        }
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, chrono::Utc::now().timestamp()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

const MIGRATION_2_CONFIDENCE: &str = "
    ALTER TABLE memories ADD COLUMN confidence TEXT NOT NULL DEFAULT 'explicit'
        CHECK(confidence IN ('explicit', 'inferred', 'reinforced'));
    CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
    CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
    CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type);
    CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at);
    CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);
";
```
`MIGRATION_1_BASELINE` is the exact `CREATE TABLE` statements you already have from Steps 1–40 —
copy them in verbatim, don't rewrite them.

### Step 73 — Add `confidence` to `Memory` and the explicit column list
**File:** `src/memory.rs` (edit). Add `pub confidence: ConfidenceLevel`. Update `row_to_memory`
in `src/db/queries.rs` to read/write it via `ConfidenceLevel::parse_str`/`.as_str()`, and add it to
the **explicit** `SELECT`/insert column list (never `SELECT *` — an explicit list survives future
migrations without silently reordering fields).

### Step 74 — Weight confidence into ranking
Every call site of `ranking::final_score` (inside `recall()`) now passes
`item.memory.confidence.weight()` as the real fifth argument instead of the placeholder `1.0`
from M4.

### Step 75 — Confidence promotion, done correctly
Promotion happens **after** the access-count increment from Step 48, and uses the *new* count —
and both the increment and the promotion check/update happen in **one SQL statement or one
transaction**, so there's no window where a read sees a stale count and promotes one access late
(or double-promotes under concurrent access).

**File:** `src/db/queries.rs`.
```rust
pub fn bump_access_and_maybe_promote(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE memories
         SET access_count = access_count + 1,
             last_accessed = ?1,
             confidence = CASE
                 WHEN confidence = 'inferred' AND access_count + 1 >= 5 THEN 'reinforced'
                 ELSE confidence
             END
         WHERE id = ?2",
        params![chrono::Utc::now().timestamp(), id],
    )?;
    Ok(())
}
```
This replaces the separate "bump stats" + "maybe promote" two-step from the original plan, which
was off-by-one-prone.

### Steps 76–83 — Tests + checkpoint
76. Migration test: run `run_migrations` against a **real Steps-1-40 database copy** (not a fresh
    one), confirm it lands at version 2 with the confidence column present and defaulted.
77. Idempotent reopen: run `run_migrations` twice in a row against the same database, confirm no
    error and `schema_migrations` has exactly one row per version.
78. Round-trip: store with each `ConfidenceLevel` variant, `get()` back, confirm equality.
79. An `Inferred` memory scores lower than an otherwise-identical `Explicit` one in `recall()`.
80. Recalling an `Inferred` memory exactly 5 times promotes it to `Reinforced` at **exactly**
    count 5, not 4 or 6 — test the boundary precisely.
81. Test `ConfidenceLevel::parse_str("garbage")` returns `Err(InvalidConfidence(_))`, not a panic.
82. `cargo clippy` / `cargo fmt` clean.
83. **Checkpoint:** confidence is persisted, ranked, promoted correctly, and the migration runner
    is real and tested against a real prior-schema database. `cargo test` green.

---

# M5 — StoreRequest / MemoryUpdate (build after M6)

**Goal:** the two API pieces the README always promised, built now that `ConfidenceLevel` exists
so there's no temporary non-compiling state.

### Step 84 — Define `StoreRequest`
**File:** `src/requests.rs` (new).
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
            custom_ttl: None,   // None means "use the type's default TTL"
            metadata: HashMap::new(),
            confidence: ConfidenceLevel::Explicit,
        }
    }
    pub fn with_ttl(mut self, ttl: chrono::Duration) -> Self { self.custom_ttl = Some(ttl); self }
    pub fn with_metadata(mut self, key: &str, value: Value) -> Self {
        self.metadata.insert(key.to_string(), value);
        self
    }
    pub fn with_confidence(mut self, c: ConfidenceLevel) -> Self { self.confidence = c; self }
}
```
Validation (enforced in `store_with_options`, not here): reject non-positive TTL **unless** the
API explicitly documents immediate expiry as an intentional use case (e.g. a test-only "expire
immediately" helper) — don't silently accept `Duration::zero()` as if it were a normal TTL.

### Step 85 — Define `MemoryUpdate` (corrected — includes type/TTL/confidence overrides)
**File:** `src/requests.rs` (edit — append). The original plan only let you change content and
importance; the corrected version documents *every* field as either changeable or explicitly
immutable, and never silently discards metadata.
```rust
#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
    pub new_content: Option<String>,
    pub new_importance: Option<f32>,
    /// None = leave metadata untouched. Some(map) = replace wholesale.
    /// There is deliberately no "merge" mode in v1 — merging has surprising semantics
    /// when a key is removed vs. never set. Document this plainly.
    pub new_metadata: Option<HashMap<String, Value>>,
    pub new_memory_type: Option<MemoryType>,
    pub new_ttl: Option<chrono::Duration>,
    pub new_confidence: Option<ConfidenceLevel>,
}

impl MemoryUpdate {
    pub fn new() -> Self { Self::default() }
    pub fn content(mut self, c: &str) -> Self { self.new_content = Some(c.to_string()); self }
    pub fn importance(mut self, i: f32) -> Self { self.new_importance = Some(i); self }
    pub fn metadata(mut self, m: HashMap<String, Value>) -> Self { self.new_metadata = Some(m); self }
    pub fn memory_type(mut self, t: MemoryType) -> Self { self.new_memory_type = Some(t); self }
    pub fn ttl(mut self, ttl: chrono::Duration) -> Self { self.new_ttl = Some(ttl); self }
    pub fn confidence(mut self, c: ConfidenceLevel) -> Self { self.new_confidence = Some(c); self }
}
```
`id` itself is documented as immutable — there is no `new_id` field, by design, since an update
that changes identity is a `store()` of a new memory, not an update.

### Step 86 — Register the module
**File:** `src/lib.rs`.
```rust
pub mod requests;
pub use requests::{StoreRequest, MemoryUpdate};
```

### Step 87 — Implement `store_with_options()` using the Step-45 atomic pipeline
**File:** `src/engine.rs`.
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

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let ttl = request.custom_ttl.unwrap_or_else(|| default_ttl(request.memory_type));
    let expires_at = now + ttl;

    // Metadata is serialized exactly once, here, and that same serialized string is what
    // round-trips through both the SQLite metadata column and the vector store's metadata copy.
    let metadata_json = serde_json::to_string(&request.metadata)
        .map_err(|e| MemoliteError::Serialization(e.to_string()))?;

    let vector = self.embedder.embed(&request.content)?; // embed-before-write, per Step-40 repair

    // Single transaction: SQLite row + embedding blob + vector-store insert.
    self.db.insert_memory(
        &id, &request.content, request.memory_type, request.importance,
        now, expires_at, &metadata_json, request.confidence,
    )?;
    self.db.insert_embedding(&id, &vector)?;
    self.vector_store.insert(&id, &vector, request.metadata.clone()).await?;

    Ok(id)
}
```

### Step 88 — `update()` as a logical replacement, with compensation
**File:** `src/engine.rs`. Corrected sequence: fetch original → preserve every field the caller
didn't explicitly change → store the replacement → mark the original superseded → **if marking
superseded fails, delete the replacement from both SQLite and the vector store** so a failed
update never leaves an orphaned, unlinked replacement memory floating in the store.

```rust
pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<String> {
    let old = self.get(id).await?.ok_or_else(|| MemoliteError::NotFound(id.to_string()))?;

    let mut request = StoreRequest::new(
        &update.new_content.unwrap_or(old.content.clone()),
        update.new_memory_type.unwrap_or(old.memory_type),
        update.new_importance.unwrap_or(old.importance),
    );
    request.custom_ttl = update.new_ttl.or(old.custom_ttl);
    request.metadata = update.new_metadata.unwrap_or(old.metadata.clone());
    request.confidence = update.new_confidence.unwrap_or(ConfidenceLevel::Inferred);
    // An update is a revision, not a fresh explicit statement, unless the caller
    // explicitly overrides new_confidence.

    let new_id = self.store_with_options(request).await?;

    if let Err(e) = self.db.mark_superseded(id, &new_id) {
        // Compensate: don't leave an unlinked replacement behind.
        let _ = self.db.delete_memory(&new_id);
        let _ = self.vector_store.delete(&new_id).await;
        return Err(e);
    }

    Ok(new_id)
}
```

### Step 89 — `mark_superseded`
**File:** `src/db/queries.rs`.
```rust
pub fn mark_superseded(conn: &Connection, old_id: &str, new_id: &str) -> Result<()> {
    let affected = conn.execute(
        "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
        params![new_id, old_id],
    )?;
    if affected == 0 {
        return Err(MemoliteError::NotFound(old_id.to_string()));
    }
    Ok(())
}
```

### Steps 90–105 — Tests + checkpoint
90. `store()` still behaves exactly as before (now a thin wrapper over `store_with_options`).
91. Custom TTL of 5s; wait 6s; confirm purge removes it.
92. `custom_ttl: None` uses the memory type's default TTL, not zero.
93. A zero or negative `custom_ttl` is rejected with a clear error.
94. Custom metadata round-trips exactly through `get()`, including nested JSON values.
95. `update()`: old memory's `superseded_by == Some(new_id)`; new memory's `superseded_by == None`.
96. `update()` with `new_metadata: None` preserves the *old* metadata exactly — test this
    explicitly, since silently discarding it here was the original plan's bug.
97. `update()` with only `.content(...)` set leaves memory_type, TTL, and confidence unchanged
    from the original (unless confidence defaults to Inferred as documented).
98. `recall()` with default query excludes the superseded original; `.include_superseded(true)`
    returns both, distinguishable via `superseded_by`.
99. `update()` on a non-existent id returns `Err(MemoliteError::NotFound(_))`, not a panic.
100. **Compensation test:** force `mark_superseded` to fail (e.g. delete the original out from
     under it in the test, or inject a failure), then assert the replacement memory is gone from
     *both* SQLite and the vector store afterward — no orphan left behind.
101. `(OPTIONAL)` `detect_possible_contradiction(&self, new_content: &str, existing_id: &str) ->
     Result<bool>`: embed `new_content`, compute cosine similarity against the existing memory's
     stored vector, return `true` above a documented threshold (e.g. 0.75) but text literally
     differs. Document explicitly: this **flags a candidate for review, it does not verify or
     auto-resolve a contradiction, and it never calls `update()` on its own.**
102. Rustdoc on `store` vs `store_with_options` vs `update`, explaining the difference.
103. `cargo clippy` / `cargo fmt` clean.
104. Re-run the entire suite from Step-40-repair through M6 — confirm zero regressions.
105. **Checkpoint:** `store_with_options()` and `update(MemoryUpdate)` exist, are tested, use
     confidence correctly, and never lose data on partial failure. `cargo test` green.

---

# M7 — Temporal querying (Steps 106–120)

**Goal:** answer "what changed" and "what's stale," precisely defined, without overclaiming what
the SQL actually returns.

### Step 106 — Extend `RecallQuery`
**File:** `src/recall.rs` (edit).
```rust
pub struct RecallQuery {
    // ...existing fields from Step 64...
    pub created_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_before: Option<chrono::DateTime<chrono::Utc>>,
    pub only_stale: bool,
}
```
Add `.created_after(dt)`, `.created_before(dt)`, `.only_stale(bool)` builder methods. Validation:
if both are set and `created_after > created_before`, `recall()` returns
`Err(MemoliteError::InvalidArgument("inverted time range"))` rather than silently returning
nothing.

### Step 107 — `query_by_time_range`, named parameters, explicit columns
**File:** `src/db/queries.rs` (new function). Uses the same explicit column list as everywhere
else — never `SELECT *`.
```rust
pub fn query_by_time_range(
    conn: &Connection,
    after: Option<i64>,
    before: Option<i64>,
) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, content, type, importance, created_at, last_accessed, access_count,
                expires_at, superseded_by, metadata, confidence
         FROM memories
         WHERE (:after IS NULL OR created_at >= :after)
           AND (:before IS NULL OR created_at <= :before)"
    )?;
    let rows = stmt.query_map(
        named_params! { ":after": after, ":before": before },
        row_to_memory,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
```

### Step 108 — `what_changed_since`, precisely defined
**File:** `src/engine.rs`. This is **not** the same as `query_by_time_range` alone —
`query_by_time_range` only returns newly-created rows. "What changed" also needs replacement
memories (from `update()`) created since the cutoff, correlated back to their originals.

```rust
pub struct ChangeRecord {
    pub old_id: Option<String>, // None for a brand-new memory, not a revision
    pub new_memory: Memory,
}

pub async fn what_changed_since(&self, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<ChangeRecord>> {
    let new_rows = self.db.query_by_time_range(Some(since.timestamp()), None)?;
    let mut out = Vec::with_capacity(new_rows.len());
    for mem in new_rows {
        let old_id = self.db.find_superseded_original_id(&mem.id)?; // None if brand-new
        out.push(ChangeRecord { old_id, new_memory: mem });
    }
    Ok(out)
}
```
Add `find_superseded_original_id(new_id: &str) -> Result<Option<String>>` in `db/queries.rs`
(`SELECT id FROM memories WHERE superseded_by = ?1`).

### Step 109 — Define "stale" from `last_accessed`, with documented half-lives
**File:** `src/ranking.rs` (edit — append).
```rust
/// A memory is stale if its recency_factor (computed from days since LAST ACCESS,
/// not creation) has decayed below 0.2 — roughly 2.3 half-lives without re-access.
/// This threshold is a documented constant, not tuned per-deployment in v1.
pub const STALE_THRESHOLD: f32 = 0.2;

pub fn is_stale(days_since_last_access: f64, memory_type: MemoryType) -> bool {
    recency_factor(days_since_last_access, memory_type) < STALE_THRESHOLD
}
```

### Step 110 — `find_stale_memories()`
**File:** `src/engine.rs`. Excludes expired and superseded memories by default — a full-table
scan by design, since staleness is a maintenance query, not a hot-path recall.
```rust
pub async fn find_stale_memories(&self) -> Result<Vec<Memory>> {
    let all = self.db.get_active_memories()?; // excludes expired + superseded
    let now = chrono::Utc::now();
    Ok(all.into_iter()
        .filter(|m| {
            let days = (now - m.last_accessed).num_days().max(0) as f64;
            ranking::is_stale(days, m.memory_type)
        })
        .collect())
}
```

### Step 111 — Apply the temporal filters inside `recall()`
Add `created_after`/`created_before`/`only_stale` as additional predicates in the same filter
stage as Steps 65–67 (M4), applied to the over-fetched candidate pool before scoring.

### Steps 112–120 — Tests + checkpoint
112. `what_changed_since()` returns a memory created after the cutoff, excludes one before it.
113. `what_changed_since()` picks up an `update()`-created replacement even if the *original*
     fact predates the cutoff, and correctly sets `old_id`.
114. A brand-new (non-revision) memory in the result has `old_id == None`.
115. `find_stale_memories()`: manually backdate a memory's `last_accessed` (directly in a test
     database, **not** via `with_ttl`, which only affects `expires_at` and never backdates
     `created_at`); confirm it appears. A freshly-accessed one doesn't.
116. `RecallQuery::new(...).only_stale(true)` filters correctly inside a real `recall()` call.
117. `.created_after()` + `.created_before()` combined narrow correctly; an inverted range
     returns `Err(InvalidArgument)`, not an empty/wrong result.
118. Tests use a test-only clock/database abstraction to set exact timestamps, not real sleeps.
119. `cargo clippy` / `cargo fmt` clean.
120. **Checkpoint:** "what's new," "what changed" (with old/new correlation), and "what's stale"
     (from last-access, not creation) all work as first-class queries. `cargo test` green.

---

# Concurrency refactor (do this before M8 — nothing is spawned yet)

**Goal:** make `MemoryEngine` safely shareable across threads/tasks *before* anything is spawned
against it. This has to happen before M8 (streaming) and M10 (maintenance worker), both of which
hand `Arc<MemoryEngine>` (or `Weak<MemoryEngine>`) into background tasks.

### Step 121 — `Mutex<Connection>`
**File:** `src/engine.rs` (edit). Replace `conn: Connection` with `conn: Mutex<Connection>`. This
is acceptable for a local, single-process v1; document that a higher-throughput version should
replace it with a dedicated SQLite actor rather than holding one connection under lock across
concurrent callers. Every lock acquisition must be scoped tightly and released before any `.await`
— never hold the `MutexGuard` across an await point.

```rust
pub struct MemoryEngine {
    conn: std::sync::Mutex<rusqlite::Connection>,
    embedder: Embedder,
    vector_store: Box<dyn VectorStore>,
}
```

### Step 122 — Compile-time `Send + Sync` gate
**File:** `tests/send_sync.rs` (new).
```rust
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn memory_engine_is_send_sync() {
    assert_send_sync::<memolite::MemoryEngine>();
}
```
This must compile and pass **before** M8 or M10 add a single `tokio::spawn`.

### Steps 123–125 — Tests + checkpoint
123. Every existing lock-scope call site is audited: confirm the guard is dropped before any
     `.await` in the same function (search for `.lock().unwrap()` followed later by `.await` in
     the same scope).
124. `cargo clippy` clean (clippy will often flag holding a guard across an await).
125. **Checkpoint:** `MemoryEngine: Send + Sync` is proven at compile time, and no lock is ever
     held across an await point. Safe to build M8 and M10 on top of this.

---

# M8 — Concurrency-safe streaming ingestion (Steps 126–145)

**Goal:** incremental memory formation from a live token/text stream, with **observable errors**
— the original design's `eprintln!`-only error path is replaced with a structured report.

### Step 126 — Ingestion message type
**File:** `src/streaming.rs` (new).
```rust
use crate::memory::MemoryType;

#[derive(Debug, Clone)]
pub struct IngestChunk {
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
}
```

### Step 127 — `StreamIngestor`, redesigned for observability
**File:** `src/streaming.rs` (edit — append).
```rust
use tokio::sync::mpsc::{self, Sender};
use tokio::task::JoinHandle;
use std::sync::Arc;
use crate::engine::MemoryEngine;
use crate::requests::StoreRequest;
use crate::error::Result;

#[derive(Debug, Default)]
pub struct IngestReport {
    pub accepted: usize,
    pub stored: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>, // (chunk content preview, error message)
}

pub struct StreamIngestor {
    sender: Option<Sender<IngestChunk>>,
    handle: JoinHandle<Result<IngestReport>>,
}

impl StreamIngestor {
    pub fn spawn(engine: Arc<MemoryEngine>, buffer_size: usize) -> Result<Self> {
        if buffer_size == 0 {
            return Err(crate::error::MemoliteError::InvalidArgument("buffer_size must be > 0".into()));
        }
        let (tx, mut rx) = mpsc::channel::<IngestChunk>(buffer_size);

        let handle = tokio::spawn(async move {
            let mut report = IngestReport::default();
            while let Some(chunk) = rx.recv().await {
                report.accepted += 1;
                let request = StoreRequest::new(&chunk.content, chunk.memory_type, chunk.importance);
                match engine.store_with_options(request).await {
                    Ok(_) => report.stored += 1,
                    Err(e) => {
                        report.failed += 1;
                        let preview: String = chunk.content.chars().take(60).collect();
                        report.errors.push((preview, e.to_string()));
                        // One bad chunk does not kill the task; ingestion continues.
                    }
                }
            }
            Ok(report)
        });

        Ok(Self { sender: Some(tx), handle })
    }

    pub fn sender(&self) -> Sender<IngestChunk> {
        self.sender.clone().expect("sender available until shutdown")
    }

    /// Explicit, awaitable shutdown: closes the channel, waits for the task to drain
    /// and exit, and returns the accumulated report (or the join/panic error).
    pub async fn shutdown(mut self) -> Result<IngestReport> {
        self.sender.take(); // drop the sender -> rx.recv() returns None -> loop exits
        self.handle.await.map_err(|e| crate::error::MemoliteError::Internal(e.to_string()))?
    }
}
```

### Step 128 — Why `Arc<MemoryEngine>`
Document in a comment at the top of `streaming.rs`: the background task holds `engine` for its
whole lifetime while the caller keeps using it elsewhere (thanks to the M6.5 concurrency refactor,
this is now provably safe). Only the specific call sites that spawn a `StreamIngestor` need
`Arc::new(engine)`.

### Step 129 — `SentenceBuffer::feed` emits *all* complete sentences
**File:** `src/streaming.rs` (edit — append). Corrected from the original `Option<String>`
version, which silently dropped a second sentence boundary landing in the same fragment.
```rust
pub struct SentenceBuffer {
    buf: String,
}

impl SentenceBuffer {
    pub fn new() -> Self { Self { buf: String::new() } }

    /// Feed one token/fragment. Returns every complete sentence found, in order.
    /// Handles multiple boundaries in a single fragment and repeated punctuation ("...", "?!").
    pub fn feed(&mut self, fragment: &str) -> Vec<String> {
        self.buf.push_str(fragment);
        let mut sentences = Vec::new();
        loop {
            match self.buf.find(['.', '!', '?']) {
                Some(pos) => {
                    // Absorb any immediately-repeated terminal punctuation ("...", "?!").
                    let mut end = pos + 1;
                    while self.buf[end..].starts_with(['.', '!', '?']) {
                        end += 1;
                    }
                    let sentence = self.buf[..end].trim().to_string();
                    if !sentence.is_empty() {
                        sentences.push(sentence);
                    }
                    self.buf = self.buf[end..].to_string();
                }
                None => break,
            }
        }
        sentences
    }

    /// Flush any trailing fragment at end of stream (no terminal punctuation seen).
    pub fn finish(&mut self) -> Option<String> {
        let remaining = self.buf.trim().to_string();
        self.buf.clear();
        if remaining.is_empty() { None } else { Some(remaining) }
    }
}
```

### Steps 130–134 — Wire buffering into the ingestor
130. Keep a `SentenceBuffer` per stream/turn in the caller (e.g. `examples/agent_loop.rs`).
131. For every incoming token, call `buffer.feed(token)`, and for **each** sentence returned,
     build an `IngestChunk` and `sender.send(chunk).await`.
132. At end of stream, call `buffer.finish()` and send the trailing fragment too, if any.
133. Default `importance` for streamed content (e.g. `0.4`), documented as a deliberate default,
     not an inference.
134. Bounded-channel backpressure is retained: `sender.send(...).await` simply waits if full.
     Document why bounded was chosen over unbounded (unbounded can grow memory without limit if
     ingestion outpaces storage).

### Step 135 — `examples/streaming_ingest.rs`
Simulate a fake token stream, feed through `SentenceBuffer`, send through a real `StreamIngestor`,
call `.shutdown().await` at the end, print the returned `IngestReport`, then `recall()` something
to prove end-to-end retrievability.

### Steps 136–145 — Tests + checkpoint
136. `SentenceBuffer::feed`: a single fragment containing **two** sentence boundaries returns a
     `Vec` of length 2, in order — this directly tests the fix from Step 129.
137. `SentenceBuffer::feed`: repeated punctuation (`"Wait... really?!"`) is treated as one boundary
     each, not split mid-punctuation.
138. Unicode test: a fragment containing multi-byte UTF-8 characters around a sentence boundary
     does not panic (byte-index slicing must respect char boundaries — use `char_indices`-safe
     splitting, not raw byte slicing, if content isn't guaranteed ASCII).
139. `finish()` flushes a trailing fragment with no terminal punctuation; returns `None` if buffer
     is empty.
140. `StreamIngestor`: send 3 chunks, call `.shutdown().await`, confirm the returned
     `IngestReport { accepted: 3, stored: 3, failed: 0, .. }` and that `recall()` finds all 3.
141. A deliberately failing chunk (mock a `store_with_options` failure) increments `failed` and
     appends to `errors` **without** stopping subsequent chunks from being ingested.
142. Backpressure test: `buffer_size = 1`, send 5 chunks, confirm all 5 eventually land in the
     report (may take longer — that's backpressure working, not a bug).
143. `StreamIngestor::spawn(engine, 0)` returns `Err(InvalidArgument)`, not a panic.
144. `cargo clippy` / `cargo fmt` clean; re-run the full suite through M7 + concurrency refactor.
145. **Checkpoint:** streamed content is retrievable end-to-end, every failure is observable in a
     structured report (never only `eprintln!`), and shutdown is explicit and awaitable.

---

# M9 — Memory compression (Steps 146–170)

**Goal:** consolidate old, low-value episodic memories with zero data loss and a truthful
description of what's actually being saved (retrieval density, **not** disk space, unless a
separate hard-delete policy is added later).

### Step 146 — Eligibility
**File:** `src/compression.rs` (new).
```rust
use crate::memory::{Memory, MemoryType};
use chrono::Utc;

/// Eligible: Episodic, older than 14 days, importance < 0.3, not already superseded.
/// Semantic and Procedural memories are NEVER auto-compressed — this is a deliberate
/// scope boundary, not an oversight.
pub fn is_compression_eligible(mem: &Memory) -> bool {
    let age_days = (Utc::now() - mem.created_at).num_days();
    mem.memory_type == MemoryType::Episodic
        && age_days > 14
        && mem.importance < 0.3
        && mem.superseded_by.is_none()
}
```

### Step 147 — Fetch candidates via explicit SQL
**File:** `src/db/queries.rs` (new function), explicit column list, no `SELECT *`.
```rust
pub fn get_episodic_memories_older_than(conn: &Connection, days: i64) -> Result<Vec<Memory>> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days)).timestamp();
    let mut stmt = conn.prepare(
        "SELECT id, content, type, importance, created_at, last_accessed, access_count,
                expires_at, superseded_by, metadata, confidence
         FROM memories
         WHERE type = 'episodic' AND created_at < ?1 AND superseded_by IS NULL"
    )?;
    let rows = stmt.query_map(params![cutoff], row_to_memory)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_embeddings(conn: &Connection, ids: &[String]) -> Result<Vec<(String, Vec<f32>)>> {
    // Explicit per-id fetch, batched — never string-concatenate SQL from `ids`.
    let mut out = Vec::with_capacity(ids.len());
    let mut stmt = conn.prepare("SELECT vector FROM embeddings WHERE memory_id = ?1")?;
    for id in ids {
        if let Ok(bytes) = stmt.query_row(params![id], |r| r.get::<_, Vec<u8>>(0)) {
            out.push((id.clone(), bytes_to_vec_f32(&bytes)));
        }
    }
    Ok(out)
}
```

### Step 148 — Greedy clustering with `Uuid` and the shared cosine helper
**File:** `src/compression.rs` (edit — append). Uses `math_utils::cosine_similarity` (Step 42) —
no local redefinition.
```rust
use uuid::Uuid;
use crate::math_utils::cosine_similarity;

pub struct Cluster {
    pub member_ids: Vec<String>,
}

/// Greedy single-pass clustering, O(n^2) worst case — fine because this only ever
/// runs over the small compression-eligible subset, never the whole store.
pub fn greedy_cluster(memories: &[(String, Vec<f32>)], threshold: f32) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut assigned = vec![false; memories.len()];

    for i in 0..memories.len() {
        if assigned[i] { continue; }
        let mut cluster = Cluster { member_ids: vec![memories[i].0.clone()] };
        assigned[i] = true;
        for j in (i + 1)..memories.len() {
            if assigned[j] { continue; }
            if cosine_similarity(&memories[i].1, &memories[j].1) >= threshold {
                cluster.member_ids.push(memories[j].0.clone());
                assigned[j] = true;
            }
        }
        clusters.push(cluster);
    }
    clusters
}
```
Only compress clusters with **3+ members** — filter `clusters` to `member_ids.len() >= 3` before
summarizing (compressing 1–2 memories saves nothing and destroys detail for no benefit).

### Step 149 — Bounded, fallible summarization
**File:** `src/compression.rs` (edit — append). Corrected: returns `Result` for an empty cluster
instead of `unwrap()`-panicking, and enforces a **maximum summary size** so concatenation can't
produce an ever-growing record.
```rust
use crate::error::{MemoliteError, Result};

pub const MAX_SUMMARY_CHARS: usize = 4000;
pub const COMPRESSION_ALGORITHM_VERSION: &str = "extractive-v1";

pub struct CompressionResult {
    pub summary_content: String,
    pub original_ids: Vec<String>,
    pub cluster_id: Uuid,
    pub threshold: f32,
    pub time_range: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
}

pub fn summarize_cluster(memories: &[Memory], threshold: f32) -> Result<CompressionResult> {
    if memories.is_empty() {
        return Err(MemoliteError::InvalidArgument("cannot summarize an empty cluster".into()));
    }
    let earliest = memories.iter().map(|m| m.created_at).min().unwrap(); // safe: non-empty checked above
    let latest = memories.iter().map(|m| m.created_at).max().unwrap();

    let mut joined = String::new();
    for m in memories {
        if joined.len() + m.content.len() + 2 > MAX_SUMMARY_CHARS {
            joined.push_str("...[truncated, size cap reached]");
            break;
        }
        if !joined.is_empty() { joined.push_str("; "); }
        joined.push_str(&m.content);
    }

    let summary_content = format!(
        "[Compressed summary of {} similar episodic memories, {} to {}]: {joined}",
        memories.len(), earliest.format("%Y-%m-%d"), latest.format("%Y-%m-%d")
    );

    Ok(CompressionResult {
        summary_content,
        original_ids: memories.iter().map(|m| m.id.clone()).collect(),
        cluster_id: Uuid::new_v4(),
        threshold,
        time_range: (earliest, latest),
    })
}
```
Module-level rustdoc, stated plainly: **this is extractive, not abstractive, summarization** —
it concatenates rather than rewrites. A real LLM-based summarizer is a drop-in replacement for
this one function later, without touching the rest of the pipeline. It also does **not** reduce
database disk usage — originals remain in SQLite (removed only from the live vector index) — so
never call it "space saving" without a separate, explicit archival/hard-delete policy.

### Step 150 — `compress_old_memories()`, transaction-safe with index recovery
**File:** `src/engine.rs` (new method). The critical fix here: mark-superseded happens in **one
SQLite transaction covering the whole cluster**; index deletion happens **only after that
transaction commits**; and if index deletion partially fails, the active index is rebuilt from
SQLite rather than left inconsistent.
```rust
pub async fn compress_old_memories(&self) -> Result<usize> {
    let candidates: Vec<Memory> = self.db.get_episodic_memories_older_than(14)?
        .into_iter()
        .filter(compression::is_compression_eligible)
        .collect();

    let ids: Vec<String> = candidates.iter().map(|m| m.id.clone()).collect();
    let with_vectors = self.db.get_embeddings(&ids)?;

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
        metadata.insert("compression.original_ids".into(), serde_json::json!(result.original_ids));
        metadata.insert("compression.threshold".into(), serde_json::json!(result.threshold));
        metadata.insert("compression.algorithm_version".into(), serde_json::json!(compression::COMPRESSION_ALGORITHM_VERSION));
        metadata.insert("compression.time_range".into(), serde_json::json!([
            result.time_range.0.to_rfc3339(), result.time_range.1.to_rfc3339()
        ]));

        let request = StoreRequest::new(&result.summary_content, MemoryType::Episodic, 0.3)
            .with_confidence(ConfidenceLevel::Inferred);
        let request = result.original_ids.iter().fold(request, |r, _| r); // (metadata attached below)
        let mut request = request;
        request.metadata = metadata;

        let new_id = self.store_with_options(request).await?;

        // Single transaction: mark ALL originals superseded together.
        self.db.mark_all_superseded(&result.original_ids, &new_id)?;

        // Only after that transaction commits, remove originals from the live vector index.
        let mut index_errors = Vec::new();
        for old_id in &result.original_ids {
            if let Err(e) = self.vector_store.delete(old_id).await {
                index_errors.push((old_id.clone(), e));
            }
        }
        if !index_errors.is_empty() {
            // Rebuild/backfill the active index from SQLite so a restart reconstructs
            // exactly the same active index, rather than leaving stale entries behind.
            self.rebuild_vector_index_from_sqlite().await?;
            return Err(MemoliteError::VectorStore(format!(
                "partial index deletion failure during compression: {index_errors:?}"
            )));
        }

        compressed_count += members.len();
    }

    Ok(compressed_count)
}
```
Add `mark_all_superseded(ids: &[String], new_id: &str) -> Result<()>` in `db/queries.rs` as one
transaction over all IDs. Add `rebuild_vector_index_from_sqlite()` on `MemoryEngine`: clears the
live vector store and re-inserts every non-superseded, non-expired memory's stored embedding.

### Step 151 — `stats()` / `MemoryStats`
Add a fully specified `stats() -> Result<MemoryStats>` with counts for active, expired,
superseded, embeddings, and compressed summaries (identifiable via the
`compression.original_ids` metadata key). Do not reference `MemoryStats` anywhere without this
definition existing.

### Steps 152–170 — Tests + checkpoint
152. `is_compression_eligible`: a 20-day-old episodic memory at importance 0.2 → eligible; the
     same but semantic → not eligible; a 5-day-old episodic memory → not eligible (too young).
153. `greedy_cluster`: three near-identical vectors + one far-away vector at threshold 0.85 →
     one cluster of 3, one cluster of 1.
154. `summarize_cluster` on an empty slice returns `Err`, not a panic.
155. `summarize_cluster` with content exceeding `MAX_SUMMARY_CHARS` truncates and appends the
     truncation marker, never silently growing unbounded.
156. Integration: store 3 similar low-importance episodic memories with `created_at` **directly
     backdated in a test database** (not via `with_ttl`, which does not backdate creation time)
     to >14 days ago; `compress_old_memories()` returns `3`.
157. Post-compression: default `recall()` no longer surfaces the 3 originals but does surface the
     new summary memory, carrying the `compression.*` provenance metadata.
158. `recall(...).include_superseded(true)` still shows the 3 originals, flagged via
     `superseded_by`.
159. The 3 originals no longer appear in raw `vector_store.search()` results.
160. A cluster of exactly 2 similar memories is **not** compressed.
161. Running `compress_old_memories()` twice back-to-back compresses zero new memories the
     second time (idempotency).
162. Semantic and Procedural memories are never touched even if old and low-importance.
163. Simulate a partial index-deletion failure (mock `vector_store.delete` to fail for one ID);
     confirm the index is rebuilt from SQLite and the call returns an error rather than leaving
     the index silently inconsistent.
164. `stats()` reports a non-zero `compressed_memory_count` after compression runs.
165. Rustdoc / `README.md` "Risks and Honest Limitations" both state compression is extractive
     and does not reduce disk usage.
166. `cargo clippy` / `cargo fmt` clean.
167. Benchmark `compress_old_memories()` at ~1,000 eligible candidates — confirm the O(n²)
     clustering step is not a real bottleneck at realistic scale.
168. Restart test: after a successful compression run, close and reopen the engine, confirm
     `vector_store.search()` results match exactly what SQLite says should be active (proves the
     "restart reconstructs the same active index" guarantee).
169. Re-run the full suite from Step-40-repair through the concurrency refactor and M8.
170. **Checkpoint:** low-value clutter consolidates automatically with zero data loss, full
     provenance, and a truthful "retrieval density, not disk savings" claim.

---

# M10 — Explicit maintenance controller with cancellation (Steps 171–190)

**Goal:** background purge + compression, with an explicit, cancellable controller — never a
self-referential worker spawned inside `open()`.

### Step 171 — Add `tokio-util`
`Cargo.toml`: add `tokio-util = { version = "0.7", features = ["rt"] }` for `CancellationToken`
(or use a Tokio `watch` channel if you'd rather avoid the extra dependency — document the choice).

### Step 172 — `MaintenanceHandle`, explicit controller
**File:** `src/maintenance.rs` (new).
```rust
use tokio_util::sync::CancellationToken;
use tokio::task::JoinHandle;
use std::sync::{Arc, Weak};
use crate::engine::MemoryEngine;

pub struct MaintenanceConfig {
    pub purge_interval: std::time::Duration,    // e.g. hourly
    pub compress_interval: std::time::Duration, // e.g. daily
}

pub struct MaintenanceHandle {
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

impl MemoryEngine {
    /// Explicit opt-in — never spawned automatically inside `open()`.
    /// Captures a Weak<MemoryEngine>, so dropping the last application Arc allows exit.
    pub fn start_maintenance(self: &Arc<Self>, config: MaintenanceConfig) -> MaintenanceHandle {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let weak: Weak<MemoryEngine> = Arc::downgrade(self);

        let join = tokio::spawn(async move {
            let mut purge_tick = tokio::time::interval(config.purge_interval);
            purge_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut compress_tick = tokio::time::interval(config.compress_interval);
            compress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    _ = purge_tick.tick() => {
                        let Some(engine) = weak.upgrade() else { break }; // app dropped its Arc
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
        });

        MaintenanceHandle { cancel, join }
    }
}

impl MaintenanceHandle {
    pub async fn shutdown(self) -> crate::error::Result<()> {
        self.cancel.cancel();
        // A panic inside the task surfaces here via the join error rather than being
        // silently swallowed — never wrap it in an ordinary Result inside the loop.
        self.join.await.map_err(|e| crate::error::MemoliteError::Internal(e.to_string()))
    }
}
```
Purge and compression run on **separate intervals** (not a fragile shared tick counter). Purge
must also delete IDs from the vector store (or trigger the M9 rebuild path) since custom vector
backends may not mirror SQLite cascades. `MissedTickBehavior::Skip` is set explicitly on both
intervals, and the first tick does **not** fire immediately unless that's separately documented
as intended.

### Step 173 — Never catch panics with an ordinary `Result`
Document explicitly: a panic inside the maintenance loop is **not** caught with `Result` — it
either surfaces through `handle.await`'s join error (the default, simplest choice) or, only if the
added complexity is justified, each tick body is wrapped with `FutureExt::catch_unwind`. Pick one
and document why.

### Steps 174–190 — Tests + checkpoint
174. Use `#[tokio::test(start_paused = true)]` and `tokio::time::advance(...)` to test tick timing
     — never real sleeps.
175. Cancellation: call `shutdown()`, confirm the task exits promptly and `join` returns `Ok(())`.
176. Engine drop: drop the last `Arc<MemoryEngine>` without calling `shutdown()`, confirm the
     `Weak::upgrade()` failure causes the task to exit rather than looping forever on a dead
     engine.
177. Concurrent calls: `store()`/`recall()` running concurrently with a maintenance tick don't
     deadlock or block unreasonably (the concurrency refactor's short lock scopes make this safe).
178. Missed-tick test: advance paused time past several intervals at once, confirm `Skip`
     behavior — no burst of queued ticks firing back-to-back.
179. Error continuation: force `purge_expired()` to fail once, confirm the loop logs and continues
     ticking rather than exiting.
180. No detached task: confirm there is no `tokio::spawn` anywhere in `open()` itself — maintenance
     is opt-in only, via `start_maintenance()`.
181. Purge also removes IDs from the vector store — test directly.
182. `tracing` is wired in for every purge/compression run (count of affected memories logged).
183. Document the exact shutdown semantics (cancellation vs. engine-drop) in `ARCHITECTURE.md`.
184. `(OPTIONAL)` expose tick counts via `stats()`.
185. Benchmark: confirm store/recall latency is unaffected (within noise) whether or not
     maintenance is mid-tick.
186. `cargo clippy` / `cargo fmt` clean.
187. Re-run the full suite through M9.
188. Confirm `MemoryEngine::open()` alone, with maintenance never started, behaves identically to
     before this milestone — maintenance must be fully opt-in with zero side effects otherwise.
189. Confirm two separate `MaintenanceHandle`s can't be started redundantly without documented
     behavior (either allow it and document double-runs, or guard against it — pick one).
190. **Checkpoint:** purge and compression run automatically only when explicitly started, are
     cleanly cancellable, survive missed ticks and transient errors, and never leak a detached
     task.

---

# M11 — Optional VecLite backend (Steps 191–205)

**Goal:** a real, pinned, truthfully-scoped adapter — not a fictional protocol.

### Step 191 — Pin the real protocol first
Before writing any adapter code: pin a concrete VecLite repository/version and copy its
**authoritative** API contract into an integration test fixture. If no stable client/API exists
yet, ship M11 as a **generic HTTP adapter example** instead, and do not market it as tested VecLite
support — say exactly that in the docs.

### Step 192 — Feature flag, correctly scoped
**File:** `Cargo.toml`.
```toml
[dependencies]
reqwest = { version = "0.12", features = ["json"], optional = true }

[features]
veclite = ["dep:reqwest"]
```
(`["dep:reqwest"]`, not `["reqwest"]` — the old syntax also implicitly creates a feature named
`reqwest`, which shadows the dependency name and causes confusing feature-unification bugs.)

### Step 193 — The adapter
**File:** `src/vector_store/veclite.rs` (new, feature-gated).
```rust
#![cfg(feature = "veclite")]
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;
use serde_json::Value;
use crate::error::{Result, MemoliteError};
use super::{VectorStore, VectorHit};

pub struct VecLiteVectorStore {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    collection: String,
    dim: usize,
}

impl VecLiteVectorStore {
    pub fn new(base_url: &str, api_key: &str, collection: &str, dim: usize) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(Self { client, base_url: base_url.to_string(), api_key: api_key.to_string(), collection: collection.to_string(), dim })
    }

    /// Verify the remote collection exists and its dimension matches `self.dim`
    /// before any insert/search traffic is sent. Called once from an explicit
    /// `connect()` step, not silently on every request.
    pub async fn verify_collection(&self) -> Result<()> {
        let url = format!("{}/collections/{}", self.base_url, urlencoding::encode(&self.collection));
        let resp = self.client.get(&url).bearer_auth(&self.api_key).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        let body: serde_json::Value = resp.json().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        let remote_dim = body["dimension"].as_u64().unwrap_or(0) as usize;
        if remote_dim != self.dim {
            return Err(MemoliteError::VectorStore(format!(
                "dimension mismatch: local={}, remote={}", self.dim, remote_dim
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl VectorStore for VecLiteVectorStore {
    async fn insert(&self, id: &str, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        let url = format!("{}/collections/{}/vectors", self.base_url, urlencoding::encode(&self.collection));
        self.client.post(&url)
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "id": id, "vector": vector, "metadata": metadata }))
            .send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        let url = format!("{}/collections/{}/search", self.base_url, urlencoding::encode(&self.collection));
        let resp = self.client.post(&url)
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "vector": query, "k": k }))
            .send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        // Map backend IDs/scores explicitly rather than assuming the wire shape
        // matches VectorHit exactly — only derive Serialize/Deserialize on VectorHit
        // if the real wire format is confirmed to match (see Step 191).
        #[derive(serde::Deserialize)]
        struct WireHit { id: String, score: f32 }
        let wire: Vec<WireHit> = resp.json().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(wire.into_iter().map(|h| VectorHit { id: h.id, score: h.score }).collect())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let url = format!("{}/collections/{}/vectors/{}",
            self.base_url, urlencoding::encode(&self.collection), urlencoding::encode(id));
        self.client.delete(&url).bearer_auth(&self.api_key).send().await
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?
            .error_for_status()
            .map_err(|e| MemoliteError::VectorStore(e.to_string()))?;
        Ok(())
    }

    fn dimension(&self) -> usize { self.dim }
}

impl std::fmt::Debug for VecLiteVectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the API key in debug output.
        f.debug_struct("VecLiteVectorStore")
            .field("base_url", &self.base_url)
            .field("collection", &self.collection)
            .field("dim", &self.dim)
            .field("api_key", &"<redacted>")
            .finish()
    }
}
```
Path components (`collection`, `id`) are URL-encoded. Every request uses `.error_for_status()` and
an explicit timeout. Backfill ownership: document that inserting historical vectors into a fresh
remote collection is the caller's responsibility via repeated `insert()` calls, not an automatic
migration path in v1.

### Step 194 — Register conditionally
**File:** `src/vector_store/mod.rs`.
```rust
#[cfg(feature = "veclite")]
pub mod veclite;
#[cfg(feature = "veclite")]
pub use veclite::VecLiteVectorStore;
```

### Step 195 — `open_with_store()`
**File:** `src/engine.rs`.
```rust
pub async fn open_with_store(path: impl AsRef<std::path::Path>, store: impl VectorStore + 'static) -> Result<Self> {
    // Same as open(), but skip constructing InMemoryVectorStore and use `store` instead.
}
```

### Steps 196–205 — Tests + checkpoint
196. Unit tests use a **local mock server** (e.g. `wiremock`), not a live VecLite instance.
197. `tests/veclite_integration_test.rs`, gated `#[cfg(feature = "veclite")]`, opt-in via
     environment variables (e.g. `VECLITE_TEST_URL`); if unset, the test **skips with a clear
     printed message**, it does not fail the suite.
198. Integration test (when configured): store 5 memories via `open_with_store(...,
     VecLiteVectorStore::new(...)?)`, `recall()` one, confirm correctness.
199. `verify_collection()` correctly errors on a dimension mismatch.
200. `Debug` output for `VecLiteVectorStore` never contains the raw API key — assert on the
     formatted string directly.
201. `cargo test` (default, no features) passes with zero VecLite instance running.
202. `cargo build` (default, no features) does not pull in `reqwest` — verify via `cargo tree`.
203. `cargo clippy --all-features` clean; `cargo fmt` clean.
204. `ARCHITECTURE.md` documents the two-line swap from `InMemoryVectorStore` to
     `VecLiteVectorStore`, with the actual before/after lines shown.
205. **Checkpoint:** the scale-up path is real, pinned against an actual protocol version, opt-in,
     and truthfully scoped — never described as more "tested" than it is.

---

# M12 — Final polish, docs, and release gate (Steps 206–220)

### Step 206 — Full rustdoc pass
Every `pub` item across `lib.rs`, `engine.rs`, `recall.rs`, `requests.rs`, `confidence.rs`,
`compression.rs`, `streaming.rs`, `maintenance.rs`, `ranking.rs`, `math_utils.rs`,
`vector_store/*.rs` gets a `///` doc comment. `cargo doc --no-deps --all-features --open` and read
it as a stranger would.

### Step 207 — `README.md` reconciliation
Diff the README's Public API section against the real signatures in `engine.rs`, line by line —
`store_with_options(StoreRequest)`, `update(id, MemoryUpdate)`, `recall(RecallQuery)`,
`recall_text(&str)`, `what_changed_since`, `find_stale_memories`, `compress_old_memories`,
`start_maintenance`/`MaintenanceHandle::shutdown` — until there is zero drift.

### Step 208 — Feature list reconciliation
Move temporal querying, streaming ingestion, memory compression, and confidence scoring from
"build if time allows" into "must-ship (v1 core)" — they're shipped, not aspirational.

### Step 209 — `ARCHITECTURE.md`
Document, explicitly:
- Thread/concurrency model (`Mutex<Connection>`, `Send + Sync` proof, lock-scope discipline).
- Durability boundaries (what a crash mid-`store_with_options` can and cannot lose).
- Model download/cache behavior for the embedder.
- Privacy: what's stored, what's sent to an optional remote vector backend if `veclite` is
  enabled, and what never leaves the local machine otherwise.
- Database schema and the real migration runner (Steps 71–72), including the baseline
  `pragma_table_info` inspection logic.
- Score semantics: exactly what `similarity` vs. `score` mean in `RecallItem`, and the full
  ranking formula with confidence weighting.
- Compression limitations (extractive, not abstractive; no disk-space claim).
- Optional-backend guarantees and non-guarantees (Step 191's honesty note).
- Maintenance shutdown semantics (cancellation vs. engine-drop, Step 183).

### Step 210 — "Risks and Honest Limitations"
Confirm both new limitations are present and accurate:
- Compression is extractive/concatenation-based, not LLM-abstractive, in v1.
- Contradiction *detection* (Step 101) is a similarity heuristic that flags candidates for
  review — it does not verify or auto-resolve contradictions.

### Steps 211–220 — Final validation (run in this order)
211. `cargo fmt --check`
212. `cargo clippy --all-targets --all-features -- -D warnings`
213. `cargo test --all-targets --all-features`
214. Run default-feature builds/tests **separately** too (`cargo build`, `cargo test`), so the
     `veclite` feature is proven genuinely optional, not accidentally load-bearing.
215. `cargo doc --no-deps --all-features` — zero warnings.
216. Run every example (`examples/basic.rs`, `examples/streaming_ingest.rs`,
     `examples/agent_loop.rs`) against a temporary database; each must run standalone via
     `cargo run --example <name>` and produce sensible output.
217. Verify a clean checkout/restart path: fresh clone, fresh build, fresh `open()` — no leftover
     state assumptions.
218. Test migration using a **copy of the actual current pre-migration database** — not a synthetic
     fixture — confirm it lands correctly at the latest schema version.
219. Add package metadata (`license`, `description`, `repository`, `readme`, `keywords`,
     `categories`) to `Cargo.toml`, then `cargo publish --dry-run` to confirm the crate packages
     cleanly (don't actually publish).
220. **Final checkpoint — release gate, not an automatic step:** after the user reviews the diff,
     release notes, semver choice, and this entire clean validation output, the user may
     **explicitly authorize** creating and pushing a git tag. Do not tag or publish as an automatic
     build step. Version `0.2.0` is appropriate only if `0.1.0` was actually released previously;
     otherwise choose the version number from actual release history — don't assume.

---

## Quick reference: what changed vs. the original plan

| Original plan | Final Memolite plan |
|---|---|
| `ContextMemory` / `ContextMemoryError` | **`Memolite`** / **`MemoliteError`**, throughout |
| M5 (requests/updates) built before M6 (confidence) | **M6 built first** — no temporary non-compiling state |
| `update(id, new_content: &str)` | `update(id, MemoryUpdate)` — typed, partial-update, with **compensation on failure** |
| `RecallQuery.memory_type_filter: Option<MemoryType>` | `RecallQuery.memory_types: Option<Vec<MemoryType>>` + `include_expired`/`include_superseded`/`metadata_equals` |
| `recall()` returns `Vec<Memory>` / ad hoc `RecallResult` | `RecallResult { items: Vec<RecallItem> }`, each item has `similarity` **and** `score` |
| Fixed pool of 20 candidates before filtering | Over-fetch `max(limit*5, 50)` **before** filtering |
| `ALTER TABLE ... ADD COLUMN` with a manual existence check | Real **versioned migration runner** with `schema_migrations` table, transactions, `pragma_table_info` baselining |
| Confidence promotion as two separate steps | **One SQL statement/transaction**, promotion at exactly access count 5 |
| `SentenceBuffer::feed -> Option<String>` | `-> Vec<String>` — no dropped sentence on a double-boundary fragment |
| `StreamIngestor` errors only via `eprintln!` | Structured `IngestReport` (`accepted`/`stored`/`failed`/`errors`) + explicit awaitable `shutdown()` |
| `conn: Connection` shared ad hoc | `conn: Mutex<Connection>` + compile-time `Send + Sync` proof, done **before** anything is spawned |
| Compression: index deletion order unspecified | Mark-superseded in one transaction **first**; index deletion **after**; partial failure triggers a full index rebuild from SQLite |
| Self-spawned worker inside `open()` | Explicit `start_maintenance(self: &Arc<Self>, config) -> MaintenanceHandle`, cancellable, `Weak<MemoryEngine>`-based |
| `veclite = ["reqwest"]` | `veclite = ["dep:reqwest"]` |
| Unverified "OasysDB"/VecLite protocol | Protocol **pinned first**; generic HTTP adapter framing if no stable API exists |
| Auto-tag `v0.2.0` as a plan step | Tagging/publishing requires **explicit user authorization** after reviewing clean validation output |

Everything above is now internally consistent: no milestone references a type, table, or module
that a later milestone was going to create. Follow the **Corrected build order** table at the top,
and every intermediate commit compiles and passes `cargo test`.