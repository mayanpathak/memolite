# 18th July — Milestone 4: Ranking + `recall_query()`

**Status: checkpoint reached.** `cargo build && cargo test` is green — 40
unit tests plus all integration test files (`compression_test` [0, not yet
populated], `corruption_test`, `cosine_test`, `crud_test`, `forget_test`,
`migration_test`, `module_test`) pass with zero failures.

## Summary

M3 shipped a temporary, self-contained `recall()` that did its own
embed → search → batch-fetch-active → bump → refetch, with no ranking
beyond raw cosine similarity and no way to filter by type, importance,
metadata, or superseded/expired inclusion.

M4 replaces that with a real ranking layer and a structured query API:
`recall()` is now a one-line wrapper around the new `recall_query()`, which
is the single implementation of the bump-and-refetch pattern going forward.

## New files

- **`src/ranking.rs`** — pure, dependency-free scoring functions:
  - `decay_half_life_days(MemoryType) -> f64` — per-type half-life (Working
    shortest, Procedural longest).
  - `recency_factor(days_since_access, MemoryType) -> f32` — exponential
    decay, clamped so negative `days_since_access` never exceeds `1.0`.
  - `reinforcement_factor(access_count) -> f32` — logarithmic boost, `1.0`
    at zero accesses, diminishing returns.
  - `final_score(similarity, importance, recency, reinforcement,
    confidence_weight) -> f32` — the product of all five factors.
    `confidence_weight` is accepted as a parameter now so M6 only has to
    change what value gets passed in, not this function's signature.
  - 7 unit tests, all passing.

## Modified files

- **`src/recall.rs`** — Step 0's `MAX_CANDIDATES` / `DEFAULT_RECALL_LIMIT` /
  `candidate_pool_size` are unchanged. Added:
  - `RecallQuery` — builder-style query struct (`query_text`, `limit`,
    `min_importance`, `memory_types`, `include_superseded`,
    `include_expired`, `metadata_equals`), defaulting to
    `DEFAULT_RECALL_LIMIT`, no filters, superseded/expired excluded.
  - `RecallItem` — one ranked result: `memory`, `similarity`, `score`.
  - `RecallResult` — `{ items: Vec<RecallItem> }`.
  - 3 new unit tests (defaults, builder methods, pool-size bounds).

- **`src/engine.rs`**
  - `recall(&self, query: &str) -> Result<Vec<Memory>>` is now a thin
    wrapper: `recall_query(RecallQuery::new(query))`, mapped down to
    `Vec<Memory>`.
  - Added `recall_query(&self, RecallQuery) -> Result<RecallResult>` — the
    real implementation: validate → embed → vector search → unfiltered
    batch fetch → filter in Rust (importance, type, superseded, expired,
    metadata) → score via `ranking::final_score` → sort/truncate →
    batch-bump access stats in one transaction → batch-refetch so returned
    memories reflect the bump this call made.
  - Removed `fetch_active_memories` (baked-in active-only filter, wrong
    shape for `RecallQuery`'s opt-in `include_superseded`/`include_expired`).
    Replaced with `fetch_memories_by_ids` (no filtering — every filter now
    lives in `recall_query()`'s Rust loop, applied uniformly alongside the
    query's other conditions).
  - `confidence_weight` is hardcoded to `1.0_f32` with a `// M6 replaces
    this one line` marker. No M5/M6/M8+ types referenced anywhere.
  - 6 new tests: type-filter exclusion, `limit(0)` → `Err`, NaN
    `min_importance` → `Err`, access-count-already-bumped assertion,
    `recall()`/`recall_query()` parity on a plain query, `metadata_equals`
    filtering, and `include_expired` default-hides/override-reveals
    (backdated via raw test SQL, same pattern as the existing corruption
    test).

- **`src/lib.rs`**
  - Added `pub mod ranking;`.
  - Added `pub use recall::{RecallItem, RecallQuery, RecallResult};` to the
    existing re-export block.
  - Updated the trailing comment: `ranking` moved out of the "not yet
    registered" list (M4 gave it real content); `requests`, `confidence`,
    `streaming`, `compression`, `maintenance`, `stats` remain listed against
    their own milestones (M5, M6, M8, M9, M10, M9.5).

- **`tests/module_test.rs`**
  - Renamed the guard test from `..._exists_in_m3` to `..._exists_in_m4`.
  - `ranking` moved from the "must not be registered" list into the "must
    be registered with a real file on disk" list, alongside `error`,
    `embedder`, `memory`, `engine`, `vector_store`, `recall`.
  - Added an explicit check that `RecallQuery`, `RecallItem`, and
    `RecallResult` are re-exported by name from `lib.rs`.
  - `requests`, `confidence`, `streaming`, `compression`, `maintenance`,
    `stats` remain on the forbidden list until their own milestones.

## Fixed during this milestone (not architectural — process issues)

1. First `cargo test` run failed `module_test`'s M3-era guard, which
   correctly rejected the newly-added `pub mod ranking;` since that guard
   hadn't been advanced to the M4 baseline yet. Fixed by updating
   `module_test.rs` to assert the M4 module set instead of the M3 one.
2. Second `cargo test` run failed the updated `module_test`'s new
   `RecallQuery`-re-export assertion: `ranking` had been added to
   `lib.rs`, but the `pub use recall::{RecallItem, RecallQuery,
   RecallResult};` line had not. Fixed by adding that line.

Both were caught by the milestone-boundary test itself working as intended,
not by a runtime bug — no test that exercises `recall_query()`'s actual
behavior (scoring, filtering, bump/refetch) ever failed.

## Explicitly out of scope for M4 (deferred to their own milestones)

- `StoreRequest` / `MemoryUpdate` / `ExpiryPolicy` / `update()` — M5.
- `ConfidenceLevel`, the `confidence` column/migration, and swapping
  `recall_query()`'s `confidence_weight` stub for
  `memory.confidence.weight()` — M6.
- `created_after` / `created_before` / `only_stale` on `RecallQuery` and
  the standalone temporal-query methods — M7.
- Streaming ingestion, compression, maintenance, stats, generic-HTTP
  backend — M8–M11.

## Checkpoint commands

```powershell
cargo build
cargo test
cargo clippy --all-targets -- -D warnings   # zero dead-code warnings expected
                                             # (confirms fetch_active_memories
                                             # was fully replaced, not orphaned)
```

All green as of this entry.     