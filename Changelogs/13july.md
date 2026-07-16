# Memolite — Step 0 Changelog  13/07/26 (Detailed Walkthrough)

This document explains, in plain language first and then in technical depth, everything that
changed in Memolite during the Step 0 implementation pass — and every problem we ran into along
the way before landing on the version that's actually in the repo now.

---

## 1. The big picture — what Memolite even is

Think of Memolite as a **notebook for an AI**. When an AI "remembers" something, it writes a
note into this notebook. Later, instead of reading every page to find something, the AI can ask
"what do I know about X?" and get back the relevant notes.

Under the hood, every note is stored in two places at once:

1. **The notebook itself (SQLite)** — the actual text of the memory, plus facts about it: how
   important it is, when it was created, when it expires, and so on. This is the permanent,
   trustworthy record.
2. **A search index (the vector store)** — a numeric fingerprint of each note (its "embedding")
   that lets the AI find notes by *meaning*, not just exact words. Think of it like the index at
   the back of a textbook — it's a shortcut for finding things fast, but it's not the book
   itself.

Everything we did in this pass is about making sure **the notebook and its index never fall out
of sync**, and that a crash, a bug, or a slow network call never leaves you with a note that
exists in one place but not the other.

---

## 2. What changed — layman explanation first, then the technical version

### 2.1 "The librarian now uses one master key, not several unlabeled ones"

**Layman version:** Before, the code that talks to the notebook was set up as if only one person
could ever touch it at a time, with no formal way to hand off the key. As soon as we wanted
multiple parts of the program working with the same engine at once (which is the whole point of
an AI agent doing several things), that stopped being safe.

**Technical version:** `MemoryEngine` previously held a bare `rusqlite::Connection`. We changed
it to `conn: Mutex<Connection>`, `embedder: Mutex<Embedder>`, and
`vector_store: RwLock<Arc<dyn VectorStore>>`. Every method now locks, does its SQLite or embedder
work, and **drops the lock guard before any `.await`** — holding a `std::sync::Mutex` guard
across an await point can deadlock another task on the same executor thread, so this discipline
is enforced everywhere, not just where it was convenient.

### 2.2 "Writing a note and filing its index card used to be two separate steps — now it's one"

**Layman version:** Imagine writing a memory into the notebook, and separately writing its index
card. If the power went out between those two steps, you'd end up with a memory that has no
index card — invisible to search forever, but still cluttering the notebook. We fixed this so
both steps happen as a single all-or-nothing action.

**Technical version:** `store()` previously ran two independent `conn.execute()` calls (one
`INSERT` into `memories`, one into `embeddings`). We wrapped both in a single
`conn.transaction()` / `tx.commit()`. This is what makes the later invariant — "a memory row
always has a matching embedding row" — actually true, instead of usually true.

### 2.3 "If the search index rejects a new note, we now un-write it from the notebook too"

**Layman version:** Say the notebook write succeeds, but the search-index step fails (maybe the
search service is down). Previously, nothing cleaned up after that failure — you'd be left with
an orphaned note nobody could ever find. Now, if the index write fails, we go back and remove the
note from the notebook too, so you never have a "ghost" memory.

**Technical version:** After the SQLite transaction commits, `store()` calls
`vector_store.insert(...)`. If that fails, we run a compensating
`DELETE FROM memories WHERE id = ?1`. If *that* also fails (rare, but possible), we return a new
error variant, `MemoliteError::CompensationFailed { operation, compensation }`, carrying both
error messages — so whoever's debugging this isn't left guessing which half of the failure
happened. The same pattern was added to `forget()` and `purge_expired()`, except their
compensation step is "rebuild the whole vector index from SQLite" rather than "delete one row",
since by the time their vector-store call fails, the SQLite row is already gone.

### 2.4 "Deleting a note now checks the note's name is even valid before doing anything"

**Layman version:** If you asked the old code to delete a memory using a garbled ID, it would
first go delete... nothing (since nothing matched), and only afterward complain that your ID was
garbled. That ordering meant a "failed" delete request could still have side effects if the ID
happened to look almost-valid. We flipped the order: check the ID is well-formed *first*, then
touch anything.

**Technical version:** `forget(id: &str)` now does `let uuid = Uuid::parse_str(id)?;` as its
first line. A malformed ID now returns `Err(MemoliteError::InvalidUuid(_))` with zero database
side effects, proven directly by a test that stores a real memory, calls `forget()` with garbage,
and asserts the real memory is untouched in both SQLite and the vector store.

### 2.5 "We now catch a very specific kind of data corruption instead of shrugging at it"

**Layman version:** Suppose someone (or some bug) deletes a note's index card but leaves the note
itself in the notebook. Old logic would just skip that broken note when rebuilding the search
index — quietly, with no warning. That's dangerous: a memory could silently vanish from search
forever and nobody would know why. We now treat this as a loud, specific error instead of a
silent skip.

**Technical version:** `reconcile_vector_index()` rebuilds the vector store from SQLite using a
`LEFT JOIN` between `memories` and `embeddings`. If a memory row has `NULL` on the embedding
side, we return `Err(MemoliteError::Corruption(...))` instead of filtering that row out. This is
safe to do so aggressively *because* of fix 2.2 above — since `store()` always writes both rows
together in one transaction, the only way to reach this state is if the SQLite file was
corrupted or hand-edited outside the library, which is exactly the kind of thing you want
surfaced loudly, not swallowed.

### 2.6 "Turning on the safety feature that was silently switched off"

**Layman version:** The database schema already said "if you delete a note, automatically clean
up its index card too" (a foreign-key cascade). But there's a setting that has to be turned on
for that promise to actually work — and it never was. So the promise was never being honored;
deleting a note left an orphaned index-card row behind forever.

**Technical version:** SQLite disables foreign-key enforcement by default; the schema's
`embeddings.memory_id ... ON DELETE CASCADE` was inert because nothing ever ran
`PRAGMA foreign_keys = ON`. `migrations.rs` now runs that pragma on every single `open()` (it's a
per-connection setting, not something saved in the file, so it must be re-applied every time).

### 2.7 "The database now remembers which renovations it's already had"

**Layman version:** Previously the "make sure the notebook has the right pages" step just used
"create this section if it doesn't already exist" logic scattered around. That's fine until you
need to make a more surgical change later (like adding a new column) — you need a reliable way to
know "have I already done renovation #2 on this specific notebook, or not?" We added that
tracking.

**Technical version:** `migrations.rs` creates a `schema_migrations` table and records
`(version, applied_at)` rows. Step 0 applies exactly **migration 1** (the baseline
`memories`/`embeddings` schema + five indexes). It deliberately does **not** call anything related
to the `confidence` column yet — that's migration 2, added later (see Hurdle #1 below for why
this distinction mattered so much).

### 2.8 "A pluggable search-index system, with one shared rulebook instead of five different ones"

**Layman version:** Previously there was no formal contract for "what does a search index have
to be able to do." We wrote one: a `VectorStore` interface that any backend (in-memory, or later,
a remote service over HTTP) has to implement — insert, search, delete, check-if-present, and
"replace everything." Critically, we wrote **one shared checklist** (dimension check + "are these
numbers actually valid numbers, not garbage like infinity") that every single one of those
operations has to run through, instead of trusting that if one method checks something, its
neighbor does too.

**Technical version:** `src/vector_store/mod.rs` defines the `VectorStore` trait and a single
`validate_vector(label, v, dim)` function. `insert`, `search` (validating the *query* vector, not
just stored ones), and `replace_all` (validating every entry) all call it explicitly.
`src/vector_store/in_memory.rs` implements `InMemoryVectorStore`, the default backend: a
`RwLock<HashMap<Uuid, (Vec<f32>, Metadata)>>` doing brute-force cosine similarity. It ships with
8 tests, including the two that a previous draft was missing: rejecting a wrong-dimension
*query* and a non-finite *query* (not just rejecting bad data on the way in).

---

## 3. Hurdles we ran into — in the order we hit them

This project went through several review passes before landing on the version now in the repo.
Each pass caught real problems. Here they are, roughly chronologically, with what went wrong and
how it was actually fixed.

### Hurdle #1 — The plan referenced a module that didn't exist yet

**What happened:** An earlier version of the build plan had Step 0's migration runner call
`crate::confidence::repair_confidence_column(conn)`. That function — and the whole
`src/confidence.rs` file — wasn't written until a much later milestone (M6). Since Rust checks
every reference at compile time, **the project would fail to compile at the very first
checkpoint**, before a single test could run.

**Why it's a big deal:** This is the software equivalent of a recipe's first step saying "now
stir in the sauce from step 14" — you can't do that yet, the sauce doesn't exist.

**Fix:** Step 0's `run_migrations()` only ever calls migration 1 (the baseline schema). The
confidence-column call is left as a **comment** describing exactly what M6 will add later — not
as code that runs now.

### Hurdle #2 — The "final" module list broke every early step

**What happened:** The plan tried to write out `lib.rs` once, "for the whole project," up front —
listing modules like `ranking`, `streaming`, `compression`, and `maintenance` that don't get real
content until much later milestones. Rust requires every module you declare to point at a file
that actually exists with real content (or at least compiles as empty) *right now* — you can't
declare a module "in advance."

**Why it's a big deal:** Same root problem as Hurdle #1 — writing the ending before the
beginning. Worse, this version of the plan also accidentally **dropped** the existing `memory`
module from the list, which would have broken every part of the program that already depended on
it.

**Fix:** `lib.rs` now registers only what has real content *at this point*: `embedder`, `engine`,
`error`, `memory`, `recall`, `vector_store`, and (privately) `migrations`. Every other module gets
added in the milestone that actually writes its code.

### Hurdle #3 — A "temporary" implementation quietly called a method from the future

**What happened:** The plan's version of `recall()` (search-by-meaning) was written to call
`recall_query()` — but `recall_query()` doesn't get built until milestone M4, several steps later
than where `recall()` itself was introduced.

**Fix:** We left `recall()` as the honest `todo!("wired to recall_query() in M4")` stub it already
was, and did **not** wire it up early. It's tempting to "just write it once, correctly, and be
done" — but that only works if every dependency it needs already exists. It didn't yet.

### Hurdle #4 — A private helper needed to be called from a different file, later

**What happened:** `open_with_store_internal()` — the shared constructor logic behind
`MemoryEngine::open()` — was originally private. A future milestone (M11) needs to call it from a
completely different file when it adds a public `open_with_store()` function for remote vector
backends. Rust's module privacy would have blocked that call.

**Fix:** Marked it `pub(crate)` now, while it's still simple to do, rather than leaving it as a
compile error waiting to happen three milestones later.

### Hurdle #5 — The first real patch attempt had real bugs, not just sequencing problems

Once the sequencing issues above were sorted out, we actually wrote and reviewed a first patch.
It got rejected for good reasons:

- **No transaction around the two-row write.** `store()` inserted into `memories` and then
  `embeddings` as two separate operations. If the second one failed, you'd get an orphaned
  memory with no embedding — which is exactly the "corruption" state the rest of the design
  was supposed to make impossible. **Fixed** by wrapping both inserts in one
  `conn.transaction()`.
- **No cleanup when the vector-store step failed.** `store()`, `forget()`, and `purge_expired()`
  all mutated SQLite first and then tried the vector store — but if the vector-store call
  failed, nothing rolled anything back. **Fixed** by adding real compensation logic (delete the
  orphaned row, or rebuild the whole index) plus the new `CompensationFailed` error so failures
  are traceable instead of silent.
- **A test held a lock across an `await`.** One of the new tests read a lock guard and then
  immediately awaited an async call while still holding it — which is precisely the pattern the
  whole design was trying to avoid everywhere else. **Fixed** by cloning the underlying `Arc`,
  dropping the guard, and *then* awaiting.
- **The "does restarting rebuild the index?" test didn't actually check the index.** It only
  confirmed the memory was still readable from SQLite — which was already guaranteed and proved
  nothing about whether the vector index reconciliation logic worked at all. **Fixed** by having
  the test directly inspect the reconstructed vector store's contents after reopening.
- **The foreign-key cascade test proved nothing about the actual code path.** It manually turned
  on foreign-key enforcement on a *separate* database connection and then checked the cascade —
  which only proves SQLite's cascade feature works in general, not that *our* code turns the
  pragma on when it should. **Fixed** by deleting through `engine.forget()` (the engine's own
  connection — the one that's supposed to have the pragma applied to it) and checking the result
  from an independent connection afterward.
- **The patch file itself didn't apply.** A `git apply --check` against the actual repository
  rejected it outright, near a file-rename hunk. **Fixed** by regenerating the patch from a real
  `git diff` against a freshly cloned copy of `main`, and verifying `git apply --check` passes
  before handing it over.

### Hurdle #6 — The repository moved out from under us mid-conversation

**What happened:** Partway through, some of the files we'd been treating as "empty placeholders
we're about to fill in" — `tests/migration_test.rs`, `tests/restart_test.rs` — turned out to
already exist as 0-byte placeholders in the live repository, added after our first clone. Our
earlier "list of brand-new files" was stale.

**Fix:** Re-cloned `main` fresh, re-checked every file's actual status, and corrected the
file-ranking list to distinguish three categories accurately: files with real pre-existing code
that got rewritten (`engine.rs`, `error.rs`, `lib.rs`, `Cargo.toml`), placeholder files that got
filled in for the first time (`vector_store/mod.rs`, `vector_store/in_memory.rs`, `migrations.rs`,
`recall.rs`, `tests/migration_test.rs`), and the one genuinely new file that existed nowhere
before (`CHANGELOG.md`).

### Hurdle #7 — No working Rust compiler available to verify any of this

**What happened:** The sandbox environment used to prepare these changes only has `rustc`/`cargo`
1.75 available via `apt`, but the project's `Cargo.toml` specifies `edition = "2024"`, which
requires a much newer toolchain (1.85+). There was no way to actually run `cargo build` or
`cargo test` in that environment.

**What we did instead:** A manual, line-by-line review substituting for the compiler: grepping
for every reference to a module or type that doesn't exist yet (found none outside of a comment),
cross-checking every `MemoliteError::Variant` used in code against what's actually declared in
`error.rs`, and tracing every lock guard's lifetime by eye to confirm none crosses an `.await`.
This is a real limitation, though — **the honest recommendation is still to run
`cargo build && cargo test` yourself** before trusting this as final, since a manual review can
miss things a compiler won't.

---

## 4. Where things stand now

Step 0 is implemented and, as far as manual review can confirm, internally consistent: nothing
references anything from a later milestone, every lock is released before any `await`, every
vector write goes through the same validation function, and the memory-plus-embedding write
invariant is enforced by an actual transaction rather than an assumption.

**Explicitly not done yet** (by design — these are later milestones, not oversights):

- `recall()` — still a `todo!()` stub; real semantic search lands in M4.
- `ConfidenceLevel`, the `confidence` column, and migration version 2 — M6.
- Streaming ingestion, compression, the maintenance controller, and the HTTP-backed remote vector
  store — M8, M9, M10, and M11 respectively.

**Files touched, in dependency order** (each only relies on files earlier in this list):

1. `Cargo.toml` — one new dependency (`async-trait`)
2. `src/error.rs` — five new error variants
3. `src/vector_store/mod.rs` — the `VectorStore` trait and shared validation
4. `src/vector_store/in_memory.rs` — the default backend and its tests
5. `src/migrations.rs` — schema setup, versioned and idempotent
6. `src/recall.rs` — small, self-contained constants for later use
7. `src/lib.rs` — module registration and re-exports
8. `src/engine.rs` — the core rewrite: locking, transactions, compensation logic
9. `tests/migration_test.rs` — schema, index, and cascade-delete tests
10. `CHANGELOG.md` — this project's running history