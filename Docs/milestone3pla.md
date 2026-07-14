# Memolite — Milestone 3 Implementation Plan (Final Revised, End-to-End)

**Scope of M3:** `store()`, `get()`, `forget()`, `purge_expired()`, and a temporary-but-real
cosine-based `recall()` — layered on the Step-0 foundation (`VectorStore` trait,
`InMemoryVectorStore`, migrations, `MemoryEngine` struct + constructor). `RecallQuery`, ranking,
requests, confidence, streaming, compression, maintenance, and stats are all out of scope — they
are M4 and later. Nothing below is code; it is sequencing, decisions, and acceptance criteria.

**Grounded in:** the v6 master build plan, the `finalunderstanding.md` architecture doc, the actual
M3 patch diff already applied to the repo, the 32-point patch review against that diff, and a
follow-up round of 9 concrete refinements that were agreed and are now folded into the step
sequence below (not left as open questions). This is the single authoritative version — supersedes
the earlier draft phase plan.

**Non-negotiable invariant for the whole milestone:** filter eligibility → truncate to
`DEFAULT_RECALL_LIMIT` → bump access stats only for the truncated set → batch-refetch that same
set. Every recall-path step below exists to protect this ordering.

---

## PHASE 1 — Foundation Repair (must land before any M3 logic is trusted)

Goal: Step 0 itself has known sequencing/API defects (per the "shortcomings" review). Fix those
first, or M3 inherits broken invariants.

1. Re-verify the actual current `main` branch state file-by-file (`Cargo.toml`, `src/lib.rs`,
   `src/error.rs`, `src/memory.rs`, `src/engine.rs`, `src/embedder.rs`, `src/migrations.rs`,
   `src/recall.rs`, `src/vector_store/mod.rs`, `src/vector_store/in_memory.rs`) — do not assume the
   v6 doc's snippets are what's actually on disk; the applied M3 patch diff is the real baseline.
2. Confirm `lib.rs` registers only modules that physically exist right now (`error`, `embedder`,
   `memory`, `engine`, `vector_store`, `recall`, `migrations`). Do not add
   `confidence`/`streaming`/`compression`/`maintenance`/`stats`/`requests`/`ranking` yet — those are
   M4+.
3. Confirm `pub use memory::{Memory, MemoryType};` is present in `lib.rs`. If any v6-style edit
   removed it, restore it — this is a hard regression, not a style choice.
4. Confirm `src/migrations.rs` does **not** call `crate::confidence::repair_confidence_column(...)`.
   If it does, remove that call now; confidence migration is out of scope until M6. Keep only the
   baseline (memories/embeddings/indexes) migration active.
5. **New — orphan-embedding detection, decided now, not deferred.** On every `open()`, after
   `PRAGMA foreign_keys = ON` and after migrations run, execute `PRAGMA foreign_key_check` and fail
   `open()` with `Err(Corruption)` if it reports any violation. This is cheap, catches orphaned
   `embeddings` rows and any other FK drift, and closes the orphan-detection gap outright instead of
   deferring it to a later milestone.
6. Decide and document (in a code comment, not chat) the ID type for `VectorStore`: keep `Uuid`
   (already implemented in the repo) as canonical. This explicitly supersedes any v6-doc text
   implying string IDs — future HTTP-backend work targets `Uuid`.
7. In `in_memory.rs`, change `lock_read()`/`lock_write()` to return `MemoliteError::VectorStore(...)`
   instead of `MemoliteError::Internal(...)` on poison — align the error taxonomy:
   `Internal` = engine-owned locks (`conn`, `embedder`); `VectorStore` = backend-owned lock
   poisoning.
8. **Cosine hardening — fully specified, not left open.** Fix `InMemoryVectorStore::cosine()`:
   - Accumulate dot product and both norms in `f64`; only cast to `f32` at the very end.
   - Zero norm on either input vector → return `0.0` (not an error — this is a legitimate
     "no direction" case, already reachable via a validly-inserted all-zero vector).
   - `cosine()` must **not** trust that `validate_vector` already ran — if either input vector
     contains a non-finite component, `cosine()` itself returns `Err(VectorStore(...))`.
   - If both inputs are finite but the computed score is non-finite (the large-finite-value
     overflow case), also return `Err(VectorStore(...))` — a bad score must never masquerade as
     "no similarity" by silently falling back to `0.0`.
9. **`open_with_store` scoped correctly for M3 — not fully public yet.** Making a full public
   `open_with_store(path, store, backfill_policy)` now would freeze a constructor shape before
   `BackfillPolicy` has more than one real variant (`ReplaceAll`), which is premature commitment.
   Instead: `open_with_store_internal` becomes `pub(crate) async fn` (not private), exercised only
   through a same-crate test path (a `pub(crate)` helper plus in-crate test usage — not a `tests/`
   file reaching in via `#[path]`, which is the wrong tool). Full public `open_with_store` stays an
   M11 deliverable.
10. Re-audit every `Mutex`/`RwLock` guard acquisition in `engine.rs` for "drop before await" — this
    must hold for `conn`, `embedder`, and `vector_store` guards in every method that will exist
    after this phase (`store`, `get`, `forget`, `purge_expired`, `recall`).
11. Confirm `MEMORY_COLUMNS` matches the actual current 10-column schema (no `confidence` column
    yet — that's M6).
12. Confirm `row_to_memory` reads exactly those 10 columns in that exact order, with typed
    `?`-propagating conversions — no silent `.unwrap_or_default()` swallowing corrupt
    timestamps/UUIDs. This is a correctness bar for the whole milestone, not just M6.
13. Run `cargo build`. Must succeed with zero warnings about unused/missing items.
14. Run `cargo clippy --all-targets -- -D warnings`.
15. Run the existing `cargo test` suite and confirm everything currently passing still passes
    unmodified.
16. **Decide the single expiration-boundary convention for the whole crate, now:**
    `expires_at <= now` means "expired," everywhere — recall filtering, purge eligibility,
    reconciliation. Write this down as a one-line doc comment near `MemoryEngine` so `<` vs `<=`
    never drifts between methods again.
17. **Decide the single compensation/error-surfacing convention, now:** when a SQLite write
    succeeds but the paired vector-store write/delete fails, the method attempts best-effort
    reconciliation but **always surfaces the original error** as `Err` — reconciliation never
    silently upgrades a failure into `Ok`. Apply this uniformly to `forget()` and `purge_expired()`
    (previously inconsistent).
18. Add a named constant `DEFAULT_RECALL_LIMIT: usize = 10` in `recall.rs` (or `engine.rs`) — stop
    hard-coding `10` inline anywhere.
19. Confirm `candidate_pool_size()` is unchanged:
    `limit.saturating_mul(5).max(50).min(MAX_CANDIDATES)`.
20. Add doc comments on the `VectorStore` trait clarifying which invariants are the backend's
    responsibility (no duplicate IDs, finite scores, ≤k results) versus what the engine will
    defensively re-check. Decision for M3: the engine trusts an in-tree backend; defensive
    re-validation of backend output is not implemented yet — documented as a known limitation, not
    silently assumed.
21. **Decision, no code:** keep `vector_store: RwLock<Arc<dyn VectorStore>>` rather than simplifying
    to a bare `Arc<dyn VectorStore>`. Reasoning: Step 9 already makes `open_with_store` a real
    (crate-internal) capability in M3, so backend selection is already first-class — simplifying now
    would just mean re-adding the lock in M11. Document the decision, move on.
22. Write (do not implement yet) the full list of Phase-1 exit tests: build-green, clippy-green,
    poisoned-lock error-variant test for the vector store, cosine zero-norm/non-finite/overflow
    tests, module-registration sanity test, and a `PRAGMA foreign_key_check` open-time failure test
    placeholder.
23. Implement the Phase-1 exit tests from Step 22.
24. Run the full test suite; all green.
25. **New — Phase-1 architecture note.** Write a short doc comment (or `ARCHITECTURE.md` stub)
    stating the five conventions just locked in: expiration boundary (`<=`), compensation policy
    (always surface original error), `VectorStore` ID type (`Uuid`), orphan-detection mechanism
    (`PRAGMA foreign_key_check` at open), and `open_with_store` visibility (`pub(crate)` for M3,
    public in M11). Future milestones read this instead of re-deciding.
26. Re-run `cargo build && cargo clippy --all-targets -- -D warnings && cargo test`.
27. Confirm no test added in this phase depends on any M4+ type.
28. **Phase 1 checkpoint gate:** all of the above green in one clean run; `open_with_store_internal`
    is `pub(crate)`; error variants correct; cosine hardened per Step 8; expiration/compensation
    conventions written down; orphan detection active. Do not proceed to Phase 2 until this gate
    passes.

---

## PHASE 2 — Write Path: `store()`, `get()`, `forget()`, `purge_expired()`

Goal: get the durable-write and deletion paths fully correct and fully tested before touching
recall, since recall depends on all of these being trustworthy.

29. Implement/confirm `store()`: validate non-empty content, validate `importance ∈ [0.0, 1.0]`,
    generate UUID + timestamps, compute `expires_at` from `MemoryType::default_ttl()`.
30. Embed content via the locked `Embedder` — lock acquired, `embed()` called synchronously, guard
    dropped, *before* any SQLite transaction begins.
31. Serialize the vector with bincode.
32. Open one SQLite transaction that inserts both the `memories` row and the `embeddings` row
    together — this pairing must never be split; it's the invariant every later corruption check
    relies on.
33. Commit the transaction, drop the `conn` guard.
34. Clone the `Arc<dyn VectorStore>` from the `RwLock` read guard, drop the guard, then `.await`
    `store.insert(...)`.
35. On vector-store insert failure: compensate by deleting the just-inserted SQLite memory row
    (cascade removes the embedding row); if the compensating delete also fails, return a
    `CompensationFailed`-style error carrying both messages; otherwise return the original
    vector-store error (per Step 17's convention).
36. Return the new UUID as a string.
37. Implement/confirm `get(id: &str)`: parse UUID first (reject malformed input before touching
    SQLite), `SELECT {MEMORY_COLUMNS} FROM memories WHERE id = ?1`, return `Option<Memory>` via
    `row_to_memory`.
38. Implement/confirm `forget(id: &str)`: parse UUID **first**, with zero SQLite side effects on a
    malformed id (fixes the historical ordering bug where delete ran before validation).
39. `forget()` deletes from SQLite first (cascade handles the embedding row).
40. `forget()` then attempts vector-store `delete(uuid)`.
41. On vector-store delete failure: attempt `reconcile_vector_index(..., BackfillPolicy::ReplaceAll)`
    as best-effort repair, but per Step 17's convention, still return the *original* delete error
    unless reconciliation itself also fails, in which case return the compound
    `CompensationFailed`.
42. Confirm `forget()` on a syntactically valid but nonexistent id is a silent no-op `Ok(())` —
    SQLite `DELETE` affecting 0 rows is not an error.
43. Implement/confirm `purge_expired()`: select all memory IDs where
    `expires_at IS NOT NULL AND expires_at <= now` (per Step 16's boundary convention).
44. Delete those rows from SQLite (cascade handles embeddings).
45. For each deleted id, attempt vector-store `delete`.
46. Apply the *same* error-surfacing convention from Step 17 — `purge_expired()` must not silently
    return `Ok` on backend delete failure while `forget()` surfaces the error; both are consistent
    now.
47. Return the count of purged memories.
48. Update `reconcile_vector_index` (used by `open()`/restart) to select only **active** memories:
    `superseded_by IS NULL AND (expires_at IS NULL OR expires_at > now)` — stop indexing
    expired/superseded rows into the vector store on every restart. This directly reduces candidate-
    pool starvation in recall later.
49. Confirm the missing-embedding-row case (LEFT JOIN NULL) still raises `Corruption`, unaffected by
    the Step 48 filter — the filter narrows *which active rows* get reconciled; it does not change
    corruption detection.
50. **Moved earlier per refinement — build test fixtures now, before writing Phase 2/3 tests.**
    Build a deterministic test-only `VectorStore` double (fixed/controllable vectors, an optional
    forced-failure mode for insert/delete/search) and a trivial deterministic fake embedder
    (e.g. a hash-based fixed-length vector, no FastEmbed load). Phase 2 and Phase 3 tests default to
    these fixtures; only the semantic-ordering test and one restart smoke test are required to use
    the real FastEmbed model. This reduces suite runtime and flakiness tied to model availability.
51. Write store-path tests: round-trip store+get; empty-content rejected; out-of-range importance
    rejected; correct `expires_at` per `MemoryType`.
52. Write compensation tests: force a vector-store insert failure via the Step 50 fixture, confirm
    the SQLite row is gone afterward (or confirm `CompensationFailed` if the compensating delete is
    also forced to fail).
53. Write forget tests: malformed id → `Err`, zero side effects (verified by a subsequent raw-SQL
    row-count check); well-formed nonexistent id → `Ok(())`; existing id → removed from both SQLite
    and the vector store.
54. Write purge tests using a fixture where the memory **actually exists in both SQLite and the
    vector store** (not just a raw-SQL-inserted row) — closes the gap where an earlier purge test
    only proved SQLite deletion, not vector-store deletion.
55. Write a purge test asserting the exact `<=` boundary: a memory expiring at exactly "now"
    (backdated via raw SQL) is purged.
56. Write a restart test proving `reconcile_vector_index` does **not** re-index an expired or
    superseded memory — assert via the vector store's `contains()` directly after reopen, not
    indirectly through `recall()` (isolates reconciliation correctness from recall correctness).
57. Write an `open_with_store` (crate-internal, per Step 9) test: open with a custom
    `InMemoryVectorStore` of the wrong dimension → `Err(InvalidArgument)` before any reconciliation
    attempt.
58. Write a `PRAGMA foreign_key_check` test: construct an orphaned `embeddings` row (insert with FKs
    temporarily disabled), confirm `open()` fails with `Err(Corruption)` — closes Step 5's
    detection mechanism with an actual test now rather than only at the end of the milestone.
59. Run `cargo clippy --all-targets -- -D warnings` and `cargo test`; all green.
60. **Phase 2 checkpoint gate:** store/get/forget/purge fully correct, consistent error policy,
    restart reconciliation is active-only, orphan detection tested, fixtures in place, all Phase-2
    tests pass. Do not proceed to Phase 3 until this gate passes.

---

## PHASE 3 — Read Path: Temporary Real `recall()`

Goal: implement M3's real (temporary, pre-M4) semantic recall correctly, closing every correctness
gap the patch review raised, without yet building `RecallQuery`/ranking (that's M4).

61. Reject empty/whitespace-only query text with `InvalidArgument`, before embedding.
62. Embed the query text under the embedder lock; drop the guard before anything else.
63. Clone the `Arc<dyn VectorStore>` from the read lock; drop the guard before the `.await` search
    call.
64. Call `store.search(&query_vector, candidate_pool_size(DEFAULT_RECALL_LIMIT))` — request up to 50
    candidates, not 10; this over-fetch is intentional so filtering has room to work.
65. If `search` returns zero hits, return `Ok(vec![])` immediately.
66. Batch-load all candidate rows from SQLite in **one query** (`WHERE id IN (...)`) instead of one
    `query_row` per candidate — resolves the N+1 pattern, reduces lock/statement overhead.
67. **New — explicit rule for stale vector hits with no SQLite row.** A candidate id present in the
    vector store's search results but absent from the batch SQLite fetch (e.g. a prior delete that
    partially failed) is silently dropped from the candidate set for this call — never an error,
    never retried inline. Best-effort reconciliation for such drift is left as a fire-and-forget
    follow-up for a later milestone (M10's maintenance loop is the natural home); M3 does not
    attempt inline repair.
68. Apply eligibility filters on the batch: exclude `expires_at <= now`, exclude
    `superseded_by IS NOT NULL` (same boundary convention as Phase 1/2).
69. **Ordering rule, made explicit:** never assume the SQL `IN (...)` result preserves order. Build
    a `HashMap<Uuid, Memory>` from the batch result, then reconstruct the final ordered candidate
    list by iterating the original `hits` (sorted-by-similarity) sequence and looking up each id in
    the map.
70. **Truncate the ordered, filtered candidate list to `DEFAULT_RECALL_LIMIT` (10) here** — this is
    the fix for the "recall returns up to 50" defect. Truncation happens *before* any access-stat
    mutation, not after.
71. If the truncated list is empty, return `Ok(vec![])`.
72. **Collapsed access-stat/refetch step (replaces the earlier per-id loop).** After filtering and
    truncating to `DEFAULT_RECALL_LIMIT`, run the `access_count`/`last_accessed` `UPDATE` for
    exactly those ids in one transaction, then commit and drop the guard.
73. Immediately after commit, do **one batch** `SELECT ... WHERE id IN (...)` (same pattern as
    Step 66) for those same ids, re-applying the Step 68 eligibility filter — instead of looping
    `get()` per id. This removes the second N+1 and shrinks the failure surface to a single query.
    If a refetched row no longer qualifies (flipped to expired/superseded between the two passes),
    drop it from the result set silently rather than erroring the whole call.
74. Reconstruct the final ordered `Vec<Memory>` using the same `HashMap`-plus-original-order pattern
    as Step 69.
75. **Document the accepted tradeoff (single failure point now, not ten):** if the batch refetch in
    Step 73 itself errors, access stats were already durably bumped by Step 72's committed
    transaction, and the overall `recall()` call still returns `Err`. Full transactional
    exactly-once semantics across bump+refetch is not achievable without holding a transaction open
    across an await boundary, which conflicts with the drop-lock-before-await rule. Write this
    explicitly into a doc comment on `recall()`.
76. **Design note, no code:** since Step 48 already makes the vector index active-only after a
    restart, expired/superseded vectors should no longer occupy candidate slots post-reopen.
    Newly-expired-since-last-reconcile items can still occupy slots *within* a single session —
    accept this as a documented limitation for M3 (adaptive re-fetching is a legitimate M4+
    enhancement, not required for milestone acceptance).
77. **Design note, no code:** defer wrapping `embed()` in `spawn_blocking`. Document the requirement
    that callers run Memolite on a multithreaded Tokio runtime; only revisit `spawn_blocking` if a
    Phase-3 test proves executor starvation is a real problem.
78. Update the `recall()` doc comment to state, accurately, that it becomes a thin wrapper around
    `recall_query()` in M4, and that the exact M4 signature story (a possible `recall_query(RecallQuery)`
    alongside a retained string-based `recall()`, versus a rename) is undecided and will be settled
    in M4 — do not claim "callers of `recall()` won't need to change," since that overstates current
    certainty.
79. Write test: recall on an empty engine → `Ok(vec![])`.
80. Write test: whitespace-only query → `Err(InvalidArgument)`.
81. Write test: relevant memory ranks above unrelated memories (semantic ordering; uses the real
    embedder per Step 50's fixture policy; non-strict assertion since it depends on model behavior).
82. Write test: **recall never returns more than `DEFAULT_RECALL_LIMIT` (10) results**, using a
    fixture with more than 10 eligible relevant memories — directly tests Step 70.
83. **Split starvation test into two (per refinement — drop the flaky combined version):**
    - **Test A (deterministic, required):** seed 50+ expired/superseded memories plus 1 valid
      memory, close and reopen the engine, assert the valid one surfaces post-restart. This relies
      on Step 48's active-only reconciliation and is reliable.
    - **Test B (same-session starvation):** explicitly marked `#[ignore]` with a comment stating
      it's a known same-session limitation (expired/superseded vectors persist in the live index
      until restart or purge) — not asserted as a passing guarantee.
84. Write test: seed a vector-store entry with no matching SQLite row (via direct backend `insert()`
    bypassing `store()`), confirm `recall()` returns cleanly and excludes it — tests Step 67's
    stale-hit rule.
85. Write test: expired memories are excluded from recall (via raw-SQL backdating, same pattern as
    `purge_test.rs`).
86. Write test: superseded memories are excluded from recall.
87. Write test: expired/superseded candidates do **not** have `access_count` incremented — proves
    filtering happens before the Step 72 stat-bump transaction, not after.
88. Write test: recall increments `access_count` and the returned `Memory` reflects the bump
    immediately (no pre-increment staleness) — call twice, confirm `1` then `2`.
89. Write test: recall updates `last_accessed` forward from a backdated raw-SQL timestamp.
90. Write test: a vector-store `search()` failure (via the Step 50 forced-failure fixture)
    propagates as `Err` from `recall()` **without** mutating any access statistics — proves the
    Step 72 stat-bump transaction only runs after a successful search+filter phase.
91. Write test: a very large (but finite) synthetic vector pair that would overflow naive `f32`
    cosine accumulation — proves Step 8's `f64`-accumulation and overflow-detection fix.
92. Run `cargo clippy --all-targets -- -D warnings` and `cargo test`; all green.

---

## PHASE 4 — Malformed-Data Hardening, Full Regression, and Milestone Gate

Goal: close remaining data-corruption and process-hygiene gaps, then formally close M3.

93. Write test: invalid bincode blob stored in an `embeddings.vector` column → `open()`/
    reconciliation returns `Err(Corruption)` or an equivalent decode error, never a panic.
94. Write test: `embeddings.dimension` column value disagrees with the actual decoded vector length
    → `Err`.
95. Write test: decoded vector dimension disagrees with the live backend's configured dimension
    (e.g. after the Step 9 crate-internal `open_with_store` with a mismatched custom store) →
    `Err(InvalidArgument)` at open time, not a later silent failure.
96. Write test: a non-finite value (`NaN`/`inf`) persisted in a stored vector blob is rejected on
    reconciliation rather than silently indexed.
97. Convert all new integration tests to use a temp-directory/RAII cleanup helper (a small guard
    struct that removes the file in `Drop`) instead of a manual `std::fs::remove_file` at the end of
    each test — closes the "cleanup skipped on panic" hygiene gap.
98. Regenerate the full patch as a clean `git diff`/`git format-patch` output and verify with
    `git apply --check` before considering the change mergeable — a previous patch attempt was
    corrupt at the byte level and must not recur. Save the plan/patch files as plain ASCII-safe
    UTF-8 (straight quotes/hyphens, no smart-quote/em-dash artifacts) so they diff and paste cleanly.
99. Run `cargo fmt --check`.
100. Run `cargo clippy --all-targets --all-features -- -D warnings`.
101. Run `cargo test --all-targets` (full suite: unit tests in `vector_store/in_memory.rs`,
     `engine.rs`, plus integration tests in `tests/crud_test.rs`, `tests/purge_test.rs`,
     `tests/recall_test.rs`, `tests/restart_test.rs`).
102. Manually re-check every item from the original 32-point review, plus the 9 refinements, against
     the final code, and mark each resolved / deferred-with-rationale — do not silently drop any
     item; deferred items go into a "Known Limitations" note. Confirm no M4+ types (`RecallQuery`,
     `ranking`, `requests`, `confidence`, `streaming`, `compression`, `maintenance`, `stats`) are
     referenced anywhere in the M3 code path — M3 must compile and pass tests in complete isolation
     from later milestones.
103. **Milestone 3 exit gate:** `cargo fmt --check`, `cargo clippy -D warnings`, and
     `cargo test --all-targets` all green in one clean run from a fresh clone; patch applies cleanly
     with `git apply --check`; every review/refinement item is either resolved or explicitly
     documented as deferred (adaptive candidate fetching, same-session starvation, `spawn_blocking`
     for embedding, full public `open_with_store`). Only after this gate passes is M3 considered
     done and is M4 (`RecallQuery`/ranking) allowed to start.

---

## Known Limitations Carried Into M4 (write these into ARCHITECTURE.md verbatim)

- Same-session candidate starvation from freshly-expired/superseded vectors is possible until the
  next restart or purge; only cross-restart starvation is guaranteed fixed (Step 48, Test A of
  Step 83).
- Stale vector-store hits with no matching SQLite row are silently dropped per-call, not repaired
  inline (Step 67); a background reconciliation sweep is deferred to M10's maintenance loop.
- `open_with_store` is `pub(crate)` only in M3; the fully public constructor with a multi-variant
  `BackfillPolicy` is an M11 deliverable (Step 9).
- `embed()` runs synchronously on the calling task; `spawn_blocking` is not used. Callers must use a
  multithreaded Tokio runtime (Step 77).
- Recall's stat-bump-then-refetch sequence has one accepted failure window: if the batch refetch
  errors after the stat-bump transaction commits, stats are durably bumped even though the call
  returns `Err` (Step 75).