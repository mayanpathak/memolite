# Milestone 5 — Extensible Storage API (`StoreRequest`, `MemoryUpdate`, `update()`)

**Status:** Complete. `cargo build` and `cargo test` both green (55/55 lib tests + all
integration test files passing, per verified local run).

## Summary

M3/M4 exposed exactly one way to create a memory: `store(content, memory_type, importance)`,
which always used `ExpiryPolicy`-equivalent type-default TTL and empty metadata, and exactly one
way to remove one: `forget(id)`. There was no way to edit a memory's content, importance, type,
expiry, or metadata after creation, and no way to store a memory with custom expiry or metadata
at all.

M5 replaces the single write path with a richer request/update model, adds `update()` as a new
first-class operation, and — during implementation review — closes a real correctness gap in
how updates interact with the supersession chain under repeated or concurrent calls.

---

## Added

### `src/requests.rs` (new module)

- **`ExpiryPolicy`** — an enum describing how a stored memory's `expires_at` should be computed:
  - `TypeDefault` — use `MemoryType::default_ttl()`, the exact behavior `store()` always had.
  - `Custom(chrono::Duration)` — expire exactly `Duration` after creation. Must be positive;
    `Custom(d)` where `d <= Duration::zero()` is rejected by `store_with_options` before any
    embedding or database work happens.
  - `Never` — `expires_at` is persisted as `NULL`; the memory never expires.
- **`StoreRequest`** — a complete description of a memory to store: `content`, `memory_type`,
  `importance`, `expiry` (defaults to `TypeDefault`), `metadata` (defaults to empty). Builder
  methods `.expiry(...)` and `.metadata(...)`. `StoreRequest::new(...)` reproduces `store()`'s
  original defaults exactly, so existing call sites are unaffected.
- **`MemoryUpdate`** — a partial update descriptor. Every field is `Option<T>`; `None` means
  "leave unchanged." Fields: `new_content`, `new_importance`, `new_metadata`, `new_memory_type`,
  `new_expiry`. No `id` field — the target id is always passed as `update()`'s first argument,
  not embedded in the update struct.
- Unit tests for `StoreRequest` defaults/builders and `MemoryUpdate::default()`.

### `MemoryEngine::store_with_options(request: StoreRequest) -> Result<String>`

New public method. Stores a memory with full control over expiry and metadata. Internally the
single writer of a memory+embedding pair (`store_with_options_id`) — validates content/importance/
expiry, embeds the content, writes both the `memories` and `embeddings` rows in one SQLite
transaction, then inserts into the live vector index with the **request's actual metadata**
(previously, pre-M5, the vector store always received an empty `HashMap` regardless of what was
conceptually "the memory's metadata" — this is now consistent between SQLite and the vector
index). On vector-store insert failure, the SQLite rows are compensated away (deleted) using the
same rollback shape `store()` always used; a failure of the rollback itself surfaces as
`CompensationFailed` with both error messages preserved.

### `MemoryEngine::update(id: &str, update: MemoryUpdate) -> Result<String>`

New public method. Never mutates a memory row in place. Instead:
1. Rejects a malformed id (`InvalidUuid`) or nonexistent id (`NotFound`) up front, before any
   work.
2. **Rejects updating an already-superseded memory** (`InvalidArgument`) up front, before any
   embedding or storage work — see "Fixed" below.
3. Rejects reviving an expired memory unless the caller explicitly supplies `new_expiry`
   (`InvalidArgument`) — a memory can never silently come back to life through an unrelated field
   edit (e.g. changing only `importance` on an expired memory does not un-expire it).
4. Merges `old` + `update` into a new `StoreRequest`, preserving the original's remaining TTL (or
   `Never`) unless `new_expiry` overrides it, and preserving metadata/content/importance/type for
   any field left as `None`.
5. Creates the new memory via `store_with_options_id` (the single write path).
6. Links the old row to the new one via `superseded_by` (see `mark_superseded` below). If linking
   fails, the newly created memory is rolled back from both SQLite and the vector store rather
   than left as an orphaned, unlinked duplicate — same compensation shape as
   `store_with_options_id`'s own failure path.

Returns the new memory's id as a string. The old memory is never deleted; it remains queryable via
`recall_query(...).include_superseded(true)`.

---

## Fixed

### Supersession race / double-supersede (found during implementation review, fixed before merge)

The original `mark_superseded` implementation was:

```rust
conn.execute(
    "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
    params![new_id, old_id.to_string()],
)?;
```

This had no guard against a row that was *already* superseded, which created two related bugs:

1. **Repeated update.** Calling `update()` a second time on an id that had already been superseded
   by an earlier `update()` call would silently succeed and overwrite `superseded_by`, breaking
   the one-hop chain invariant (two different "next" memories could each believe they were the
   sole successor, with only the last write actually recorded).
2. **Concurrent update.** Two `update()` calls racing on the same source id would both
   independently create and fully commit a new memory (in SQLite and the vector store), then race
   on `mark_superseded`. Last write wins silently — the losing caller's new memory ends up fully
   persisted and recallable, but with nothing pointing to it as "the current version," and no
   error is returned to the caller that lost the race.

**Fix**, applied in two layers:

- `mark_superseded`'s `UPDATE` is now conditioned on `superseded_by IS NULL`:
  ```rust
  "UPDATE memories SET superseded_by = ?1 WHERE id = ?2 AND superseded_by IS NULL"
  ```
  and `affected == 0` is disambiguated into two distinct errors: `NotFound` if the row doesn't
  exist at all, `InvalidArgument` if it exists but was already superseded (the race case). This is
  the authoritative, atomically-correct guard — only SQLite can adjudicate a genuine race.
- `update()` additionally checks `old.superseded_by.is_some()` up front, before doing any
  embedding or storage work, so the common (non-racing) mistake of calling `update()` twice fails
  fast and cheaply rather than doing wasted work before the DB-level guard catches it.

---

## Changed

- **`store()`** is now a thin wrapper: `store_with_options(StoreRequest::new(content, memory_type,
  importance))`. Public signature and observable behavior for existing callers are unchanged.
- **`src/lib.rs`** registers `pub mod requests;` and re-exports `ExpiryPolicy`, `MemoryUpdate`,
  `StoreRequest` from the crate root, following the same pattern M4 used for `ranking` /
  `RecallQuery`/`RecallItem`/`RecallResult`. `memory` and `ranking` remain registered exactly as
  they were; no module was removed or deferred.
- **`tests/module_test.rs`** updated: the M4-era assertion that `requests` must *not* be
  registered yet is now inverted (it must be registered, with a file on disk, and its types
  re-exported) — mirroring how `ranking` made the same transition at the M3→M4 boundary. Test
  renamed `lib_rs_registers_only_the_modules_that_exist_in_m4` → `..._in_m5` and its doc comment
  updated to explain the boundary move, so the same pattern is easy to repeat at M6.

---

## Test coverage added this milestone

All in `src/engine.rs`'s `#[cfg(test)]` module unless noted:

- `store_with_options_persists_metadata_in_both_sqlite_and_vector_store` — metadata parity fix,
  verified via a `metadata_equals` recall filter actually matching.
- `store_with_options_rejects_non_positive_custom_expiry` — both zero and negative durations.
- `store_with_options_never_expiry_leaves_expires_at_null`.
- `update_content_only_preserves_other_fields` — importance/type/metadata untouched, and the
  original row's `superseded_by` correctly points at the new id.
- `update_preserves_never_expiry`.
- `update_rejects_reviving_an_expired_memory_without_explicit_new_expiry` — both the rejection and
  the explicit-revival success path in one test.
- `update_on_a_nonexistent_id_is_not_found`.
- `update_on_a_malformed_id_is_invalid_uuid`.
- `update_on_an_already_superseded_memory_is_rejected` — the double-supersede fix, exercised
  through the public `update()` API, including confirming the original chain link is undisturbed
  and that updating the *current* version afterward still works.
- `mark_superseded_rejects_a_second_call_against_the_same_source_id` — direct unit test of the
  atomic guard itself (simulates the race by calling `mark_superseded` twice against the same
  source id with two different targets).
- `mark_superseded_on_a_nonexistent_id_is_not_found`.
- `include_superseded_reveals_the_original_after_update` — end-to-end: default recall hides the
  superseded original, `include_superseded(true)` reveals it.
- `src/requests.rs`: `store_request_new_uses_type_default_expiry_and_empty_metadata`,
  `store_request_builders_override_defaults`, `memory_update_default_is_all_none`.

All pre-existing M3/M4 tests (store/forget/recall/corruption/compensation/dimension-mismatch,
`recall_query` filters, ranking, vector-store validation) are unchanged and passing.

**Confirmed by local run:** `cargo build` succeeds; `cargo test` reports 55/55 passing in the lib
target, plus all integration test files (`compression_test`, `corruption_test`, `cosine_test`,
`crud_test`, `forget_test`, `migration_test`, `module_test`) green.

---

## Known gaps carried forward (not regressions — explicitly out of scope for M5)

- **`purge_expired()` has no test coverage added this milestone.** It was implemented at Step 0
  and is unchanged by M5, but no test in the current suite exercises it directly (expired-vs-
  healthy discrimination, vector-index cleanup, zero-expired no-op). Flagged for a follow-up pass.
- **No end-to-end multi-step lifecycle test** (store → recall → update → recall → forget →
  recall) exists yet; current tests verify each operation's own effect in isolation.
- **No concurrent-caller test** (e.g. multiple `store()`/`update()` calls fired concurrently via
  `tokio::join!`) exists yet, despite `MemoryEngine` being `Send + Sync` and designed to sit behind
  an `Arc` for exactly that use case.
- **`update()`'s remaining-TTL calculation has a small, harmless timing drift**: the "remaining
  time" snapshot is taken once at the top of `update()`, but the new memory's `created_at` (used as
  the base for `Custom(remaining)`) is a later `Utc::now()` call inside `store_with_options_id`
  (after the embed call completes). The new expiry ends up a few milliseconds later than
  mathematically exact. Not corrected in M5; noted for awareness, not a functional bug.

These are documented candidates for the next test-writing pass, not deferred to a future
milestone's *implementation* — the M5 code itself is complete and correct as shipped.