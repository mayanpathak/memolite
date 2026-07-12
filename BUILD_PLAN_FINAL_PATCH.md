# Memolite build-plan final patch (repository audit, 2026-07-11)

Paste this entire appendix at the end of the 200+ step master plan. It supersedes both the original Steps 41-210 and the earlier "Corrected Build Plan, Steps 41-90" wherever they conflict.

## Audited baseline

The repository is currently complete through Step 40: SQLite persistence, typed errors, TTL/purge, a reusable `fastembed` AllMiniLML6V2 embedder, a persisted `embeddings` table, and embedding tests exist. `recall()` is still `todo!()`. There is no `db` module, migration system, vector-store module, ranking/query/request/confidence/streaming/compression module, worker, statistics API, benchmark, or VecLite adapter.

Existing names that every later step must preserve:

- Crate/error names: `memolite`, `MemoliteError`, `crate::error::Result`.
- Database field: currently `conn: rusqlite::Connection`; SQL is inline in `src/engine.rs`.
- Embedder field: `embedder: std::sync::Mutex<Embedder>`; `Embedder::embed` needs `&mut self`.
- IDs: public `Memory.id` and `Memory.superseded_by` are `Uuid`/`Option<Uuid>`; SQL/vector-store IDs are strings. Always use `memory.id.to_string()` at those boundaries.
- The embeddings table stores bincode-serialized `Vec<f32>` and dimension `384`.

Delete the large commented-out legacy copy at the top of `src/engine.rs` as a cleanup step before M3. It is dead text, not a second implementation.

## Non-negotiable corrections to the earlier patch

1. Do not add `NotFound`; it already exists in `src/error.rs`. Add only `VectorStore(String)`, plus the later variants explicitly listed below.
2. Export `InMemoryVectorStore` from `src/vector_store/mod.rs`; otherwise `use crate::vector_store::{VectorStore, InMemoryVectorStore};` does not compile.
3. `InvalidMetadata` is `#[from] serde_json::Error`; use `.map_err(MemoliteError::InvalidMetadata)?` only with the error value (or simply `?`), never pass a string.
4. Enable SQLite foreign keys on every connection with `conn.execute_batch("PRAGMA foreign_keys = ON; ...")?`. `ON DELETE CASCADE` is inert unless this pragma is enabled.
5. Validate `importance` in Rust before doing expensive embedding work. Add `InvalidImportance(f32)` and reject non-finite values and values outside `0.0..=1.0`.
6. Embed and serialize before inserting the memory. Then write the memory row and embedding row in one SQLite transaction. The current order leaves an orphan memory when embedding fails; add a regression test proving whitespace input leaves neither row.
7. Treat SQLite as the durable source of truth. If the vector-store insert fails after the SQLite transaction commits, compensate by deleting the just-created SQLite memory in a second transaction and return the vector-store error. Document that custom remote backends should make insert/delete idempotent.
8. Never use `SELECT *` with `row_to_memory`; use an explicit, shared column list in the exact decoder order.
9. Do not hold a `std::sync::MutexGuard` across `.await`. Scope the embedder/database lock completely around synchronous work, drop it, and only then call async vector-store methods.
10. Do not claim that dropping a Tokio `JoinHandle` aborts its task. It detaches the task. The worker needs a cancellation token/watch channel and an awaited shutdown path.

# Replacement M3 - durable vector search (Steps 41-55)

### 41-43: vector-store contract and implementation

Add `async-trait = "0.1"`. Create `src/vector_store/mod.rs` and `src/vector_store/in_memory.rs`. Keep the original `VectorHit` and `VectorStore` shapes, but require dimension validation:

```rust
#[async_trait::async_trait]
pub trait VectorStore: Send + Sync {
    async fn insert(&self, id: &str, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: &str) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

`InMemoryVectorStore` owns `RwLock<HashMap<String, (Vec<f32>, HashMap<String, Value>)>>` and `dimension`. `insert` and `search` return `MemoliteError::VectorStore` on a dimension mismatch or poisoned lock. Cosine similarity returns `0.0` for zero norms, rejects non-finite results, sorts descending with `f32::total_cmp`, applies deterministic ID tie-breaking, and truncates to `k`. Export it with `pub use in_memory::InMemoryVectorStore;` and export the module/types from `lib.rs`.

Tests: insert/search ordering, replace same ID, delete, empty store, `k == 0`, dimension mismatch, zero vector, deterministic ties.

### 44: engine construction and restart backfill

Add `vector_store: Arc<dyn VectorStore>` to `MemoryEngine`. Use `Arc`, not `Box`, because later workers and test doubles need cheap shared ownership. Implement one internal constructor used by both:

```rust
pub async fn open(path: impl AsRef<Path>) -> Result<Self>;
pub async fn open_with_store(
    path: impl AsRef<Path>,
    store: Arc<dyn VectorStore>,
) -> Result<Self>;
```

`open` constructs the embedder, reads its dimension, creates the in-memory store, then delegates. `open_with_store` rejects a backend whose dimension differs from the embedder. Run schema migrations, then load every `(memory_id, vector, metadata)` for active, unexpired, non-superseded memories from SQLite. Decode and dimension-check every vector before inserting it. If backfill fails, opening fails loudly; never silently start with a partial index.

### 45: atomic `store`

Refactor the old three-argument `store` to delegate to the M5 request implementation once M5 lands. Until then, implement this order: validate -> embed -> encode -> SQLite transaction inserting both rows -> vector-store insert -> compensation on vector-store failure. Do not recompute the vector.

### 46-48: recall and access mutation

The temporary M3 signature is `recall(&self, query: &str) -> Result<Vec<Memory>>`. Embed the query under the embedder mutex, drop the guard, request more than the final limit (initially 20), load hits by ID, and exclude expired or superseded rows. Preserve vector-hit ordering. Update `access_count` and `last_accessed` in a single SQL statement for each returned row, then return the post-update values (or document and consistently test pre-update values). Use `memory.id.to_string()`.

### 49-55: required integration gates

Test semantic ordering, empty DB, restart/backfill, expired and superseded exclusion, access-stat increments, delete from both SQLite and vector store, purge from both stores, malformed embedding/dimension failure on open, and store compensation with a deliberately failing vector-store fake. Run fmt, clippy with warnings denied, and all tests.

# Replacement M4 - ranked and filtered recall (Steps 56-70)

Create `ranking.rs` and `recall.rs`. Keep scoring components individually testable and clamp/validate their inputs. Use a weighted additive score, not the original all-multiplication formula, because one zero factor otherwise erases an otherwise excellent match:

```rust
final_score = 0.60 * similarity.max(0.0)
            + 0.15 * importance
            + 0.15 * recency
            + 0.10 * reinforcement;
```

Define and document per-type recency half-lives and `reinforcement = 1.0 + ln(1 + access_count)`, normalized/clamped before combining. Add exact worked-example tests and monotonicity/boundary tests.

Use one public recall API from the end of this milestone onward:

```rust
pub async fn recall(&self, query: RecallQuery) -> Result<RecallResult>;

pub struct RecallQuery {
    pub text: String,
    pub limit: usize,
    pub min_score: Option<f32>,
    pub min_importance: Option<f32>,
    pub memory_types: Option<Vec<MemoryType>>,
    pub include_expired: bool,
    pub include_superseded: bool,
    pub metadata_equals: HashMap<String, Value>,
}

pub struct RecallItem { pub memory: Memory, pub similarity: f32, pub score: f32 }
pub struct RecallResult { pub items: Vec<RecallItem> }
```

Provide `impl From<&str> for RecallQuery` and a convenience `recall_text(&self, &str)` if ergonomic string recall is desired; Rust cannot overload two methods named `recall`. Validate `limit > 0`, finite score thresholds, and non-empty query text. Over-fetch vector candidates (`max(limit * 5, 50)`, capped/documented), apply filters, score, deterministically sort by score then ID, truncate, and update stats only for final returned items. `as_prompt_context()` must have a character/token-style budget and escape/clearly delimit untrusted memory content; never concatenate unlimited content.

# Replacement M5/M6 - requests, updates, confidence, and migrations (Steps 71-105)

Build confidence before request types so no temporary non-compiling state exists. Add `ConfidenceLevel::{as_str, parse_str, weight, maybe_promote}` and `InvalidConfidence(String)`. Use `Inferred = 0.7`, other weights `1.0`; include confidence in the final ranking calculation as a documented multiplier on the combined score.

Add a real migration runner in `engine.rs` (do not invent `src/db/*` yet): create `schema_migrations(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)`, run migrations inside a transaction, and record each version. Migration 1 represents the existing tables; migration 2 adds `confidence TEXT NOT NULL DEFAULT 'explicit' CHECK(confidence IN (...))`. Inspect `pragma_table_info` when adopting an existing database so migration 1/2 are safely baselined. Add indexes on `created_at`, `last_accessed`, `type`, `expires_at`, and `superseded_by`.

Add confidence to `Memory` and to the explicit select/decoder list. Define `StoreRequest` and `MemoryUpdate` as in the corrected plan, plus validation. Metadata is serialized once and round-trips. `custom_ttl: None` means the type default; reject non-positive TTL unless the API explicitly documents immediate expiry.

`store_with_options` uses the atomic Step 45 pipeline. `update` must run as a logical replacement: fetch the original, preserve unchanged metadata, TTL policy, type, and confidence unless explicitly changed; store the replacement; then mark the original superseded. If marking fails, compensate by deleting the replacement from both stores. Add `new_memory_type`, `new_ttl`, and `new_confidence` options or explicitly document why each is immutable. Never silently discard old metadata when `new_metadata` is `None`.

Confidence promotion happens after an access increment and uses the new count. Do one SQL statement/transaction so the threshold is not off by one. Test migration from the real current schema, idempotent reopen, every round-trip, partial updates, missing ID, compensation, and promotion exactly at count 5.

# Replacement M7 - temporal APIs (Steps 106-120)

Extend `RecallQuery` with `created_after`, `created_before`, and `only_stale`. Reject an inverted time range. All SQL uses named parameters and the shared explicit column list.

Define `what_changed_since` precisely as replacement memories created since the cutoff plus new non-superseded memories created since it; return a structured change record if callers need both the old and new IDs. Do not claim the original one-line `query_by_time_range` returns revised originals—it only returns newly created rows.

Define stale from `last_accessed`, not `created_at`, using documented per-type half-lives. Exclude expired and superseded memories by default. Tests must set timestamps directly in a test database/clock abstraction; `with_ttl` does not backdate `created_at`.

# Replacement M8 - concurrency-safe streaming (Steps 121-140)

Before spawning anything, make `MemoryEngine` shareable. Replace `conn: Connection` with `conn: Mutex<Connection>` (acceptable for this local v1) and keep every lock scope synchronous and short. Add a compile-time assertion/test that `MemoryEngine: Send + Sync`. For a higher-throughput version, replace this with a dedicated SQLite actor; do not use one connection concurrently without synchronization.

Redesign `StreamIngestor` so errors are observable:

```rust
pub struct StreamIngestor {
    sender: Option<mpsc::Sender<IngestChunk>>,
    handle: JoinHandle<Result<IngestReport>>,
}
pub async fn shutdown(mut self) -> Result<IngestReport>;
```

The report contains accepted/stored/failed counts and per-item errors (or send errors through a second channel). Validate `buffer_size > 0`. Never rely only on `eprintln!`.

`SentenceBuffer::feed` must emit *all* complete sentences in a fragment (`Vec<String>`, not `Option<String>`), handle `. ! ?` repeatedly, preserve remainder, and provide `finish()` to flush a trailing fragment at end of stream. This fixes the original example where one feed can contain two boundaries. Add Unicode and multiple-sentence tests. Bounded-channel backpressure is retained and documented.

# Replacement M9 - compression without data loss (Steps 141-165)

Keep eligibility and greedy clustering, but use `Uuid` consistently and move cosine similarity to one shared tested helper. Avoid `unwrap()` in `summarize_cluster`; return `Result` for an empty cluster. Enforce a maximum summary size; concatenation without a cap can create ever-growing records.

Fetch candidates and embeddings with explicit SQL. Backdate `created_at` directly in tests; custom TTL does not change age. For every cluster: create the summary, then mark all originals superseded in one SQLite transaction. Remove originals from the live vector index only after that transaction succeeds. If index deletion partially fails, rebuild/backfill the index from SQLite and return an error. A restart must reconstruct exactly the same active index.

Clarify the promise: originals remain in SQLite, so this improves active retrieval density but does **not** reduce database disk usage. Do not call it space saving unless an explicit archival/hard-delete policy is later added. Add provenance metadata (`compression.original_ids`, threshold, algorithm version, time range) and idempotency tests.

Do not refer to a nonexistent `MemoryStats`. Either add a fully specified `stats() -> Result<MemoryStats>` milestone (counts for active, expired, superseded, embeddings, compressed summaries) or remove Step 158.

# Replacement M10 - background maintenance lifecycle (Steps 166-180)

Do not spawn a self-referential worker inside `open()` and do not store a handle on an engine captured by the same task. Use an explicit controller:

```rust
pub struct MaintenanceHandle { cancel: CancellationToken, join: JoinHandle<()> }
impl MemoryEngine {
    pub fn start_maintenance(self: &Arc<Self>, config: MaintenanceConfig) -> MaintenanceHandle;
}
impl MaintenanceHandle { pub async fn shutdown(self) -> Result<()>; }
```

Add `tokio-util` for `CancellationToken` or use a Tokio watch channel. The task captures a `Weak<MemoryEngine>` so dropping the last application `Arc` allows exit. Use `MissedTickBehavior::Skip`; first tick should not unexpectedly run immediately unless documented. Purge hourly and compress daily with separate intervals, not a fragile tick counter. Purge must also delete IDs from the vector store (or rebuild active index) because custom backends may not mirror SQLite cascades.

Never try to catch Rust panics with ordinary `Result`. Either let the join error expose the panic, or wrap each iteration with `FutureExt::catch_unwind` only if the added complexity is justified. Tests use paused Tokio time (`#[tokio::test(start_paused = true)]`) rather than real sleeps. Test cancellation, engine drop, concurrent calls, missed ticks, error continuation, and no detached task.

# Replacement M11 - optional backend (Steps 181-195)

Do not invent VecLite's protocol. First pin a concrete VecLite repository/version and copy its authoritative API contract into an integration fixture. If no stable client/API exists, define M11 as a generic HTTP adapter example and do not market it as tested VecLite support.

Use `veclite = ["dep:reqwest"]`, not `veclite = ["reqwest"]`. Add `Serialize`/`Deserialize` to `VectorHit` only if the real wire format matches. Use `MemoliteError::VectorStore`, call `.error_for_status()`, set request timeouts, URL-encode path components, avoid leaking API keys in debug output, and map backend IDs/scores explicitly. Specify collection creation/dimension verification and backfill ownership. Integration tests are opt-in via environment variables and skip with a clear message when no service is configured; unit tests use a local mock server.

# Replacement M12 - release gate (Steps 196-210)

Write the README from the shipped API, add `ARCHITECTURE.md`, and document thread/concurrency behavior, durability boundaries, model download/cache behavior, privacy, database schema/migrations, score semantics, compression limitations, and optional-backend guarantees. Do not promise benchmarks or comparisons without recorded reproducible commands/results.

Final commands:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo doc --no-deps --all-features
cargo publish --dry-run
```

Also run default-feature builds/tests separately so optional code is truly optional. Run every example against a temporary database, verify a clean checkout/restart path, and test migration using a copy of the current pre-migration database. Add package metadata (`license`, `description`, `repository`, `readme`, `keywords`, `categories`) before `publish --dry-run`.

Do not tag or publish as an automatic build step. Step 209 becomes: "After the user reviews the diff, release notes, semver choice, and clean validation output, the user may explicitly authorize creating and pushing a tag." Version `0.2.0` is appropriate only if `0.1.0` has actually been released; otherwise choose the version from release history.

## Correct final build order

1. Cleanup and transactional Step-40 repair (foreign keys, validation, embed-before-write, atomic SQLite rows).
2. M3 vector-store contract, in-memory store, backfill, recall, synchronized delete/purge.
3. M4 ranked query/result API and filters.
4. M6 confidence enum and migration infrastructure.
5. M5 request/update API using confidence from day one.
6. M7 temporal behavior.
7. Concurrency refactor (`Mutex<Connection>` or DB actor) and `Send + Sync` gate.
8. M8 streaming with observable errors and explicit shutdown.
9. M9 compression with provenance, bounded summaries, and index recovery.
10. M10 explicit maintenance controller with cancellation.
11. M11 only after the real backend protocol is verified and pinned.
12. M12 documentation, compatibility, packaging, benchmarks, and user-authorized release.

This order keeps every intermediate commit compiling and prevents later concurrency/migration work from forcing a rewrite of the public API.
