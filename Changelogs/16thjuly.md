# Memolite — M3 Phase 1 & Phase 2 (Step 8) Changelog — 16/07/26

This entry covers two pieces of work done against the M3 plan, layered on top of the Step 0
foundation from the 13/07 changelog: **Phase 1 (Foundation Repair)**, and the first step of
**Phase 2 (Test Fixtures)**. Nothing in `store()`, `get()`, `forget()`, `purge_expired()`, or
`recall()` was touched yet — that's Phase 2's remaining steps and Phase 3, still to come.

---

## 1. What we found before changing anything

Before writing any code, we cloned the actual repo and read every file the plan claimed needed
changes, instead of trusting the plan's own snippets (three separate planning documents existed
for this milestone and disagreed with each other in places, plus with the real code). That check
turned up something worth recording: **several "Phase 1" requirements were already satisfied by
the Step 0 work**, and touching those files again would have been pure churn.

Specifically, **no changes were needed** to:

- **`src/error.rs`** — `MemoliteError::VectorStore(String)` already existed, along with every
  other variant the plan asked for (`Database`, `InvalidMemoryType`, `InvalidMetadata`,
  `InvalidUuid`, `InvalidArgument`, `Corruption`, `CompensationFailed`).
- **`src/lib.rs`** — module registration was already exactly right (`error`, `embedder`, `memory`,
  `engine`, `vector_store`, `recall`, private `migrations`), and `pub use memory::{Memory,
  MemoryType}` was already present.
- **`open_with_store_internal`** in `src/engine.rs` — already `pub(crate)`, not private.
- **`MEMORY_COLUMNS`** — already exactly the 10 real columns, no premature `confidence` column.
- **`row_to_memory`** — already used strict, typed, `?`-propagating conversions for every column;
  no `.unwrap_or_default()` silently swallowing a corrupt timestamp or UUID.

Knowing this up front meant Phase 1's actual footprint is small: five files, mostly additive.

---

## 2. What changed, file by file

### 2.1 `src/vector_store/in_memory.rs` — cosine similarity can no longer lie to you

**Layman version:** The "how similar are these two memories" calculation used to just trust its
inputs completely. If a memory's embedding somehow contained a broken number (infinity, "not a
number"), the old code would silently produce a nonsense similarity score instead of flagging the
problem — and a very large-but-valid pair of numbers could quietly overflow into a wrong answer
too. Now the calculation checks its own inputs and its own output, and refuses to hand back a
lie.

**Technical version:** `cosine()` changed from `fn cosine(a: &[f32], b: &[f32]) -> f32` to
`fn cosine(a: &[f32], b: &[f32]) -> Result<f32>`. It now:
- Accumulates the dot product and both norms in `f64`, only casting to `f32` at the very end, so
  large-but-finite inputs can't silently overflow `f32` mid-calculation.
- Checks every component of both input vectors for `is_finite()` up front — it does **not**
  assume `validate_vector` already ran, even though every current call site does run it first.
- Still returns `Ok(0.0)` for a zero-norm vector (a legitimate "no direction" case), rather than
  treating it as an error.
- Returns `Err(MemoliteError::VectorStore(...))` if the *computed* similarity itself comes out
  non-finite, so an overflow can never masquerade as "these are unrelated" (score `0.0`).

`search()` was updated to match — it now `.map()`s each candidate through the fallible `cosine()`
and `.collect::<Result<Vec<VectorHit>>>()?`s the whole batch, so one bad stored vector fails the
search loudly instead of one bad `VectorHit` sneaking into the sorted results.

**Also fixed:** `lock_read()`/`lock_write()` were returning `MemoliteError::Internal(...)` on a
poisoned lock. Changed both to `MemoliteError::VectorStore(...)`. This matters for the error
taxonomy the whole crate relies on: `Internal` is supposed to mean "the engine's own lock (`conn`
or `embedder`) broke," and `VectorStore` is supposed to mean "the backend broke." Before this fix,
a caller pattern-matching on the error variant couldn't actually tell those two situations apart
for the in-memory backend.

### 2.2 `src/migrations.rs` — corrupted files now fail loudly at open time

**Layman version:** The database schema always *promised* that deleting a memory would
automatically clean up its search-index entry too (a "cascade delete"), and Step 0 already fixed
the setting that makes that promise real going forward. But that fix only protects the database
from *new* damage — it does nothing about a file that was already corrupted before the fix
existed, or one that got hand-edited outside the library entirely. Now, every time the database is
opened, it's given a quick health check, and if anything's inconsistent, opening fails outright
instead of quietly loading a broken file.

**Technical version:** After `run_migrations()` commits its transaction, it now runs
`PRAGMA foreign_key_check` and collects any reported violations. If the list isn't empty, `open()`
returns `Err(MemoliteError::Corruption(...))` naming the affected table(s), instead of succeeding
against a file with orphaned rows. This runs on every `open()`, not just the first one, since
that's consistent with how the `PRAGMA foreign_keys = ON` fix from Step 0 already works (a
per-connection setting has to be re-applied every time). Required importing `MemoliteError`
alongside `Result` into this file.

### 2.3 `src/recall.rs` — one named constant instead of a `10` waiting to be typo'd somewhere

**Layman version:** Several places in the upcoming recall logic need to agree on "how many results
do we return by default." Instead of writing the number `10` in more than one place and hoping
they never drift apart, there's now exactly one place that number lives.

**Technical version:** Added `pub const DEFAULT_RECALL_LIMIT: usize = 10;`, alongside the
already-existing `MAX_CANDIDATES` and `candidate_pool_size()`. Nothing consumes it yet — the real
`recall()` implementation that will use it is Phase 3, still ahead.

### 2.4 `src/vector_store/mod.rs` — writing down who's responsible for what

**Layman version:** The contract between "the engine" and "whatever's actually storing the
vectors" was implied by the code but not written down anywhere. Now it's explicit: here's what any
storage backend must guarantee, and here's what the engine currently does *not* double-check on
its own.

**Technical version:** Expanded the doc comment directly above the `VectorStore` trait to spell
out backend responsibilities (no duplicate IDs, every returned score finite, `search` returns at
most `k` results, `validate_vector` is always called first) and to explicitly document a known M3
limitation: the engine trusts an in-tree backend's output without defensively re-validating it.
No code changed — this is pure documentation, but the kind future-milestone authors will actually
read before they change backend-selection logic.

### 2.5 `src/engine.rs` — two conventions written down so they can't drift apart

**Layman version:** Two rules need to hold true everywhere in this codebase, forever, or subtle
bugs creep in over time: "expired" has to mean the exact same thing in every place that checks
for it, and "something went wrong, but we tried to clean up after it" always has to tell you about
the *original* problem, not just whether the cleanup itself worked. Both rules already existed in
spirit; now they're written down in one place everyone can find.

**Technical version:** Added two doc-comment blocks directly above `pub struct MemoryEngine`:

- **Expiration boundary:** `expires_at <= now` means "expired," everywhere — recall filtering,
  purge eligibility, and reconciliation must all use this same boundary, never `<` in one place
  and `<=` in another.
- **Compensation policy:** when a SQLite write succeeds but the paired vector-store
  write/delete fails, the engine attempts best-effort reconciliation but *always* surfaces the
  *original* error as `Err` — reconciliation never silently upgrades a failure into `Ok`. Applies
  uniformly to `forget()` and `purge_expired()`.

**One known gap flagged, not fixed, on purpose:** `purge_expired()` currently filters with
`expires_at < ?1` (strict less-than), which technically contradicts the `<=` convention just
written down. Per the plan's own phase ordering, fixing that boundary is a Phase 2 code change
(Step 43), not a Phase 1 documentation step, so it was left as-is here and just flagged instead of
silently fixed early or silently left undocumented.

---

## 3. Phase 2, Step 8 — shared test fixtures (`tests/common/mod.rs`, new file)

**Layman version:** A lot of the tests still to come need a "storage backend that can be told to
fail on command" — something no *real* backend can do on purpose. They also need a cheap,
repeatable way to turn text into a fake-but-consistent numeric fingerprint, without paying the
cost of loading the real AI model every single test. This file provides both of those tools, built
once, shared by every test file that needs them.

**Technical version:** New file `tests/common/mod.rs` (named `mod.rs` specifically so `cargo test`
doesn't treat it as its own test binary — it's only compiled when a real test file writes `mod
common;`). Two fixtures:

- **`FakeVectorStore`** — a real `VectorStore` trait implementation backed by a plain
  `HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>` behind a `Mutex`, doing the same
  dimension/finite validation a real backend must do, plus three `Mutex<bool>` switches
  (`fail_insert`, `fail_delete`, `fail_search`) that force the matching method to return
  `Err(MemoliteError::VectorStore(...))` on demand. Includes `always_failing_insert()` /
  `always_failing_delete()` / `always_failing_search()` convenience constructors for tests that
  want the failure active from the very first call, plus an `insert_raw()` escape hatch for
  seeding a "stale vector hit with no matching SQLite row" scenario directly.
- **`FakeEmbedder`** — a deterministic, hash-based stand-in for the real `fastembed`-backed
  `Embedder`. Documented explicitly as **not** injectable into `MemoryEngine` (its `embedder`
  field is a concrete `Mutex<Embedder>`, not a trait object — that shape was frozen back in Step
  0), so any test opening a real engine still loads the real ONNX model regardless. What it's
  actually for: cheaply generating reproducible vectors to feed directly into `FakeVectorStore`,
  `InMemoryVectorStore`, or raw SQL against the `embeddings` table.

Both fixtures were matched against the *actual* trait signatures in this repo
(`insert(&self, id: Uuid, vector: &[f32], ...)`, `search(...) -> Result<Vec<VectorHit>>`,
`delete(&self, id: Uuid)`, `VectorHit { id, score }`) rather than the slightly different shapes
(`&Uuid`, `Vec<f32>`, a `similarity` field) that appeared in the original planning documents —
those documents were written speculatively, ahead of the real Step 0 code.

Included a small `#[cfg(test)] mod fixture_self_tests` block (6 tests) proving the fixtures
themselves behave correctly — insert/search round-trips, each forced-failure flag actually fires
and leaves state as expected, and the embedder is deterministic, empty-input-rejecting, and
actually varies with input. This is stricter than Phase 2's own Gate 2 requires (Gate 2 only asks
that the fixtures *compile*), but catching a broken fixture now is cheaper than debugging a
mysterious Phase 3 test failure later and not knowing whether the bug is in the engine or in the
fixture pretending to be a backend.

---

## 4. What's explicitly still outstanding

Nothing beyond what's described above was implemented in this pass. In particular, still ahead:

- **Phase 2, Steps 9–13:** the real `store()`/`get()`/`forget()`/`purge_expired()`/
  `reconcile_vector_index()` rewrites, including finally changing `purge_expired()`'s `<` to `<=`.
- **Phase 2, Steps 14–23:** the actual test files that consume `tests/common/mod.rs`
  (`poison_test.rs`, `cosine_test.rs`, `compensation_test.rs`, updated `forget_test.rs`/
  `purge_test.rs`/`restart_test.rs`, `open_with_store_test.rs`).
- **Phase 3 (all of it):** the real `recall()` — right now it's still the honest
  `todo!("wired to recall_query() in M4")` stub from Step 0.
- **Phase 4:** malformed-data hardening tests, the `TempDb` RAII cleanup helper, and the final
  `cargo fmt --check` / `cargo clippy` / `cargo test --all-targets` milestone gate.

**Compiler caveat, carried over from the 13/07 changelog and still true:** no working Rust
toolchain was available in the environment used to prepare these changes (no `rustc`/`cargo` at
all this time, versus a too-old one last time). Every change above was verified by manual,
line-by-line review — matching every method signature against the actual trait definitions in
this repo, tracing lock-guard lifetimes by eye to confirm none crosses an `.await`, and applying
each edit as an exact `str_replace` against a freshly cloned copy of `main` so the "before" text is
guaranteed to match. The honest recommendation is still the same as last time: run
`cargo build && cargo clippy --all-targets -- -D warnings && cargo test` yourself before trusting
this as final.

---

## 5. Files touched this pass

1. `src/vector_store/in_memory.rs` — hardened `cosine()`, fixed `lock_read`/`lock_write` error
   variant, updated `search()` for the new fallible `cosine()`.
2. `src/migrations.rs` — added `PRAGMA foreign_key_check` corruption detection on every `open()`.
3. `src/recall.rs` — added `DEFAULT_RECALL_LIMIT`.
4. `src/vector_store/mod.rs` — expanded `VectorStore` trait documentation (docs only).
5. `src/engine.rs` — added expiration-boundary and compensation-policy convention docs (docs
   only).
6. `tests/common/mod.rs` — **new file.** `FakeVectorStore`, `FakeEmbedder`, and their self-tests.
7. `Changelogs/16thjuly.md` — this document.

**Explicitly not touched, and correctly so:** `src/error.rs`, `src/lib.rs` (already compliant —
see §1), and every file still scoped to Phase 2 Steps 9+ or later phases (`store()`, `get()`,
`forget()`, `purge_expired()`, `recall()`, and all remaining test files).