# Memolite — M3 Phase 4 ("Phase 1 & 2 Tests") Changelog — 18/07/26

This entry covers **Phase 4** of the M3 checklist: writing the test files that lock down Phase 1
(foundation repair) and Phase 2 (write path — `store()`/`get()`/`forget()`/`purge_expired()`),
building on the Phase 1/2 *code* and test fixtures already landed in the 16/07 pass. Phase 3
(`recall()` itself) and Phase 6 (`recall()`'s own test suite) were already implemented in an
earlier pass and were not touched here.

Unlike the last two passes, this one was actually run: `cargo test` was executed on the real repo
(Windows, real SQLite/ONNX toolchain) and the results are pasted in §3 below, including one real
bug it caught.

---

## 1. What we found before writing anything

Read the actual repo — not the planning docs — before touching it, since the three earlier
planning documents had drifted from what's actually on `main` (this was flagged in the 16/07
changelog too, and turned out to matter again here):

- `MemoryEngine::open_with_store_internal` is `pub(crate)`, not `pub`. Integration tests under
  `tests/` compile as a separate crate and cannot see `pub(crate)` items — so any test that needs
  to inject a custom `VectorStore` into a real engine **cannot live in `tests/`**, no matter what
  the plan's file list says.
- `InMemoryVectorStore`'s `data: RwLock<VectorMap>` field is private. Poisoning that lock (to test
  that poisoning surfaces as `MemoliteError::VectorStore`, not `Internal`) requires directly
  taking the write guard, which again only code inside `src/vector_store/in_memory.rs` itself can
  do.
- `recall()` is already fully implemented (an earlier pass added it), so tests that treat "is this
  memory still in the live vector index" as an external, observable question can use `recall()` as
  a proxy instead of needing direct access to the vector store.

Two consequences of the first two points, both deviations from the plan's literal file list,
called out explicitly rather than silently worked around:

- **`tests/poison_test.rs` was not created.** Lock-poisoning coverage already existed in
  `src/vector_store/in_memory.rs`'s own `#[cfg(test)]` module
  (`lock_poisoning_surfaces_as_vectorstore_error_not_internal`) from the 16/07 pass. Nothing to
  add.
- **`tests/compensation_test.rs` and `tests/open_with_store_test.rs` were not created as separate
  files.** Their content — a forced vector-store-delete failure proving `forget()` surfaces the
  *original* error even after a successful best-effort reconciliation, and a dimension-mismatched
  backend being rejected by `open_with_store_internal` before any reconciliation work — was added
  to `src/engine.rs`'s existing `#[cfg(test)] mod tests` block instead, right next to the sibling
  `store()`-compensation test that already lived there.

---

## 2. What was added, file by file

### 2.1 `tests/cosine_test.rs` — new

External, public-API-only version of the cosine hardening tests (zero-norm → `0.0`, non-finite
input/query rejected, large-finite vectors don't overflow via `f64` accumulation, wrong-dimension
query rejected). Uses only `InMemoryVectorStore`'s public trait methods, so — unlike the poison
test — this one *can* live outside the crate. 5 tests.

### 2.2 `tests/module_test.rs` — new

One sanity test reading `src/lib.rs`'s source text directly and asserting it registers exactly the
modules that exist as of M3 (`error`, `embedder`, `memory`, `engine`, `vector_store`, `recall`,
private `migrations`) and does **not** yet register any milestone-4+ module (`ranking`,
`requests`, `confidence`, `streaming`, `compression`, `maintenance`, `stats`).

### 2.3 `tests/orphan_test.rs` — new (and fixed once, see §3)

Constructs an orphaned `embeddings` row via a raw connection, then confirms
`MemoryEngine::open()` fails with `Err(Corruption)` — proving the `PRAGMA foreign_key_check`
added to `migrations.rs` in the 16/07 pass actually catches drift, not just compiles.

### 2.4 `tests/crud_test.rs` — appended

Three new tests: `store()` rejects empty/whitespace content, `store()` rejects importance outside
`[0.0, 1.0]` on both ends, and each `MemoryType` produces the correct `default_ttl()` (days for
Semantic/Episodic/Procedural, hours for Working), checked by comparing the returned `Memory`'s
`expires_at` against its `created_at`.

### 2.5 `tests/forget_test.rs` — new

Three tests: malformed id rejected with zero side effects on the real data, a well-formed but
missing id is a silent no-op, and a forgotten memory disappears from `recall()` results (not just
`get()`) — the closest an external test can get to proving the live vector index was actually
touched, since the vector store itself isn't public.

### 2.6 `tests/purge_test.rs` — appended

Two tests: a memory stored through the real API (so it exists in **both** SQLite and the live
vector index, unlike the existing raw-SQL-only test) is unrecallable after being backdated and
purged; and an exact-boundary test proving `expires_at == now` counts as expired (the `<=`
convention from the 16/07 pass), not just `expires_at < now`.

### 2.7 `tests/restart_test.rs` — filled in (was an empty placeholder on disk)

Two tests: a memory stored before a close/reopen cycle is still recallable afterward, and —
the one that actually exercises `reconcile_vector_index`'s active-only filter — a memory that was
backdated to already-expired *before* a restart does **not** get re-indexed into the live vector
store on reopen, even though its SQLite row is still there (nothing purged it). Uses `recall()` as
the external proxy for vector-index membership, same reasoning as §2.5.

### 2.8 `src/engine.rs` — appended to the existing `#[cfg(test)] mod tests` block

Two tests added in-crate for the `pub(crate)`-only reasons in §1:

- `forget_surfaces_the_original_error_when_vector_delete_fails` — a new `AlwaysFailsDelete` test
  double (sibling to the existing `AlwaysFailsInsert`) whose `delete()` always fails but whose
  `replace_all()` succeeds trivially, proving `forget()`'s compensation convention: a *successful*
  best-effort `reconcile_vector_index` repair never upgrades the real failure into `Ok` — the
  original `VectorStore` error is still what comes back, and the SQLite row is still gone
  regardless.
- `open_with_store_rejects_a_dimension_mismatched_backend` — a 7-dimensional
  `InMemoryVectorStore` handed to `open_with_store_internal` against an embedder that produces
  384-dimensional vectors, confirming the mismatch is rejected as `InvalidArgument` before any
  reconciliation work happens.

---

## 3. Test run — and the one real bug it found

`cargo test` was run on Windows against the real repo. Full unit-test suite (24 tests, all of
Phase 1–3's in-crate coverage) passed, as did `cosine_test`, `crud_test`, `forget_test`, and the
pre-existing `migration_test` and `module_test`. One failure:

```
---- orphaned_embedding_row_fails_open_with_corruption stdout ----
thread 'orphaned_embedding_row_fails_open_with_corruption' panicked at tests\orphan_test.rs:39:10:
inserting an orphaned embeddings row should succeed with FK enforcement off:
SqliteFailure(Error { code: ConstraintViolation, extended_code: 787 }, Some("FOREIGN KEY constraint failed"))
```

**Cause:** the test assumed a fresh `rusqlite::Connection` defaults to `foreign_keys = OFF` (true
on some SQLite builds, since `run_migrations()` is the only place that explicitly turns it on).
That assumption doesn't hold universally — the bundled SQLite build on this Windows machine
defaults foreign-key enforcement to **ON** per-connection, so the raw connection refused to create
the very orphan row the test needed, before `MemoryEngine::open()` ever got a chance to detect it.

**Fix:** stopped relying on the default and explicitly ran `PRAGMA foreign_keys = OFF` on the raw
connection before the insert. This is a platform-independent way to force the state the test
actually needs, regardless of what any given SQLite build happens to default to. Re-running after
the fix was not yet confirmed in this pass — flagged as the one remaining thing to verify (see
§4).

This is a genuine example of why "the plan said FKs are off by default" isn't something to trust
without checking, and a good illustration of why running tests for real (even just once, on one
machine) catches things line-by-line manual review can't.

---

## 4. What's explicitly still outstanding

- **Re-run `cargo test` after the `orphan_test.rs` fix** to confirm the pragma change actually
  resolves the failure (expected to pass, not yet re-confirmed against the real toolchain in this
  pass).
- **Phase 4's remaining items from the checklist beyond "Phase 1 & 2 Tests":** the malformed-data
  corruption tests (`tests/corruption_test.rs` — invalid bincode blob, dimension mismatches,
  non-finite persisted vectors), the `TempDb` RAII cleanup helper to replace manual
  `std::fs::remove_file` calls across every test file, and `ARCHITECTURE.md`.
- **Final milestone gate:** `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
  warnings`, and `cargo test --all-targets` all green in one clean run, plus a full re-check of
  every item from the original 32-point review against the final code.

---

## 5. Files touched this pass

1. `tests/cosine_test.rs` — **new**, 5 tests.
2. `tests/module_test.rs` — **new**, 1 test.
3. `tests/orphan_test.rs` — **new**, 1 test (fixed once — see §3).
4. `tests/crud_test.rs` — **appended**, 3 tests.
5. `tests/forget_test.rs` — **new**, 3 tests.
6. `tests/purge_test.rs` — **appended**, 2 tests.
7. `tests/restart_test.rs` — **filled in** (was empty), 2 tests.
8. `src/engine.rs` — **appended** to the existing in-crate test module, 2 tests
   (`forget_surfaces_the_original_error_when_vector_delete_fails`,
   `open_with_store_rejects_a_dimension_mismatched_backend`).
9. `Changelogs/18july.md` — this document.

**Explicitly not created, and correctly so, per §1:** `tests/poison_test.rs`,
`tests/compensation_test.rs`, `tests/open_with_store_test.rs` — their required coverage exists
in-crate instead, for reasons the plan's own file list didn't account for (`pub(crate)` and
private-field visibility).