# Memolite Project – Changelog

**Date:** July 9, 2026

## Goal Completed

Added expiry logic, a working `purge_expired()` method, proper typed error
handling across the database layer, and cleaned up the code so it passes
`cargo clippy` with zero warnings. The store now actually "forgets" things
on its own instead of keeping every memory forever.

---

# Detailed Progress

## 1. Added Per-Type Expiry Defaults (TTL)

Not every memory should live for the same amount of time. A quick note about
what happened five minutes ago shouldn't stick around as long as a core fact
about the user.

Added a `default_ttl()` method on `MemoryType` that returns how long each
type is allowed to live before it's eligible to be deleted:

| Memory Type | Default Lifetime |
|---|---|
| Semantic | 365 days |
| Episodic | 30 days |
| Procedural | 730 days |
| Working | 4 hours |

**Why this matters**

* Short-lived scratch notes (`Working`) clean themselves up automatically.
* Durable facts (`Semantic`, `Procedural`) stick around much longer, matching
  how they'd realistically be useful.
* This is just a lookup table for now — no machine learning, no dynamic
  tuning. Simple and predictable on purpose.

---

## 2. `store()` Now Sets `expires_at` Automatically

Previously, every stored memory had `expires_at = NULL`, meaning nothing
ever expired.

Updated `store()` so that every time a memory is saved, it automatically
calculates:

```
expires_at = created_at + memory_type.default_ttl()
```

**Result**

* A `Working` memory stored right now will expire roughly 4 hours from now.
* A `Semantic` memory stored right now will expire roughly a year from now.
* The caller doesn't have to think about this at all — it just happens
  based on the type they picked.

---

## 3. Implemented `purge_expired()`

Added the method that actually deletes expired memories:

```rust
let deleted_count = engine.purge_expired().await?;
```

**What it does**

* Deletes every row where `expires_at` is set and is earlier than right now.
* Leaves memories with no expiry (`expires_at IS NULL`) completely alone.
* Returns how many rows were deleted, so it's possible to log or report on
  cleanup activity later.

**Why this matters**

* This is the mechanism that keeps the store from growing forever with
  stale scratch notes and old events.
* It's a manual call for now (you decide when to run it) — an automatic
  background task that runs this on a timer is a later milestone, not part
  of this change.

---

## 4. Wrote a Real Test for Expiry

Added `tests/purge_test.rs`, which:

1. Stores one normal memory through the public `store()` method (this one
   gets a real, future expiry date and should survive).
2. Manually inserts a second memory directly into the SQLite file with an
   `expires_at` timestamp set an hour in the past (since `store()` itself
   can't produce an already-expired memory).
3. Calls `purge_expired()`.
4. Confirms exactly one memory was deleted, the expired one is gone, and
   the still-valid one is untouched.

**Why the manual insert was necessary**

Since `store()` always calculates a future expiry, there was no way to
create an "already expired" memory through the normal public API. Opening a
second raw connection to the same database file, then inserting a row by
hand with a past expiry, was the cleanest way to set up that scenario
without changing production code just to make it testable.

---

## 5. Replaced `anyhow` With a Proper Typed Error Type

Up to this point, all database errors were being handled through
`anyhow::Result`, which is fine for prototyping but doesn't tell a caller
*what kind* of thing went wrong — just that something did.

**Added `src/error.rs`**, a new `MemoliteError` enum (built with the
`thiserror` crate) with a specific variant for each real failure mode:

* `Database` — an actual SQLite error (bad SQL, constraint violation, etc.)
* `InvalidMemoryType` — a row's `type` column had a value that shouldn't be
  possible given the database's own `CHECK` constraint
* `InvalidMetadata` — a row's metadata column wasn't valid JSON
* `InvalidUuid` — a row's id (or `superseded_by`) wasn't a real UUID
* `InvalidTimestamp` — a row's stored timestamp doesn't correspond to a
  real point in time

Every public method in `MemoryEngine` (`open`, `store`, `get`, `forget`,
`purge_expired`) now returns this crate's own `Result<T>` type instead of
`anyhow::Result<T>`.

**Why this matters**

* Nothing in the database layer is silently swallowed or generically
  labeled "an error happened" anymore — each failure mode is named.
* Down the line, calling code can `match` on the specific error variant and
  react differently (e.g. retry on a `Database` error, but not on an
  `InvalidUuid` error, since retrying won't fix corrupted data).
* This is also just a better habit than reaching for `anyhow` everywhere —
  `anyhow` is great for applications, but a library that other code depends
  on should expose real, documented error types.

---

## 6. Removed All `.unwrap()` Calls on Database Operations

Went through every database-touching function and confirmed there isn't a
single `.unwrap()` or `.expect()` hiding on a real SQLite call path. Every
fallible operation now goes through `?` and bubbles up as a proper
`MemoliteError` instead of being able to crash the whole program if
something unexpected happens (a locked file, a malformed row, etc.).

---

## 7. Ran `cargo clippy` and Fixed the Warning It Found

Ran:

```
cargo clippy --all-targets --all-features -- -D warnings
```

Clippy flagged one real issue:

> `from_str` can be confused for the standard trait method
> `std::str::FromStr::from_str`

**What this meant**

`MemoryType` had a method literally called `from_str(s: &str) -> Result<Self>`,
which looks exactly like it's implementing Rust's built-in `FromStr` trait
(the one that would let you write `"episodic".parse::<MemoryType>()`) — but
it wasn't actually wired up as that trait. That's a naming trap: anyone
reading the code would reasonably expect `.parse()` to work, and it
wouldn't.

**Fix**

Renamed the method from `from_str` to `parse_str` everywhere it's defined
and called. Same behavior, no more misleading name.

**Why this matters**

* This is exactly the kind of thing `clippy` is good at catching early —
  a naming choice that would confuse a future reader (including future me),
  not a bug that shows up in a test.
* Fixing lint warnings as they appear, rather than letting them pile up,
  keeps the codebase in a state where `cargo clippy -- -D warnings` can be
  run as a real gate before every commit.

---

# Current Status

### Completed

* ✅ Per-type TTL/decay defaults (`MemoryType::default_ttl()`)
* ✅ `store()` computes and sets `expires_at` automatically
* ✅ `purge_expired()` implemented and tested
* ✅ Manual-insert test proving expired memories actually get deleted
* ✅ All SQLite/database errors wrapped in a typed `MemoliteError` (via
  `thiserror`) — no more `anyhow` in the database layer
* ✅ Zero `.unwrap()` calls on any database operation
* ✅ `cargo clippy --all-targets --all-features -- -D warnings` passes clean
* ✅ `cargo test` — 3 passing tests (`store_then_get_returns_matching_memory`,
  `forget_removes_the_memory`, `purge_expired_deletes_only_expired_memories`)

---

# Current Capabilities

The project can now:

* Open or create a SQLite database.
* Automatically initialize the required schema.
* Store memories with metadata, and automatically calculate when each one
  should expire based on its type.
* Retrieve a memory by ID (`get`).
* Delete a specific memory by ID (`forget`).
* Delete all expired memories in one call (`purge_expired`).
* Return specific, typed errors instead of generic ones for every possible
  failure mode.
* Persist all of the above between application runs.

**Checkpoint reached:** this is now a working, typed, persistent,
self-expiring key-value store — with zero AI involved anywhere yet. This is
the "systems engineering" half of the project, fully banked before any
embedding or ranking logic gets added.

---

# Next Tasks

The next planned steps move into the ranking/retrieval side of the project:

* **Step:** Add local embedding generation so `recall()` has something to
  search against.
* **Step:** Implement `InMemoryVectorStore` with cosine similarity search.
* **Step:** Wire `recall()` up to real similarity search instead of
  `todo!()`.