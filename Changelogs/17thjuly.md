# Memolite — M3 Phase 4 ("Phase 1 & 2 Tests") Changelog — 17/07/26

This entry covers **Phase 4** of the M3 checklist: writing the test files that lock down Phase 1
(foundation repair) and Phase 2 (write path — `store()`/`get()`/`forget()`/`purge_expired()`),
building on the Phase 1/2 *code* and test fixtures already landed in the 16/07 pass. Phase 3
(`recall()` itself) and Phase 6 (`recall()`'s own test suite) were already implemented in an
earlier pass and were not touched here.

Unlike the last two passes, this one was actually run: `cargo test` was executed on the real repo
(Windows, real SQLite/ONNX toolchain) and the results are pasted in §3 below, including one real
bug it caught.

A second pass, covering **Phase 7** (corruption tests + hygiene), was added later the same day —
see §6 onward.

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

## 4. What's explicitly still outstanding (as of the Phase 4 pass)

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

## 5. Files touched this pass (Phase 4)

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

---

---

## 6. Addendum (same day) — Phase 7: Corruption Tests + Hygiene

Closes the two items §4 explicitly left outstanding from the checklist: the malformed-data
corruption suite, and the `TempDb` RAII cleanup helper (`ARCHITECTURE.md` and the final gate run
are still open — see §9).

This addendum's changes were reviewed against the real file-by-file contents of `src/engine.rs`,
`src/error.rs`, `src/migrations.rs`, `src/vector_store/mod.rs`, `src/vector_store/in_memory.rs`,
and `tests/common/mod.rs` on `main` before writing anything, same discipline as §1 — and it
surfaced the same category of plan-vs-repo drift the Phase 4 pass did, on the checklist's Step 41.

### 6.1 What we found before writing anything

- Same `pub(crate)` constraint from §1 applies again, this time to one specific corruption
  scenario: the checklist's Step 41 ("dimension vs backend mismatch → `Err(InvalidArgument)`")
  is exactly the check `open_with_store_internal` does when a caller-supplied `VectorStore`'s
  `dimension()` disagrees with the embedder's real dimension. `MemoryEngine::open()` — the only
  constructor a `tests/` file can reach — always builds its own `InMemoryVectorStore` sized to
  match the embedder, so there is no public path to that branch at all.
- That exact scenario is already covered, and has been since the Phase 4 pass (§2.8): the
  in-crate test `open_with_store_rejects_a_dimension_mismatched_backend` in
  `src/engine.rs`. Nothing new needed there.
- The other three checklist items (invalid bincode blob, a row's `dimension` column disagreeing
  with its own decoded vector length, and a non-finite float smuggled into an otherwise
  well-formed blob) are all real, externally-triggerable states — `store()` can never produce any
  of them itself, but a raw `rusqlite::Connection` hand-editing a row `store()` already wrote can,
  exactly like `tests/orphan_test.rs` and `tests/restart_test.rs` already do for their own
  scenarios. All three go through `MemoryEngine::open()` → `reconcile_vector_index()`, and none of
  them requires touching a `pub(crate)` item.

**Consequence, called out the same way §1 called out its two omissions:** the checklist's Step 41
is not a separate test in `tests/corruption_test.rs`. Duplicating it there wasn't possible (it
won't compile against `pub(crate)` from outside the crate) and duplicating it in-crate would just
be the same test twice under a different name. `tests/corruption_test.rs`'s own doc comment says
this explicitly, pointing at the existing unit test by name, so the omission is documented at the
point someone would actually go looking for it — not just here.

### 6.2 What was added, file by file

#### `tests/common/mod.rs` — appended

Added `TempDb`: an RAII guard that reserves a unique path under `std::env::temp_dir()` on
construction (creating nothing on disk yet) and removes that file, unconditionally, when dropped.
This is what every existing integration test's tail-end `std::fs::remove_file(&path).expect(...)`
call was missing — that line only ever ran on a clean, non-panicking exit, so any test that failed
an `assert!`/`expect()` earlier in its body left its `.db` file behind in the OS temp dir forever.
`Drop::drop` runs regardless of *why* the guard's scope ended, panic included, which a manual tail
call structurally cannot do. Added one self-test (`temp_db_removes_its_file_on_drop`) proving the
guard actually deletes the file, not just that it compiles. Module doc comment updated to describe
three fixtures instead of two.

#### `tests/corruption_test.rs` — new, 3 tests

- `invalid_bincode_blob_fails_open_without_panicking` — overwrites `embeddings.vector` with 6
  garbage bytes (too short to even hold bincode's own 8-byte length prefix, so this is guaranteed
  to be a decode error rather than an accidental, nonsensical-but-valid decode) → asserts
  `Err(MemoliteError::EmbeddingDecode(_))`. The test passing at all — reaching the assertion
  instead of aborting on a panic — is itself part of what proves "no panic."
- `dimension_column_disagreeing_with_decoded_vector_length_is_corruption` — leaves the vector blob
  itself untouched (still decodes to a real, finite vector) but bumps the row's own `dimension`
  column by one → asserts `Err(MemoliteError::Corruption(_))`, exercising the exact
  `vector.len() != stored_dim` self-consistency check in `reconcile_vector_index`, one layer
  before anything reaches the backend.
- `non_finite_value_in_stored_vector_is_rejected_on_reconciliation` — reads the real embedder's
  dimension via the already-public `MemoryEngine::dimension()`, writes a correctly-sized vector
  with one component replaced by `f32::NAN`, re-serializes it with `bincode::serialize` the same
  way `store()` does → asserts the reopen is `Err`. This one is *not* caught by
  `reconcile_vector_index` itself (it has no finiteness check of its own); it's caught one layer
  down, by `InMemoryVectorStore::replace_all`'s existing `validate_vector` call — which is exactly
  the documented "engine trusts the backend to validate" contract on the `VectorStore` trait, doing
  its job.

All three use `TempDb` from the moment the file was created — there was never a manual
`temp_db_path`/`remove_file` version of this file to migrate away from.

#### `tests/crud_test.rs`, `tests/purge_test.rs`, `tests/forget_test.rs`, `tests/restart_test.rs` — edited

Every test that opened a real file on disk now does `let db = TempDb::new("...")` and
`MemoryEngine::open(db.path())` instead of the old `temp_db_path("...")` helper, and the trailing
`drop(engine); std::fs::remove_file(&path).expect(...)` two-liner at the end of each test body is
gone — Rust's own drop order (locals drop in reverse declaration order, so `db` — declared first —
drops *after* `engine`) already guarantees the engine's file handle is released before `TempDb`
tries to remove the file, without an explicit `drop(engine)` needing to be added anywhere it wasn't
already there for some other reason. `forget_test.rs`'s two tests that open the engine against
`:memory:` were left untouched — there's no file for `TempDb` to manage in either case.

No behavior changed in any of these four files; every assertion is identical to before. This is a
pure mechanical migration, same as the plan's own framing of it ("hygiene," not new coverage).

### 6.3 Test run

Not executed this addendum — no Rust toolchain (`cargo`) was available in the sandbox this pass
was written in, unlike the Phase 4 pass's real Windows run in §3. Every choice above was checked
by hand against the actual code paths in `src/engine.rs`, `src/error.rs`, and
`src/vector_store/{mod.rs,in_memory.rs}` (which error variant each corrupted state actually
produces, in what order the checks run, that `bincode`, `chrono`, `rusqlite`, and `uuid` are all
already `[dependencies]` and therefore usable from `tests/` without a `Cargo.toml` change) rather
than by running it. **Running `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
`cargo fmt --check` against this addendum's five files on a real toolchain is the first thing the
next pass should do** — same category of risk §3 already demonstrated once this milestone (a
plan-level assumption that held everywhere reviewed by hand except the one place a real run caught
it).

### 6.4 What's explicitly still outstanding (updated)

- **Run the real toolchain against this addendum** (see §6.3) — not yet done.
- **`ARCHITECTURE.md`** — still not written. Should capture the five Phase-1 conventions (
  expiration boundary, compensation policy, `VectorStore` ID type, orphan-detection mechanism,
  `open_with_store` visibility) plus the "Known Limitations" list, per the original plan.
- **Final milestone gate** — unchanged from §4: `cargo fmt --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-targets` all
  green in one clean run, plus the full re-check of every item from the original 32-point review
  (and the 9 follow-up refinements) against the final code, with each marked resolved or
  explicitly deferred.
- **Re-run confirmation from §3/§4** — the `orphan_test.rs` FK-pragma fix from the Phase 4 pass
  still has not been re-confirmed against a real toolchain run; this addendum did not touch that
  file, so the same open item carries forward unchanged.

### 6.5 Files touched this addendum

1. `tests/common/mod.rs` — **appended**: `TempDb` struct + `Drop` impl + 1 self-test; module doc
   comment updated.
2. `tests/corruption_test.rs` — **new**, 3 tests.
3. `tests/crud_test.rs` — **edited**: `TempDb` in place of `temp_db_path`/manual `remove_file`,
   no behavior change.
4. `tests/purge_test.rs` — **edited**: same mechanical change, no behavior change.
5. `tests/forget_test.rs` — **edited**: same mechanical change (one test only — the other two use
   `:memory:`), no behavior change.
6. `tests/restart_test.rs` — **edited**: same mechanical change, no behavior change.
7. `Changelogs/18july.md` — this addendum.

**Explicitly not created, and correctly so, per §6.1:** a fourth `tests/corruption_test.rs` case
for the checklist's Step 41 — its coverage already exists in-crate
(`open_with_store_rejects_a_dimension_mismatched_backend`, added in the Phase 4 pass, §2.8) for
the same `pub(crate)`-visibility reason §1 already established this milestone.