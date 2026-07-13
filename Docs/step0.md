# Memolite — Step 0 Implementation Handbook
### A Complete, Copy-Pasteable Engineering Manual for the V6 Master Build Plan's Foundation Layer

**Repository:** `https://github.com/mayanpathak/memolite`
**Scope of this document:** Step 0 ONLY (0.1 through 0.8). Nothing from M3 onward is covered.
**Audience:** A Rust engineer who has never seen this repository before.
**Authoritative source:** *Memolite — Final Master Build Plan (v6)*.

---

## 0. How to Use This Handbook

This handbook is a **procedure**, not a summary. Every section tells you:

1. **What file** you are touching (exact path).
2. **Whether that file already exists** in the repository or must be created.
3. **What the file currently contains** (if it exists), quoted or described precisely enough that you can locate the right spot.
4. **Exactly where** in the file the new code goes (before/after which existing line, or "create the whole file").
5. **The complete code** to paste — never "insert appropriately."
6. **Why** the change exists — which V6 requirement or which V5-review finding it closes.
7. **What must compile** afterward, and how to verify it with real `cargo` commands.

You must complete sections **in the order given**. Each section ends with a **Checkpoint**. Do not move to the next section until the checkpoint's `cargo` commands succeed exactly as described. If they don't, stop and use the **Common Mistakes / Debugging** block at the end of that section before proceeding.

### 0.0.1 — An Important Correction You Need to Know About Before You Start

V6, as written, is the *architecturally* authoritative document — its design (the `VectorStore` trait, `replace_all`, `BackfillPolicy`, the corruption-on-missing-embedding rule, the single validation helper) is correct and final. However, a technical review of V6 (included alongside it in your source materials) identified that **V6's Step 0, taken 100% literally, does not compile**, for two concrete reasons:

1. V6's `run_migrations()` (its Step 0.7) calls `crate::confidence::repair_confidence_column(conn)?;` — but `src/confidence.rs` is not created until Milestone M6, which is far outside the scope of Step 0. If you paste that line in now, `cargo build` fails with `error[E0433]: failed to resolve: use of undeclared crate or module 'confidence'`.
2. V6's "authoritative final `lib.rs`" block declares modules (`ranking`, `requests`, `confidence`, `streaming`, `compression`, `maintenance`, `stats`) and re-exports types (`RecallQuery`, `StoreRequest`, `ConfidenceLevel`, `MaintenanceHandle`, `MemoryStats`, ...) that do not exist until later milestones. Rust requires every `mod x;` line to point at a real file and every `pub use` to point at a real item, at all times. Declaring these now breaks the Step 0 checkpoint. It also silently drops `pub mod memory;` / `pub use memory::{Memory, MemoryType};`, which already exist in the current repository and are load-bearing for everything else — losing them breaks the whole crate.

This handbook implements the **architecture V6 specifies**, but sequences it so that **Step 0 alone compiles and passes its own tests**, exactly as V6's own text promises ("Checkpoint 0: `cargo build && cargo test` green"). Concretely, this means:

- We do **not** call `confidence::repair_confidence_column` from `run_migrations()` in Step 0. That call is added later, in M6, in the same milestone that creates `src/confidence.rs`. Step 0's migration only performs V6's "migration 1" (baseline tables/indexes). This is explicitly called out at the point it matters below (§0.7).
- We register in `lib.rs` **only** the modules and public items that exist as of the end of Step 0. `lib.rs` will grow again in M3, M4, M5, etc. — this handbook shows you the Step-0-correct version and tells you, at each line, which future milestone extends it.
- We keep the repository's existing `pub mod memory;` and `pub use memory::{Memory, MemoryType};` exactly as they are today. Step 0 does not touch `src/memory.rs`.

Every other structural decision below — the trait shape, the struct shape, `BackfillPolicy`, `reconcile_vector_index`, `validate_vector` — is taken **verbatim** from V6, because V6's technical review found no defects in those specific designs; only in the two sequencing issues above.

### 0.0.2 — Prerequisites

Before starting, confirm on your machine:

```bash
rustc --version      # should be a recent stable toolchain (1.75+)
cargo --version
git --version
```

Clone and enter the repository:

```bash
git clone https://github.com/mayanpathak/memolite.git
cd memolite
git log --oneline     # you should see the existing baseline commits
cargo build           # confirm the CURRENT repo builds before you change anything
cargo test
```

**Do not proceed until `cargo build` and `cargo test` succeed on the untouched repository.** If they don't, Step 0 is not your problem to fix — the baseline is broken, and you need to resolve that first (out of scope for this handbook).

---

## 1. Repository Tree — Before and After Step 0

### 1.1 Tree BEFORE Step 0 (what you should see today)

```text
memolite/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── src/
│   ├── lib.rs
│   ├── engine.rs          # MemoryEngine — currently owns a plain rusqlite::Connection
│   ├── memory.rs           # Memory, MemoryType — already correct, do not touch
│   ├── error.rs             # MemoliteError enum — will be extended
│   └── embedder.rs          # FastEmbed wrapper — already correct, do not touch
├── tests/
│   ├── crud_test.rs
│   └── purge_test.rs
├── examples/
│   ├── basic.rs
│   └── embed_test.rs
└── Changelogs/
    ├── 8thjuly.md
    └── 9thjuly.md
```

### 1.2 Tree AFTER Step 0 (what you will have when this handbook is complete)

```text
memolite/
├── Cargo.toml                       # MODIFIED — new dependencies added (§0.1)
├── Cargo.lock                       # auto-regenerated by cargo, do not hand-edit
├── README.md                        # untouched
├── src/
│   ├── lib.rs                       # MODIFIED — Step-0-correct module registration (§0.9)
│   ├── engine.rs                    # MODIFIED — struct replaced, open()/open_with_store_internal
│   │                                 #            added, reconcile_vector_index added (§0.5, §0.8)
│   ├── memory.rs                    # UNCHANGED
│   ├── error.rs                     # MODIFIED — new error variants added (§0.2)
│   ├── embedder.rs                  # UNCHANGED
│   ├── migrations.rs                # NEW FILE — run_migrations (§0.7)
│   ├── recall.rs                    # NEW FILE (partial) — MAX_CANDIDATES, candidate_pool_size (§0.6)
│   └── vector_store/                # NEW DIRECTORY
│       ├── mod.rs                   # NEW FILE — VectorStore trait, VectorHit, VectorEntry,
│       │                             #            validate_vector (§0.3)
│       └── in_memory.rs             # NEW FILE — InMemoryVectorStore + its unit tests (§0.4)
├── tests/
│   ├── crud_test.rs                 # UNCHANGED
│   └── purge_test.rs                # UNCHANGED
├── examples/
│   ├── basic.rs                     # UNCHANGED
│   └── embed_test.rs                # UNCHANGED
└── Changelogs/
    ├── 8thjuly.md                   # UNCHANGED
    └── 9thjuly.md                   # UNCHANGED
```

**Two new files under `src/vector_store/`, one new file `src/migrations.rs`, one new (partial) file `src/recall.rs`. Three existing files modified: `Cargo.toml`, `src/error.rs`, `src/engine.rs`, `src/lib.rs` (four, not three — corrected count below in the summary table).**

| File | Status | Section |
|---|---|---|
| `Cargo.toml` | MODIFY | §0.1 |
| `src/error.rs` | MODIFY | §0.2 |
| `src/vector_store/mod.rs` | CREATE | §0.3 |
| `src/vector_store/in_memory.rs` | CREATE | §0.4 |
| `src/engine.rs` | MODIFY | §0.5, §0.8 |
| `src/recall.rs` | CREATE | §0.6 |
| `src/migrations.rs` | CREATE | §0.7 |
| `src/lib.rs` | MODIFY | §0.9 |

---

## 2. Module Dependency Diagram (End of Step 0)

This is the compile-order dependency graph. Rust doesn't strictly require you to write files in this order, but understanding it tells you *why* a change in one file makes another file compile or fail to compile.

```text
                     ┌───────────────┐
                     │   lib.rs      │  registers every module below
                     └───────┬───────┘
                             │
        ┌────────────────────┼─────────────────────┬───────────────┐
        │                    │                     │               │
        ▼                    ▼                     ▼               ▼
  ┌───────────┐        ┌───────────┐        ┌─────────────┐  ┌───────────┐
  │ error.rs  │◄───────┤ memory.rs │        │ embedder.rs │  │ recall.rs │
  │(no deps   │        │(no deps   │        │(depends on  │  │(depends on│
  │ on others)│        │ on others)│        │ error.rs)   │  │ nothing   │
  └─────┬─────┘        └─────┬─────┘        └──────┬──────┘  │ new here) │
        │                    │                      │         └───────────┘
        │              ┌─────┴──────────────────────┘
        │              │
        ▼              ▼
  ┌─────────────────────────────┐
  │ vector_store/mod.rs         │  depends on: error.rs
  │  - VectorStore trait        │
  │  - VectorHit, VectorEntry   │
  │  - validate_vector()        │
  └──────────┬───────────────────┘
             │
             ▼
  ┌─────────────────────────────┐
  │ vector_store/in_memory.rs   │  depends on: error.rs, vector_store/mod.rs
  │  - InMemoryVectorStore      │
  └──────────┬───────────────────┘
             │
             ▼
  ┌─────────────────────────────┐
  │ migrations.rs               │  depends on: error.rs (via crate::error::Result)
  │  - run_migrations()         │
  └──────────┬───────────────────┘
             │
             ▼
  ┌─────────────────────────────────────────────┐
  │ engine.rs                                    │  depends on: error.rs, memory.rs,
  │  - MemoryEngine struct                       │              embedder.rs, migrations.rs,
  │  - BackfillPolicy enum                       │              vector_store/mod.rs,
  │  - open() / open_with_store_internal()       │              vector_store/in_memory.rs
  │  - reconcile_vector_index()                  │
  └───────────────────────────────────────────────┘
```

**Rule of thumb:** a file can only reference (`use crate::...`) something that has already been declared as a module in `lib.rs` and that actually exists on disk with the item defined inside it. This is why we build bottom-up: `error.rs` first (nothing depends on it existing that isn't already there), then the new leaf modules (`vector_store`, `recall`, `migrations`), and only last the file that ties them all together (`engine.rs`), and finally `lib.rs` is updated to expose the new surface.

---

## 3. Section 0.1 — `Cargo.toml`

### 3.1 Why this modification exists

Every dependency used anywhere in Step 0 (and, per V6's stated intent, anywhere in the *entire* V6 plan) must be declared once, up front, so that no later milestone silently assumes a dependency that was never added. This is V6's explicit design decision ("0.1 — Cargo.toml, one authoritative dependency step") and it closes a review finding from the earlier V4 pass (dependencies like `async-trait`, `tokio-util`, `tracing`, `wiremock` were previously scattered and easy to miss).

For Step 0 specifically, you need:

- `async-trait` — the `VectorStore` trait (§0.3) declares `async fn` methods inside a `trait`. Native Rust trait syntax does not support `async fn` in traits directly in a `dyn`-compatible way in this Rust edition; `async-trait` provides the attribute macro `#[async_trait]` that desugars this into a boxed future under the hood, which is required because `MemoryEngine` will later store `Arc<dyn VectorStore>` (a trait object) — trait objects need `dyn`-compatible methods.
- `tokio` — the async runtime. `InMemoryVectorStore`'s unit tests use `#[tokio::test]`, and every trait method is `async`.
- `serde` / `serde_json` — `VectorEntry.metadata` is a `HashMap<String, serde_json::Value>`, used to carry arbitrary JSON-like metadata per memory.
- `uuid` — every memory and every vector entry is identified by a `Uuid`.
- `rusqlite` — the underlying SQLite driver used by `migrations.rs` and `engine.rs`.
- `chrono` — used later in `engine.rs` for timestamps (`Utc::now()` appears in the constructor's surrounding code even though Step 0 itself doesn't call it directly in most of its own snippets — it is required because `engine.rs` as a whole file needs it for the parts introduced in Step 0.5 that read `chrono` types via `rusqlite`'s row extraction, and because subsequent steps in the same file will need it — declaring it now avoids a later silent gap).
- `thiserror` — used by `error.rs` to derive `Error` on `MemoliteError`.
- `bincode` — used by `reconcile_vector_index` (§0.8) to deserialize persisted vector blobs.
- `tokio-util` — not strictly required by Step 0's code, but V6 declares it in the "one authoritative dependency step" because `CancellationToken` (used in M8/M10) must never be added implicitly later. Declaring it now costs nothing and prevents drift.
- `tracing` — same reasoning: required starting M10, declared now per V6's "declare everything up front" philosophy.
- `reqwest` / `urlencoding` (optional, behind the `generic-http` feature) — not used until M11, but the `[features]` table and the optional dependency declarations are added now so that the `Cargo.toml` never needs a second "authoritative dependency step." They are `optional = true`, so **they do not get compiled or downloaded unless you explicitly build with `--features generic-http`**. This is important: it means Step 0's default `cargo build` will not pull in `reqwest` at all.
- `criterion` (dev-dependency) — not used until M12's benchmarks, declared now for the same reason.
- `wiremock` (dev-dependency) — not used until M11's HTTP backend tests, declared now for the same reason.

### 3.2 Current state of `Cargo.toml`

The file exists. It currently contains (at minimum) a `[package]` section and dependencies for `rusqlite`, `fastembed`, `serde`, `serde_json`, `uuid`, `chrono`, `tokio`, `thiserror`/`anyhow`, and `bincode`, matching what `embedder.rs` and the existing `engine.rs` already use. Open the file and locate the `[dependencies]` table.

### 3.3 Exact replacement

Because dependency tables are easy to get wrong via partial edits (duplicate keys are a hard `cargo` error), **replace the entire `[dependencies]`, `[dev-dependencies]`, and `[features]` sections** (add `[features]` and `[dev-dependencies]` if they don't exist yet) with the block below. Leave `[package]` and `[[bench]]` (if present) untouched — do not add `[[bench]]` yet; that belongs to M12, not Step 0. Do NOT add it now — adding a `[[bench]]` entry that points at `benches/memolite_bench.rs` before that file exists will make `cargo build`/`cargo test` fail with "couldn't read benches/memolite_bench.rs: No such file or directory."

Paste this, replacing the existing `[dependencies]` table (and adding `[dev-dependencies]` / `[features]` if absent):

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "2"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4", "serde"] }
rusqlite = { version = "0.37", features = ["bundled"] }
fastembed = "5"
bincode = "1.3"
async-trait = "0.1"
tokio-util = { version = "0.7", features = ["rt"] }
tracing = "0.1"

# generic-http backend only — never pulled in by default builds.
# Not used until Milestone M11. Declared now so the dependency list
# never needs a second "authoritative" pass. `optional = true` means
# neither crate is downloaded or compiled unless the "generic-http"
# feature is explicitly enabled.
reqwest = { version = "0.12", features = ["json"], optional = true }
urlencoding = { version = "2", optional = true }

[dev-dependencies]
tokio = { version = "1", features = ["full", "test-util"] }
wiremock = "0.6"
criterion = "0.5"

[features]
generic-http = ["dep:reqwest", "dep:urlencoding"]
```

**Do not add a `[[bench]]` table in Step 0.** That is introduced in M12 alongside the actual `benches/memolite_bench.rs` file. Adding it early breaks the build because Cargo eagerly validates that the referenced bench file exists.

### 3.4 Why each field matters (line-by-line)

| Line | Meaning |
|---|---|
| `tokio = { version = "1", features = ["full"] }` | `"full"` enables the macro (`#[tokio::main]`, `#[tokio::test]`), sync primitives, time, and rt-multi-thread features all at once — simplest correct choice for a library whose tests exercise async code. |
| `serde = { ..., features = ["derive"] }` | Enables `#[derive(Serialize, Deserialize)]`, used later on `ConfidenceLevel` (M6) and needed transitively by `serde_json::Value`. |
| `serde_json = "1"` | Provides `serde_json::Value`, the type used for all "flexible metadata" fields (`VectorEntry.metadata: HashMap<String, Value>`). |
| `anyhow = "1"` | Used by the existing codebase for ergonomic error conversion in binaries/examples; not directly used by Step 0's new library code, but already a repo dependency, kept for compatibility. |
| `thiserror = "2"` | Provides `#[derive(thiserror::Error)]`, used in `error.rs` (§0.2) to give `MemoliteError` a real `std::error::Error` implementation with formatted `Display` messages. |
| `chrono = { ..., features = ["serde"] }` | `DateTime<Utc>` is used throughout the engine for timestamps; the `"serde"` feature lets these timestamps be serialized if metadata ever embeds them. |
| `uuid = { ..., features = ["v4", "serde"] }` | `"v4"` enables `Uuid::new_v4()` (random UUID generation); `"serde"` lets `Uuid` appear inside serde-serialized structures. |
| `rusqlite = { ..., features = ["bundled"] }` | `"bundled"` compiles SQLite from source as part of the build, so you don't need a system-installed `libsqlite3` — critical for reproducible builds across machines. |
| `fastembed = "5"` | The embedding-generation library already used by `embedder.rs`. Step 0 doesn't call it directly but `engine.rs`'s `open()` (§0.5) does, so it must remain declared. |
| `bincode = "1.3"` | Binary serialization format used to store `Vec<f32>` embeddings as a compact `BLOB` in SQLite, and to deserialize them back in `reconcile_vector_index` (§0.8). |
| `async-trait = "0.1"` | See §3.1 above — required for the `VectorStore` trait's async methods to be usable as `dyn VectorStore`. |
| `tokio-util = { ..., features = ["rt"] }` | Provides `CancellationToken`, not used until M8/M10, declared now. |
| `tracing = "0.1"` | Structured logging macros (`tracing::warn!`), not used until M10, declared now. |
| `reqwest` / `urlencoding` (optional) | HTTP client and URL-encoding, only for the M11 `generic-http` feature. |
| `[dev-dependencies] tokio { "test-util" }` | Enables `tokio::time::pause()`-style deterministic clock control for tests (used starting M10). |
| `wiremock = "0.6"` | Mock HTTP server for M11's tests. |
| `criterion = "0.5"` | Benchmarking harness for M12. |
| `[features] generic-http = [...]` | Declares a Cargo feature flag that, when enabled, turns on the two optional dependencies. Off by default. |

### 3.5 Checkpoint 0.1

Run:

```bash
cargo build
```

**Expected result:** Cargo will re-resolve dependencies (you will see many `Compiling ...` and `Downloading ...` lines for the newly-added crates: `async-trait`, `tokio-util`, `tracing`, and their transitive dependencies). Because you have not yet changed any `.rs` file, the crate itself should compile exactly as it did before (same warnings/errors as your pre-Step-0 baseline, nothing new). If `reqwest`/`urlencoding` are downloaded even though you didn't pass `--features generic-http` — **stop**, that means `optional = true` was not applied to those two lines; re-check §3.3 exactly.

```bash
cargo tree -e features | grep -i reqwest
```

**Expected result:** no output (reqwest is not part of the default feature set's resolved tree). If you see `reqwest` listed, re-check that both `reqwest` and `urlencoding` have `optional = true` and that `[features] generic-http = ["dep:reqwest", "dep:urlencoding"]` uses the `dep:` prefix syntax exactly as shown (omitting `dep:` implicitly creates a *feature* named `reqwest` instead of gating the dependency, which is a common and subtle mistake).

### 3.6 Common mistakes

- **Duplicate `[dependencies]` table**: if the original file already had one and you appended a second one instead of replacing it, `cargo build` fails with `error: could not parse input as TOML ... duplicate key`. Fix by merging into one table.
- **Forgetting `features = ["v4", "serde"]` on `uuid`**: later code calling `Uuid::new_v4()` fails to compile with `error[E0599]: no function or associated item named 'new_v4' found`.
- **Forgetting `dep:` prefix in `[features]`**: this silently creates an unrelated feature flag and does *not* gate the optional dependency, meaning `reqwest` gets pulled in on every default build. Always use `"dep:reqwest"`, not `"reqwest"`.

---

## 4. Section 0.2 — `src/error.rs`

### 4.1 Current purpose of this file

`src/error.rs` already exists and defines the crate's central error type, `MemoliteError` (an enum deriving `thiserror::Error`), plus a `pub type Result<T> = std::result::Result<T, MemoliteError>;` alias used everywhere else in the crate instead of the standard library's `Result`. It already has variants for things like database errors (`#[from] rusqlite::Error`), embedding encode/decode errors, and not-found errors — these came from the pre-existing CRUD/embedding functionality and must not be removed.

### 4.2 Why this modification exists

Step 0 introduces new failure modes that don't map to any existing variant:

- `VectorStore` operations (§0.3) can fail for reasons specific to vector storage (dimension mismatch, non-finite values, backend-specific I/O failures). These need a dedicated `VectorStore(String)` variant so callers can distinguish "the vector backend failed" from "SQLite failed."
- `MemoryEngine`'s constructor and other operations need a generic `InvalidArgument(String)` variant for input validation failures (used pervasively from M3 onward, but the variant must exist now because `open_with_store_internal` in §0.5 already returns it when a caller-supplied store's dimension doesn't match the embedder's).
- `Internal(String)` is needed for "this should never happen" conditions — specifically, **poisoned lock recovery**. Every time this codebase calls `.lock()` or `.read()`/`.write()` on a `Mutex`/`RwLock`, the `Result` returned by the standard library must be handled (a poisoned lock happens if a thread panicked while holding it). V6's uniform pattern is:
  ```rust
  self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?
  ```
  This requires `Internal(String)` to exist.
- `CompensationFailed { operation, compensation }` is needed starting in M3 (not Step 0's own code), but V6 adds it in Step 0 anyway, because Step 0 is defined as "add every error variant the whole plan will need, once." Adding it now costs nothing (unused variants only trigger a `dead_code` warning if literally nothing ever constructs them — and since this is a `pub enum` intended for library consumers, `dead_code` lints do not fire on public items).

### 4.3 Exact location inside the file

Open `src/error.rs`. You will see something resembling:

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MemoliteError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("embedding encode error: {0}")]
    EmbeddingEncode(String),

    #[error("embedding decode error: {0}")]
    EmbeddingDecode(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("uuid parse error: {0}")]
    UuidParse(#[from] uuid::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    // ... possibly more existing variants ...
}

pub type Result<T> = std::result::Result<T, MemoliteError>;
```

**Do not delete or rename any existing variant.** You are only *adding* new arms to the enum.

### 4.4 Insert after the last existing variant, before the closing `}` of the enum

Locate the final existing variant inside `pub enum MemoliteError { ... }` (whatever it happens to be — e.g. `Serde(#[from] serde_json::Error),`) and paste the following block immediately after it, still inside the enum's braces, before the enum's closing `}`:

```rust
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("vector store error: {0}")]
    VectorStore(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// An operation failed, and the automatic rollback/compensation step
    /// that was supposed to clean up after it *also* failed. Both messages
    /// are preserved so an operator isn't left guessing which half broke.
    /// Not constructed by any Step 0 code — added now because V6 treats
    /// Step 0 as the single place every error variant used anywhere in the
    /// whole plan is declared, so later milestones (M3 onward) never need
    /// to touch this enum again.
    #[error("operation failed: {operation}; compensation also failed: {compensation}")]
    CompensationFailed { operation: String, compensation: String },
```

### 4.5 Full resulting enum (for reference — this is what the enum looks like after the edit, assuming the pre-existing variants shown in §4.3; your actual pre-existing variants may differ slightly, but the shape is the same)

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MemoliteError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("embedding encode error: {0}")]
    EmbeddingEncode(String),

    #[error("embedding decode error: {0}")]
    EmbeddingDecode(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("uuid parse error: {0}")]
    UuidParse(#[from] uuid::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("vector store error: {0}")]
    VectorStore(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("operation failed: {operation}; compensation also failed: {compensation}")]
    CompensationFailed { operation: String, compensation: String },
}

pub type Result<T> = std::result::Result<T, MemoliteError>;
```

### 4.6 Explanation of every new variant

| Variant | Kind | Fields | Meaning |
|---|---|---|---|
| `InvalidArgument(String)` | tuple variant | one `String` — a human-readable message | Constructed whenever a caller passes a value that fails validation (empty content, out-of-range importance, mismatched vector dimension at `open_with_store`). The `String` is the specific reason, e.g. `"supplied vector store has dimension 128 but the embedder produces 384"`. |
| `VectorStore(String)` | tuple variant | one `String` | Wraps any failure originating inside a `VectorStore` implementation — dimension mismatches, non-finite floats, or (starting M11) HTTP failures. Kept as a `String` rather than a nested error type because different backends have unrelated underlying error types (`reqwest::Error` for HTTP, nothing for in-memory), and a `String` is the simplest common denominator. |
| `Internal(String)` | tuple variant | one `String` | Reserved for conditions that indicate a bug or an unrecoverable runtime state, most commonly a poisoned `Mutex`/`RwLock`. A poisoned lock means some other thread panicked while holding it; recovering with `.into_inner()` would silently propagate a possibly-corrupt state, so this codebase always turns lock poisoning into a hard `Err` instead. |
| `CompensationFailed { operation, compensation }` | struct variant | `operation: String`, `compensation: String` | Represents a "double failure": some multi-step operation (e.g. writing to SQLite then to the vector store) failed at the second step, *and* the automatic rollback of the first step also failed. Both the original error and the rollback error are preserved as strings so nothing is silently swallowed. Not used by any Step 0 code path, but required to exist because it's part of the crate's complete error surface per V6. |

### 4.7 Checkpoint 0.2

```bash
cargo build
```

**Expected result:** succeeds with no new errors. You may see a `warning: variant is never constructed: 'CompensationFailed'` **only if** `MemoliteError` is a private/non-`pub` type or if clippy's dead-code lint is unusually strict; for a `pub enum` exposed from a library crate this warning normally does **not** fire, because external crates could construct it. If you do see such a warning, it is harmless at this stage and will disappear once M3 starts constructing these variants — do not attempt to silence it with `#[allow(dead_code)]`.

```bash
cargo test
```

**Expected result:** all pre-existing tests still pass; nothing new to test yet since no new variant has behavior beyond being an enum arm.

### 4.8 Common mistakes

- **Forgetting the trailing comma** after the last variant before your insertion — Rust enums require commas between variants; a missing comma produces `error: expected ',', found ...`.
- **Renaming an existing variant by accident** while scrolling/editing — always diff your file against the original before moving on.
- **Adding `#[error(...)]` without the derive macro's placeholder syntax matching the field names** — for the struct variant `CompensationFailed`, the `#[error("... {operation} ... {compensation}")]` string must reference the *exact* field names (`operation`, `compensation`), not positional `{0}`/`{1}`, because it's a struct-style variant, not a tuple-style variant.

---

## 5. Section 0.3 — `src/vector_store/mod.rs` (NEW FILE)

### 5.1 Why this file exists

This is the single most important file in Step 0. It defines the **seam** between Memolite's engine logic and any concrete way of storing/searching vectors. V6's entire reconciliation architecture (used by restart recovery, `forget()`, and later compression rebuilds) depends on every backend implementing one common contract, especially the `replace_all` method. Without this trait existing first, nothing else in Step 0 (or any later milestone) can be written, because `engine.rs`'s struct (§0.5) stores an `Arc<dyn VectorStore>`.

This directly fixes the review finding that earlier plan versions referenced `VectorStore`/`InMemoryVectorStore` from `engine.rs` before the trait existed anywhere in the document — an ordering bug. In this handbook, the trait is created *first*, before anything depends on it.

### 5.2 Is this a new file or existing file?

**New file.** Neither the file nor the `src/vector_store/` directory exists yet in the repository. You must create the directory and the file.

### 5.3 Exact path

```text
src/vector_store/mod.rs
```

Creating `mod.rs` inside a new `vector_store/` subdirectory is Rust's convention for a module that itself contains submodules (here, `in_memory` from §0.4, and later `generic_http` in M11). `src/vector_store/mod.rs` is the "root" of the `vector_store` module; anything declared `pub mod in_memory;` inside it pulls in `src/vector_store/in_memory.rs`.

### 5.4 Complete file contents

Create the file with exactly this content:

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use serde_json::Value;
use uuid::Uuid;
use crate::error::{MemoliteError, Result};

pub mod in_memory;
pub use in_memory::InMemoryVectorStore;

#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: Uuid,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct VectorEntry {
    pub id: Uuid,
    pub vector: Vec<f32>,
    pub metadata: HashMap<String, Value>,
}

/// Shared by every backend's `insert`/`search`/`replace_all`. There is
/// exactly one validation function in the whole crate; no method on any
/// backend is allowed to skip calling it just because a sibling method
/// already validates something similar. This closes a class of bugs where
/// one method (e.g. `replace_all`) validated dimension/finiteness but a
/// sibling method (e.g. `search`) forgot to, silently accepting malformed
/// input.
pub fn validate_vector(label: &str, v: &[f32], dim: usize) -> Result<()> {
    if v.len() != dim {
        return Err(MemoliteError::VectorStore(format!(
            "{label} has dimension {} but store expects {dim}", v.len()
        )));
    }
    if !v.iter().all(|x| x.is_finite()) {
        return Err(MemoliteError::VectorStore(format!("{label} contains a non-finite value")));
    }
    Ok(())
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    /// MUST be an idempotent upsert. MUST call `validate_vector` on `vector`
    /// before storing anything.
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    /// MUST call `validate_vector` on `query` before searching.
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn contains(&self, id: Uuid) -> Result<bool>;
    async fn clear(&self) -> Result<()>;
    /// Replaces the *entire* contents of this store with exactly `entries`.
    /// Any id currently present but absent from `entries` MUST be gone
    /// afterward; every id in `entries` MUST be present and correct
    /// afterward. MUST call `validate_vector` on every entry before storing
    /// anything (all-or-nothing: a bad entry rejects the whole call).
    ///
    /// This is the single reconciliation primitive used everywhere the
    /// engine needs to make the vector index agree with SQLite: restart
    /// backfill, forget-time cleanup after a partial failure, and (in a
    /// later milestone) an index rebuild all call this one method. The
    /// engine never constructs a new backend instance itself, so a rebuild
    /// can never silently swap a remote backend out for an in-memory one.
    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

### 5.5 Explanation of every import

| Import | Why it's needed |
|---|---|
| `use async_trait::async_trait;` | Brings in the `#[async_trait]` attribute macro applied below to `trait VectorStore`. Without it, `async fn` inside a `trait` either fails to compile (in editions without native async-trait support) or produces a trait that cannot be used as `dyn VectorStore` (a "trait object"), which `MemoryEngine` requires (§0.5 stores `Arc<dyn VectorStore>`). |
| `use std::collections::HashMap;` | `VectorEntry.metadata` and the `insert`/`replace_all` signatures use `HashMap<String, Value>` for arbitrary key-value metadata. |
| `use serde_json::Value;` | The type used for metadata values — supports strings, numbers, booleans, arrays, nested objects, matching whatever a caller might want to attach to a memory. |
| `use uuid::Uuid;` | Every vector is identified by a `Uuid`, matching the `Memory.id` field in `memory.rs`. |
| `use crate::error::{MemoliteError, Result};` | Pulls in the crate's error enum and its `Result<T>` alias (§0.2) — every trait method returns this `Result`. |
| `pub mod in_memory;` | Declares that `src/vector_store/in_memory.rs` (§0.4) is a submodule of `vector_store`. **This line will cause a compile error until §0.4's file exists** — this is why §0.4 must be completed before you try to build. |
| `pub use in_memory::InMemoryVectorStore;` | Re-exports `InMemoryVectorStore` so callers can write `crate::vector_store::InMemoryVectorStore` instead of the longer `crate::vector_store::in_memory::InMemoryVectorStore`. Also required for `lib.rs`'s own re-export (§0.9) to work. |

### 5.6 Explanation of every struct

#### `VectorHit`
```rust
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: Uuid,
    pub score: f32,
}
```
Represents one result of a similarity search: which memory (`id`) and how similar it was to the query (`score`, typically a cosine similarity in `[-1.0, 1.0]`, though backends are not required to strictly enforce that range). `#[derive(Debug, Clone)]` lets you print it for debugging and cheaply copy it around (cloning a `Uuid` and an `f32` is trivial — no heap allocation).

#### `VectorEntry`
```rust
#[derive(Debug, Clone)]
pub struct VectorEntry {
    pub id: Uuid,
    pub vector: Vec<f32>,
    pub metadata: HashMap<String, Value>,
}
```
Represents one complete "row" of data needed to fully reconstruct an index entry from scratch: the memory's identity, its embedding, and any metadata that should travel alongside the vector in the backend (used by backends that support metadata-filtered search, though the in-memory backend in §0.4 stores it but doesn't yet use it for filtering). This is the unit that `replace_all` operates on — a `Vec<VectorEntry>` is "the entire desired state of the index."

### 5.7 Explanation of the function `validate_vector`

```rust
pub fn validate_vector(label: &str, v: &[f32], dim: usize) -> Result<()> {
    if v.len() != dim {
        return Err(MemoliteError::VectorStore(format!(
            "{label} has dimension {} but store expects {dim}", v.len()
        )));
    }
    if !v.iter().all(|x| x.is_finite()) {
        return Err(MemoliteError::VectorStore(format!("{label} contains a non-finite value")));
    }
    Ok(())
}
```

- **Signature**: takes a `label: &str` (a human-readable name used only in the error message, e.g. `"query"` or `"vector for <uuid>"`), the vector `v: &[f32]` to check, and `dim: usize`, the dimension the store expects (this is always `embedder.dimension()`, 384 for the FastEmbed model this crate uses).
- **Return type**: `Result<()>` — either `Ok(())` (valid) or `Err(MemoliteError::VectorStore(..))` (invalid). It never panics.
- **Check 1 — dimension**: `v.len() != dim`. Every vector operation in this crate assumes fixed-dimension vectors (because cosine similarity between vectors of different lengths is undefined and `.zip()` would silently truncate to the shorter one, corrupting results without erroring). If they don't match, return immediately with a descriptive error, e.g. `"query has dimension 128 but store expects 384"`.
- **Check 2 — finiteness**: `v.iter().all(|x| x.is_finite())`. `f32::is_finite()` returns `false` for `NaN`, `+inf`, and `-inf`. Any of these values would silently corrupt cosine-similarity math (e.g. `NaN` propagates through every subsequent comparison and sorting operation, and Rust's `f32::total_cmp` — used later for sorting — has defined-but-surprising behavior for `NaN`). Rejecting non-finite values here means every caller downstream can assume all stored/queried vectors are well-formed.
- **Why it lives here, not in each backend**: this is the fix for a documented review finding — earlier drafts validated dimension/finiteness only inside `replace_all`, and `insert`/`search` (both on `InMemoryVectorStore` and later on the HTTP backend) silently skipped validation, because there was no single, mandatory call site. By making `validate_vector` a free function that every method (in §0.4, and later in M11) is *documented and code-reviewed* to call, there is one source of truth for what "valid" means.

### 5.8 Explanation of the trait `VectorStore`

```rust
#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn contains(&self, id: Uuid) -> Result<bool>;
    async fn clear(&self) -> Result<()>;
    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

- **`trait VectorStore: Send + Sync`**: The `: Send + Sync` supertrait bound means any concrete type implementing `VectorStore` must itself be safely shareable across threads (`Sync`) and transferable to another thread (`Send`). This is required because `MemoryEngine` (§0.5) stores its vector store behind `Arc<dyn VectorStore>`, and `Arc<T>` is only `Send + Sync` itself if `T: Send + Sync`. Since `MemoryEngine` will later be wrapped in `Arc<MemoryEngine>` and used from multiple async tasks (e.g. streaming ingestion in M8, maintenance in M10), every field's type must support this.
- **`#[async_trait]`**: Applied at the trait definition. Every implementation of this trait (§0.4's `InMemoryVectorStore`, and later M11's `GenericHttpVectorStore`) must *also* have `#[async_trait]` applied to its `impl VectorStore for X { ... }` block — the macro must be present on both sides.
- **`async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>;`**
  - **Input**: `id` — which memory this vector belongs to. `vector: &[f32]` — a borrowed slice (not owned `Vec<f32>`), because the caller (the engine) already owns the vector and inserting shouldn't require transferring ownership or an extra clone at the call site (the implementation can clone internally if it needs to store it, which `InMemoryVectorStore` does). `metadata` — owned `HashMap`, since the store needs to keep it.
  - **Output**: `Result<()>` — success or a `MemoliteError`.
  - **Contract (documented, not enforced by the compiler)**: "MUST be an idempotent upsert" — calling `insert` twice with the same `id` but different vectors must result in the second vector replacing the first, not an error and not a duplicate entry. "MUST call `validate_vector`" — every implementation is required, by convention/code-review (not by the type system), to call `validate_vector` before storing anything.
  - **Side effects**: mutates the backend's internal storage.
  - **Thread safety**: `&self`, not `&mut self` — this means concurrent calls to `insert` from multiple threads must be safe without an external mutex, which is why `InMemoryVectorStore` (§0.4) uses an internal `RwLock` rather than requiring the caller to synchronize access.
- **`async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;`**
  - **Input**: `query` — the vector to search for nearest neighbors of. `k` — the maximum number of results to return.
  - **Output**: `Result<Vec<VectorHit>>` — up to `k` hits, expected to be sorted by descending similarity (score), though the trait itself doesn't encode ordering in the type system — it's a documented contract implementations must honor.
  - **Complexity**: unspecified by the trait; `InMemoryVectorStore`'s implementation (§0.4) is O(n) per search (linear scan over every stored vector) — this is documented as an "honest limitation" of the in-memory backend, not something the trait hides.
- **`async fn delete(&self, id: Uuid) -> Result<()>;`** — removes one entry by id. No error is required if the id doesn't exist (idempotent-delete semantics are the convention used by `InMemoryVectorStore`, matching how SQL `DELETE` behaves).
- **`async fn contains(&self, id: Uuid) -> Result<bool>;`** — existence check without retrieving the vector itself.
- **`async fn clear(&self) -> Result<()>;`** — removes every entry.
- **`async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;`** — see the doc-comment above the method for the full contract; this is elaborated in §5.9 below because it's the most important method on the trait.
- **`fn dimension(&self) -> usize;`** — **not** `async` (note the missing `async` keyword and the missing `.await` requirement at call sites) — this is a pure, synchronous accessor returning the fixed embedding dimension this store was configured for (e.g. `384`). It's synchronous because computing/returning it never requires I/O — for `InMemoryVectorStore` it's just returning a stored `usize` field.

### 5.9 Why `replace_all` is the most important method

`replace_all` is the single mechanism that answers the question "make the vector index agree exactly with what SQLite says is true." Every reconciliation scenario in the whole V6 plan funnels through this one method:

- **Restart**: when `MemoryEngine::open()` runs (§0.5), the in-memory vector index is empty (RAM was wiped by the process restart) — `reconcile_vector_index` (§0.8) reads every memory+embedding pair back out of SQLite and calls `replace_all` to rebuild the index from scratch.
- **`forget()` failure recovery** (introduced in M3, not Step 0, but relying on this same primitive): if deleting a single vector fails, the whole index is reconciled from SQLite again via `replace_all`, guaranteeing correctness even though the exact incremental delete failed.
- **Compression rebuild** (M9): after summarizing and superseding old memories, the index can be rebuilt wholesale via the same method.

Because `replace_all` is defined once on the trait and each backend implements it however is correct for that backend (`InMemoryVectorStore` swaps an entire `HashMap` under one write-lock; a hypothetical remote backend would call a bulk-replace HTTP endpoint), **the engine code that calls `replace_all` never needs to know or care which backend is active.** This is what makes rebuilds "backend-agnostic."

### 5.10 Checkpoint 0.3

At this point, `src/vector_store/mod.rs` exists but references `pub mod in_memory;`, and `src/vector_store/in_memory.rs` does not exist yet. **`cargo build` will fail right now** with an error resembling:

```text
error[E0583]: file not found for module `in_memory`
 --> src/vector_store/mod.rs:7:1
  |
7 | pub mod in_memory;
  | ^^^^^^^^^^^^^^^^^^
```

**This is expected and correct.** Do not be alarmed. This file is also not yet registered in `lib.rs`, so even this error won't surface until `lib.rs` is updated (§0.9) — but if you've been building incrementally with `cargo check` pointed only at this file, this is the exact error you'd see. Continue directly to §0.4 without trying to `cargo build` the whole crate yet — there is no meaningful checkpoint to run until §0.4 exists, since `vector_store` isn't registered in `lib.rs` until §0.9 anyway. If you want a sanity check that the file at least *parses* as valid Rust syntax right now, you can run:

```bash
rustc --edition 2021 --crate-type lib src/vector_store/mod.rs 2>&1 | head -30
```

This will report the missing-module error above (because `in_memory` doesn't exist) but will **not** report any *syntax* errors in the trait/struct definitions themselves if you pasted them correctly. If you see syntax errors (unexpected token, mismatched braces), fix those before proceeding — do not carry a syntax error forward into §0.4.

---

## 6. Section 0.4 — `src/vector_store/in_memory.rs` (NEW FILE)

### 6.1 Why this file exists

`vector_store/mod.rs` declares a *trait* — an interface with no behavior. Something must actually implement it, or the trait is useless and nothing compiles that tries to construct a concrete `VectorStore`. `InMemoryVectorStore` is V6's default, zero-external-dependency implementation: a `HashMap` guarded by a `RwLock`, computing cosine similarity by brute-force linear scan. It is what `MemoryEngine::open()` (§0.5) constructs by default when no other backend is supplied.

This file directly satisfies the `pub mod in_memory;` declaration from §0.3 — without it, that line fails to compile (§5.10's expected error).

### 6.2 Is this a new file or existing file?

**New file.**

### 6.3 Exact path

```text
src/vector_store/in_memory.rs
```

### 6.4 Complete file contents

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use serde_json::Value;
use uuid::Uuid;
use crate::error::{MemoliteError, Result};
use super::{validate_vector, VectorEntry, VectorHit, VectorStore};

pub struct InMemoryVectorStore {
    dim: usize,
    data: RwLock<HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>,
}

impl InMemoryVectorStore {
    pub fn new(dim: usize) -> Self {
        Self { dim, data: RwLock::new(HashMap::new()) }
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 { return 0.0; }
        dot / (na * nb)
    }

    fn lock_read(&self) -> Result<std::sync::RwLockReadGuard<'_, HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>> {
        self.data.read().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))
    }
    fn lock_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>> {
        self.data.write().map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))
    }
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()> {
        validate_vector(&format!("vector for {id}"), vector, self.dim)?;
        self.lock_write()?.insert(id, (vector.to_vec(), metadata));
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        validate_vector("query", query, self.dim)?;
        let guard = self.lock_read()?;
        let mut hits: Vec<VectorHit> = guard.iter()
            .map(|(id, (v, _))| VectorHit { id: *id, score: Self::cosine(query, v) })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(k);
        Ok(hits)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        self.lock_write()?.remove(&id);
        Ok(())
    }

    async fn contains(&self, id: Uuid) -> Result<bool> {
        Ok(self.lock_read()?.contains_key(&id))
    }

    async fn clear(&self) -> Result<()> {
        self.lock_write()?.clear();
        Ok(())
    }

    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()> {
        let mut replacement = HashMap::with_capacity(entries.len());
        for e in entries {
            validate_vector(&format!("entry for {}", e.id), &e.vector, self.dim)?;
            replacement.insert(e.id, (e.vector, e.metadata));
        }
        *self.lock_write()? = replacement;
        Ok(())
    }

    fn dimension(&self) -> usize { self.dim }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nearest_vector_ranks_first() {
        let store = InMemoryVectorStore::new(2);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        store.insert(a, &[1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert(b, &[0.0, 1.0], HashMap::new()).await.unwrap();
        assert_eq!(store.search(&[1.0, 0.0], 1).await.unwrap()[0].id, a);
    }

    #[tokio::test]
    async fn insert_is_an_upsert() {
        let store = InMemoryVectorStore::new(2);
        let id = Uuid::new_v4();
        store.insert(id, &[1.0, 0.0], HashMap::new()).await.unwrap();
        store.insert(id, &[0.0, 1.0], HashMap::new()).await.unwrap();
        assert_eq!(store.data.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn wrong_dimension_insert_is_rejected() {
        let store = InMemoryVectorStore::new(3);
        assert!(store.insert(Uuid::new_v4(), &[1.0, 0.0], HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn non_finite_insert_is_rejected() {
        let store = InMemoryVectorStore::new(2);
        assert!(store.insert(Uuid::new_v4(), &[f32::NAN, 0.0], HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn wrong_dimension_query_is_rejected_not_silently_truncated() {
        let store = InMemoryVectorStore::new(3);
        store.insert(Uuid::new_v4(), &[1.0, 0.0, 0.0], HashMap::new()).await.unwrap();
        assert!(store.search(&[1.0, 0.0], 1).await.is_err());
    }

    #[tokio::test]
    async fn non_finite_query_is_rejected() {
        let store = InMemoryVectorStore::new(2);
        assert!(store.search(&[f32::INFINITY, 0.0], 1).await.is_err());
    }

    #[tokio::test]
    async fn replace_all_removes_ids_absent_from_the_new_set() {
        let store = InMemoryVectorStore::new(2);
        let stale = Uuid::new_v4();
        store.insert(stale, &[1.0, 0.0], HashMap::new()).await.unwrap();
        let kept = Uuid::new_v4();
        store.replace_all(vec![VectorEntry { id: kept, vector: vec![0.0, 1.0], metadata: HashMap::new() }]).await.unwrap();
        assert!(!store.contains(stale).await.unwrap());
        assert!(store.contains(kept).await.unwrap());
    }

    #[tokio::test]
    async fn replace_all_leaves_store_untouched_on_validation_failure() {
        let store = InMemoryVectorStore::new(2);
        let original = Uuid::new_v4();
        store.insert(original, &[1.0, 0.0], HashMap::new()).await.unwrap();
        let bad = VectorEntry { id: Uuid::new_v4(), vector: vec![1.0], metadata: HashMap::new() };
        assert!(store.replace_all(vec![bad]).await.is_err());
        assert!(store.contains(original).await.unwrap());
    }
}
```

### 6.5 Explanation of every import

| Import | Why |
|---|---|
| `async_trait::async_trait` | Needed on the `impl VectorStore for InMemoryVectorStore` block, mirroring the trait's own attribute. |
| `std::collections::HashMap` | The underlying storage container. |
| `std::sync::RwLock` | A reader-writer lock: many concurrent readers (`search`, `contains`) OR one exclusive writer (`insert`, `delete`, `clear`, `replace_all`) at a time. Chosen over `Mutex` because search is the hottest, most frequent operation and should not be serialized against other concurrent searches. |
| `serde_json::Value` | Metadata value type, matching `vector_store/mod.rs`. |
| `uuid::Uuid` | Entry identity type. |
| `crate::error::{MemoliteError, Result}` | Error handling, same as `mod.rs`. |
| `super::{validate_vector, VectorEntry, VectorHit, VectorStore}` | `super` refers to the parent module, `vector_store` (i.e., `mod.rs`, §0.3). This imports the trait being implemented, the two structs used in its signatures, and the shared validation helper. |

### 6.6 Explanation of the struct `InMemoryVectorStore`

```rust
pub struct InMemoryVectorStore {
    dim: usize,
    data: RwLock<HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>,
}
```

- **`dim: usize`** — the fixed embedding dimension this store instance was created for (set once at construction, never changed). Every vector passed to `insert`/`search`/`replace_all` is checked against this value via `validate_vector`.
- **`data: RwLock<HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>`** — the actual storage. The key is the memory's `Uuid`. The value is a tuple `(Vec<f32>, HashMap<String, Value>)` — the embedding vector and its associated metadata, stored together so a single lookup by id retrieves both. Both fields are **private** (no `pub` keyword) — external code must go through the trait methods, never touch `data` directly. (The unit tests below can access `store.data` directly only because `#[cfg(test)] mod tests { use super::*; ... }` is defined *inside the same file*, so it has access to private fields of the enclosing module — this is a standard Rust idiom for white-box unit testing, not a violation of encapsulation from the outside.)

### 6.7 Explanation of every method

#### `InMemoryVectorStore::new(dim: usize) -> Self`
Constructor. Takes the dimension and creates an empty `HashMap` wrapped in a fresh `RwLock`. This is **not** part of the `VectorStore` trait (the trait has no notion of "how to construct a backend" — that's backend-specific, which is exactly why the engine's `open()` needs to know concretely about `InMemoryVectorStore` when constructing the *default* backend, even though everywhere else it only talks to `dyn VectorStore`).

#### `InMemoryVectorStore::cosine(a: &[f32], b: &[f32]) -> f32` (private, `fn`, not `async fn`)
Computes cosine similarity between two equal-length vectors:
```text
cosine(a, b) = (a · b) / (‖a‖ × ‖b‖)
```
- `dot`: the dot product, `Σ aᵢ·bᵢ`, computed via `.iter().zip(b.iter()).map(|(x,y)| x*y).sum()`.
- `na`, `nb`: the Euclidean norms (magnitudes) of each vector, `√(Σ aᵢ²)`.
- **Zero-vector guard**: `if na == 0.0 || nb == 0.0 { return 0.0; }` — dividing by zero would produce `NaN`/`inf`; instead, a zero-magnitude vector is defined to have zero similarity to everything, which is a safe, sane default (a real embedding from FastEmbed should never actually be the zero vector, but defending against it costs nothing).
- This is a private helper (no `pub`), used only inside `search`.

#### `InMemoryVectorStore::lock_read(&self) -> Result<RwLockReadGuard<...>>` and `lock_write(&self) -> Result<RwLockWriteGuard<...>>`
Both are private helpers that wrap `self.data.read()` / `self.data.write()` (which return `std::sync::LockResult<Guard>`, i.e. `Result<Guard, PoisonError<Guard>>`) and convert the poison error into `MemoliteError::Internal(...)` via `.map_err(...)`. Every other method in this file calls these helpers instead of calling `.read()`/`.write()` directly, so lock-poisoning error handling is written exactly once.

#### `insert(&self, id: Uuid, vector: &[f32], metadata: HashMap<String, Value>) -> Result<()>`
1. Calls `validate_vector(&format!("vector for {id}"), vector, self.dim)?` — checks dimension and finiteness; the `?` operator propagates any `Err` immediately, so if validation fails, nothing below runs and no partial mutation happens.
2. `self.lock_write()?.insert(id, (vector.to_vec(), metadata));` — acquires the write lock (blocking other readers/writers until it's released, which happens automatically when the returned guard is dropped at the end of the statement), then calls `HashMap::insert`, which — per `HashMap`'s own documented behavior — **replaces** any existing value for that key. This is what makes the operation an "idempotent upsert" as required by the trait's doc comment: inserting the same `id` twice just overwrites.
3. `vector.to_vec()` — converts the borrowed `&[f32]` into an owned `Vec<f32>` so it can be stored (the `HashMap` needs to own its values).

#### `search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>`
1. Validates the query vector — **this is the fix for the historical "search doesn't validate its query" bug**: earlier drafts of this backend validated vectors in `insert`/`replace_all` but not in `search`, meaning a wrong-length query would silently be `.zip()`-truncated against every stored vector instead of erroring. Calling `validate_vector` here first closes that gap.
2. Acquires a **read** lock (allows concurrent searches from multiple threads simultaneously, since none of them mutate).
3. Builds `hits: Vec<VectorHit>` by iterating every stored `(id, (vector, metadata))` pair, computing `Self::cosine(query, v)` for each, and discarding the metadata (`_`) since search results only need id+score.
4. Sorts descending by score: `hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)))`.
   - `b.score.total_cmp(&a.score)` — note the reversed order (`b` compared against `a`) — this produces a **descending** sort (highest score first). `total_cmp` is used instead of `partial_cmp` because `f32` doesn't implement `Ord` (due to `NaN`), and `total_cmp` provides a total, deterministic ordering even in edge cases (though `validate_vector` should already have excluded `NaN` from ever being stored or queried).
   - `.then_with(|| a.id.cmp(&b.id))` — a deterministic tie-breaker: if two entries have exactly equal scores, sort by `Uuid` ascending. Without this, `sort_by`'s behavior for equal-score entries would depend on the unspecified internal order of `HashMap` iteration, making test assertions and real behavior non-reproducible across runs.
5. `hits.truncate(k)` — keeps only the first `k` elements (the highest-scoring ones, since the vector is already sorted descending), discarding the rest. If `k` is larger than the number of hits, this is a no-op (truncate never panics if `k >= len`).

#### `delete(&self, id: Uuid) -> Result<()>`
Acquires the write lock and calls `HashMap::remove`, which is a no-op (returns `None` internally, but this method discards that and always returns `Ok(())`) if the id isn't present — matching the documented idempotent-delete convention.

#### `contains(&self, id: Uuid) -> Result<bool>`
Read-lock, `HashMap::contains_key`.

#### `clear(&self) -> Result<()>`
Write-lock, `HashMap::clear` (removes every entry, keeps the allocated capacity).

#### `replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>`
This is the most carefully designed method in the file, because it must satisfy the trait's strong contract (§5.9). Walk through it:
1. `let mut replacement = HashMap::with_capacity(entries.len());` — builds a **brand new**, separate `HashMap`, sized up-front to avoid reallocation, entirely *off to the side* — the live `self.data` is not touched yet.
2. `for e in entries { validate_vector(...)?; replacement.insert(e.id, (e.vector, e.metadata)); }` — validates and inserts each entry into the side-table `replacement`, not into `self.data`. **If any single entry fails validation, the `?` operator returns immediately from the whole function** — at that point, `replacement` (the half-built side-table) is simply dropped (Rust's ownership system deallocates it automatically), and **`self.data` was never touched, so the live store is left in its original, untouched state.** This is what the doc comment and the test `replace_all_leaves_store_untouched_on_validation_failure` both verify: partial failure never partially corrupts the live index.
3. `*self.lock_write()? = replacement;` — only after every entry has been validated successfully does this line run. It acquires the write lock and then uses the dereference-assignment pattern `*guard = replacement` to **atomically swap** the entire contents of the map in one operation — there is no window where the store is half-old/half-new, because this is a single assignment while holding the exclusive write lock.

This is why the trait's contract can promise "all-or-nothing": the validation loop happens entirely before any mutation, and the mutation itself is a single atomic swap.

#### `dimension(&self) -> usize`
Returns the stored `dim` field. Synchronous (no `async`), since it requires no I/O or locking.

### 6.8 Explanation of the test module

`#[cfg(test)] mod tests { use super::*; ... }` — this whole block is compiled **only** when running `cargo test` (the `#[cfg(test)]` attribute excludes it from normal `cargo build`/release builds entirely, so it adds zero size/cost to the shipped library). `use super::*;` imports everything from the enclosing `in_memory` module (the struct, the trait implementation, and everything `in_memory.rs` itself imported), so tests can refer to `InMemoryVectorStore`, `Uuid`, `HashMap`, `VectorEntry` etc. without re-importing them.

Each test function is annotated `#[tokio::test]` (not `#[test]`) because the function body itself is `async fn` and calls `.await` — `#[tokio::test]` is a macro that wraps the async function body in a small single-threaded Tokio runtime so it can actually be executed as a synchronous test from the test harness's point of view.

| Test | What it proves | Why it matters |
|---|---|---|
| `nearest_vector_ranks_first` | Inserts two orthogonal-ish vectors, searches for the one closer to `[1.0, 0.0]`, asserts the closer one (`a`) is ranked first. | Basic correctness of cosine similarity + sort direction (descending, not ascending). |
| `insert_is_an_upsert` | Inserts the same `id` twice with different vectors, checks `store.data.read().unwrap().len() == 1`. | Proves the trait's "idempotent upsert" contract holds — no duplicate entries accumulate. |
| `wrong_dimension_insert_is_rejected` | 3-dimensional store, inserts a 2-dimensional vector, expects `Err`. | Confirms `validate_vector` is actually being called inside `insert`, not just declared. |
| `non_finite_insert_is_rejected` | Inserts `[NaN, 0.0]`, expects `Err`. | Confirms the finiteness check works, not just the dimension check. |
| `wrong_dimension_query_is_rejected_not_silently_truncated` | 3-dimensional store with a valid stored vector, searches with a 2-dimensional query, expects `Err`. | **This is the specific regression test for the historical bug** where `search()` never validated its query and would silently `.zip()`-truncate mismatched vectors instead of erroring. |
| `non_finite_query_is_rejected` | Searches with `[f32::INFINITY, 0.0]`, expects `Err`. | Same idea as above, for the finiteness check on the query path specifically. |
| `replace_all_removes_ids_absent_from_the_new_set` | Inserts a "stale" id, calls `replace_all` with a completely different id, asserts the stale one is gone (`!contains(stale)`) and the new one is present (`contains(kept)`). | Proves `replace_all` really *replaces* (deletes anything not in the new set), not merely "upserts everything in the list while leaving old entries behind" — this distinction is the entire reason `replace_all` exists as its own method instead of a loop of `insert` calls. |
| `replace_all_leaves_store_untouched_on_validation_failure` | Inserts a valid `original`, then calls `replace_all` with a single malformed entry (wrong dimension), expects `Err`, then asserts `original` is *still* present. | Proves the "all-or-nothing" / "validate everything before mutating anything" behavior described in §6.7 step 2 — a bad `replace_all` call must never partially wipe the store. |

### 6.9 Checkpoint 0.4

At this point, both `src/vector_store/mod.rs` and `src/vector_store/in_memory.rs` exist and reference each other correctly, but `vector_store` is still **not yet registered in `lib.rs`**, so `cargo build` on the whole crate will not yet even attempt to compile this module. To validate this module in isolation before wiring it into `lib.rs`, temporarily add this single line near the top of `src/lib.rs` (you will replace/expand this properly in §0.9 — this is a *temporary* sanity check only):

```rust
pub mod vector_store;
```

Then run:

```bash
cargo build 2>&1 | tail -60
```

**Expected result:** the crate compiles (there may be pre-existing warnings from other files unrelated to your change, which is fine — do not try to fix those). If you see errors originating from `src/vector_store/mod.rs` or `src/vector_store/in_memory.rs`, they will look like one of:

- `error[E0433]: failed to resolve: use of undeclared crate or module 'async_trait'` → you skipped §0.1 (Cargo.toml) or mistyped the dependency name.
- `error[E0107]: missing generics for struct 'HashMap'` → a typo in a type signature; re-copy exactly from §6.4.
- `error[E0499]: cannot borrow '*self.lock_write()?' as mutable more than once` → you altered the `replace_all` body's lock-acquisition pattern; re-copy exactly, keeping the guard on the left of `=` in a single statement.

Then run:

```bash
cargo test vector_store:: 2>&1 | tail -40
```

**Expected result:**
```text
running 8 tests
test vector_store::in_memory::tests::insert_is_an_upsert ... ok
test vector_store::in_memory::tests::nearest_vector_ranks_first ... ok
test vector_store::in_memory::tests::non_finite_insert_is_rejected ... ok
test vector_store::in_memory::tests::non_finite_query_is_rejected ... ok
test vector_store::in_memory::tests::replace_all_leaves_store_untouched_on_validation_failure ... ok
test vector_store::in_memory::tests::replace_all_removes_ids_absent_from_the_new_set ... ok
test vector_store::in_memory::tests::wrong_dimension_insert_is_rejected ... ok
test vector_store::in_memory::tests::wrong_dimension_query_is_rejected_not_silently_truncated ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

All 8 tests must pass before continuing. Do not proceed to §0.5 with any failing test here — the rest of Step 0 assumes this backend is fully correct.

**Leave the temporary `pub mod vector_store;` line in `lib.rs` for now** — §0.9 will show you its final, correct position alongside the other Step-0 module registrations, so you don't need to remove it, only to later confirm it matches exactly.

---

## 7. Section 0.5 — `src/engine.rs`: `MemoryEngine` struct, `BackfillPolicy`, `open()`, `open_with_store_internal()`

### 7.1 Current purpose of this file

`src/engine.rs` already exists and defines `MemoryEngine`, the crate's central orchestrator. In the **current** (pre-Step-0) repository, per V6's own verified baseline note, the struct looks approximately like this:

```rust
pub struct MemoryEngine {
    conn: rusqlite::Connection,
    embedder: std::sync::Mutex<crate::embedder::Embedder>,
}

impl MemoryEngine {
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        // ... existing schema creation, not yet using migrations.rs ...
        let embedder = crate::embedder::Embedder::new()?;
        Ok(Self { conn, embedder: std::sync::Mutex::new(embedder) })
    }

    // ... existing store/recall/forget/get functions using `self.conn` directly,
    //     no vector store field exists yet, recall() is unimplemented (todo!()) ...
}
```

The exact existing body of `open()` and the other methods varies slightly by commit, but the two facts that matter for Step 0 are, per V6's stated baseline: (1) `conn` is a **plain, non-`Mutex`-wrapped** `rusqlite::Connection`, and (2) there is **no `vector_store` field at all** yet.

### 7.2 Why this modification exists

Step 0 must change `MemoryEngine`'s fields to their **final** shape (this final shape is never revisited again in later milestones — every subsequent milestone only *adds methods*, never changes these four fields) and, in the same step, provide a constructor that actually builds a value of that new shape. If you change only the struct and not the constructor together, the crate will not compile (this is precisely the historical review-flagged defect: "Step 0 changes `MemoryEngine`'s fields but the compatible `open()` isn't written until M3, so `cargo build` fails at the Step-0 checkpoint"). This handbook fixes that by presenting the struct and constructor as a **single, inseparable edit** — you make both changes in the same sitting, and you do not run `cargo build` on `engine.rs` in a state where only one of them has happened (well — you may, to observe the intermediate error described in §7.9, but you must not consider Step 0 "done" until both are in place).

The new fields exist for these reasons:

- `conn: std::sync::Mutex<rusqlite::Connection>` — wrapped in a `Mutex` because, starting in M3 (outside Step 0's own methods, but the *type* must be correct now), multiple async operations may need to use the SQLite connection, and `rusqlite::Connection` is not `Sync` on its own — a `Mutex` makes shared access across threads safe by only ever allowing one holder of the lock at a time.
- `embedder: std::sync::Mutex<crate::embedder::Embedder>` — already existed as a `Mutex` in the current repo (FastEmbed's `embed()` call requires `&mut self`, so a `Mutex` is needed to get mutable access from a shared `&self` method); unchanged in shape by Step 0.
- `vector_store: std::sync::RwLock<std::sync::Arc<dyn crate::vector_store::VectorStore>>` — **new field**. This is how the engine holds "whichever backend is currently active" as a trait object. It's wrapped in `RwLock<Arc<...>>` (not just `Arc<...>` directly) because the *reference itself* (which `Arc` is currently stored) could in principle be swapped out (not used anywhere in Step 0's own code, but the shape supports it for forward compatibility); the common-case operation is "read the current `Arc`, clone it (cheap, just a refcount bump), drop the lock immediately, then use the cloned `Arc` to call an `async` method" — this pattern is spelled out precisely in §7.6.
- `maintenance_running: std::sync::Arc<std::sync::atomic::AtomicBool>` — **new field**. Not used by any Step 0 logic, but declared now because it's part of `MemoryEngine`'s **final** shape (per V6 §0.5's own text: "This step changes the struct to... [the final struct]"), so that `M10`'s maintenance controller doesn't need to change the struct definition again later — everything from M3 onward only ever adds `impl` blocks, never touches these four fields.

The `BackfillPolicy` enum is introduced now (even though it's not exercised by any *caller* until M11) because `open_with_store_internal` — the shared, private constructor that both `open()` (Step 0's own concern) and the future `open_with_store()` (M11's public API) will call — takes a `BackfillPolicy` as one of its parameters. Since `open()` must exist and compile in Step 0, and `open()`'s job is to call `open_with_store_internal`, the enum must exist first.

### 7.3 Exact location inside the file

Open `src/engine.rs`. Locate the top of the file (imports), then the `pub struct MemoryEngine { ... }` definition, then the first `impl MemoryEngine { ... }` block containing `pub async fn open(...)`.

### 7.4 Step 7.4a — Replace the struct definition

**Find** the existing struct definition (approximately):

```rust
pub struct MemoryEngine {
    conn: rusqlite::Connection,
    embedder: std::sync::Mutex<crate::embedder::Embedder>,
}
```

**Replace it entirely** with:

```rust
pub struct MemoryEngine {
    conn: std::sync::Mutex<rusqlite::Connection>,
    embedder: std::sync::Mutex<crate::embedder::Embedder>,
    vector_store: std::sync::RwLock<std::sync::Arc<dyn crate::vector_store::VectorStore>>,
    maintenance_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
```

### 7.5 Explanation of every field (final)

| Field | Type | Purpose |
|---|---|---|
| `conn` | `std::sync::Mutex<rusqlite::Connection>` | The single SQLite connection this engine owns. Wrapped in `Mutex` for safe shared access; only one caller may hold the lock (and therefore run a query) at a time. |
| `embedder` | `std::sync::Mutex<crate::embedder::Embedder>` | The FastEmbed wrapper that turns text into `Vec<f32>` embeddings. Wrapped in `Mutex` because `embed()` needs `&mut self` internally. |
| `vector_store` | `std::sync::RwLock<std::sync::Arc<dyn crate::vector_store::VectorStore>>` | The currently-active vector backend, type-erased behind the `VectorStore` trait so the engine's logic never needs to know whether it's talking to `InMemoryVectorStore` or (from M11) a remote HTTP-backed store. |
| `maintenance_running` | `std::sync::Arc<std::sync::atomic::AtomicBool>` | A flag (not used until M10) preventing two background maintenance controllers from running on the same engine simultaneously. `AtomicBool` allows lock-free, thread-safe read/write of a single boolean. Wrapped in `Arc` so a background task (spawned later, in M10) can hold its own clone of the *same* underlying flag even after the engine itself is otherwise inaccessible to that task. |

### 7.6 Step 7.5b — Add `BackfillPolicy` and rewrite the constructor

**Locate** the existing `impl MemoryEngine { pub async fn open(...) -> Result<Self> { ... } }` block. You are going to (1) delete the entire existing body of `open()`, (2) add a new enum `BackfillPolicy` directly above the `impl` block (or directly above the struct — either position is fine as long as it's at module scope, not nested inside `impl`), and (3) add `open_with_store_internal` as a **private** associated function on `MemoryEngine`.

Insert this **immediately above** the struct definition from §7.4 (i.e., before `pub struct MemoryEngine { ... }`):

```rust
/// Controls what `open_with_store()` does to a caller-supplied backend's
/// *existing* remote contents at open time. Not exercised by any Step 0
/// caller (only `open()` exists in Step 0, and it always uses
/// `ReplaceAll` — see `open()` below), but declared now because it is a
/// parameter type of `open_with_store_internal`, the shared constructor
/// both `open()` (Step 0) and the future public `open_with_store()`
/// (Milestone M11) call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillPolicy {
    /// Do not touch the remote store's contents at all. Local SQLite rows
    /// with no matching remote vector will simply fail to recall until a
    /// later explicit rebuild is performed. Safe default for a backend
    /// shared with other data.
    ExistingOnly,
    /// Upsert every local row into the store via `insert`, but never
    /// delete anything the store already has that SQLite doesn't know
    /// about. Safe for a shared backend; brings this database's own rows
    /// up to date without touching anyone else's.
    UpsertLocal,
    /// Call `replace_all` so the store's contents become *exactly* this
    /// database's rows — anything else present is deleted. Only correct
    /// when this backend/collection is dedicated exclusively to this one
    /// Memolite database.
    ReplaceAll,
}
```

Now **replace** the entire existing `pub async fn open(...) { ... }` method body (delete everything from `pub async fn open` through its matching closing `}`) with:

```rust
impl MemoryEngine {
    /// Opens (or creates) the local SQLite file, backed by the default
    /// in-memory vector store. `ReplaceAll` is correct and safe here
    /// specifically because the in-memory store constructed inside
    /// `open_with_store_internal` when `store_override` is `None` is
    /// private to this one engine instance; nothing else could ever be
    /// sharing it, so there is no risk in making its contents exactly
    /// mirror SQLite on every open.
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open_with_store_internal(path, None, BackfillPolicy::ReplaceAll).await
    }

    /// Shared constructor. `store_override`:
    ///   - `None`   -> construct a fresh `InMemoryVectorStore` sized to the
    ///                 embedder's dimension (this is what `open()` does).
    ///   - `Some(s)` -> use the caller-supplied backend `s` instead (used
    ///                 starting Milestone M11's public `open_with_store()`;
    ///                 not called with `Some` by anything in Step 0).
    /// `backfill` controls how the vector index is reconciled against
    /// SQLite at open time — see `BackfillPolicy` and `reconcile_vector_index`.
    async fn open_with_store_internal(
        path: impl AsRef<std::path::Path>,
        store_override: Option<std::sync::Arc<dyn crate::vector_store::VectorStore>>,
        backfill: BackfillPolicy,
    ) -> Result<Self> {
        let mut raw_conn = rusqlite::Connection::open(path)?;
        crate::migrations::run_migrations(&mut raw_conn)?;
        let embedder = crate::embedder::Embedder::new()?;
        let dim = embedder.dimension();

        let vector_store: std::sync::Arc<dyn crate::vector_store::VectorStore> = match store_override {
            Some(store) => {
                if store.dimension() != dim {
                    return Err(MemoliteError::InvalidArgument(format!(
                        "supplied vector store has dimension {} but the embedder produces {}",
                        store.dimension(), dim
                    )));
                }
                store
            }
            None => std::sync::Arc::new(crate::vector_store::InMemoryVectorStore::new(dim)),
        };
        let conn = std::sync::Mutex::new(raw_conn);
        reconcile_vector_index(&conn, &vector_store, backfill).await?;

        Ok(Self {
            conn,
            embedder: std::sync::Mutex::new(embedder),
            vector_store: std::sync::RwLock::new(vector_store),
            maintenance_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}
```

**Do not add any other method to this `impl MemoryEngine` block in Step 0.** `store()`, `recall()`, `forget()`, etc. are all M3 — out of scope here. If your pre-existing `engine.rs` already had those methods (from before Step 0), you may **leave them in the file for now, but they will not compile yet**, because they reference `self.conn` as if it were a bare `Connection` rather than a `Mutex<Connection>`, and they don't know about `vector_store` at all. This is expected and is resolved in M3, not Step 0. If this bothers you or blocks your ability to reach a green `cargo build` for Step 0's own checkpoint, the pragmatic fix — consistent with V6's own incremental-commit philosophy ("keeping each intermediate commit compiling") — is to **comment out or temporarily `#[allow(dead_code)]`-gate the pre-existing `store`/`recall`/`forget`/`get` method bodies** for the duration of Step 0, and restore/rewrite them properly when you reach M3. This handbook does not cover how to rewrite them (that's M3's job) — it only tells you that Step 0's own checkpoint requires the *file as a whole* to compile, and pre-existing methods that reference the old field shapes must not be left in a broken state while you validate Step 0.

### 7.7 Explanation of the enum `BackfillPolicy`

| Variant | Meaning | When it's chosen |
|---|---|---|
| `ExistingOnly` | Do nothing to the remote store's contents at open time. | Never chosen by any Step 0 code (only relevant starting M11, when callers can choose it explicitly). |
| `UpsertLocal` | Add/update this database's own rows in the store, but never delete anything else present. | Never chosen by any Step 0 code. |
| `ReplaceAll` | Make the store's contents *exactly* equal to this database's rows — deletes anything not in SQLite. | **Always** chosen by `open()` in Step 0, because the default `InMemoryVectorStore` it constructs is private to this one engine — nothing else could possibly be sharing it, so wiping it to exactly match SQLite is always safe. |

`#[derive(Debug, Clone, Copy, PartialEq, Eq)]` — `Debug` lets you `{:?}`-print it for logging/debugging; `Clone, Copy` mean passing it around is a trivial bitwise copy (it holds no heap data, just a discriminant), so you never need to worry about ownership when passing a `BackfillPolicy` into a function; `PartialEq, Eq` let you compare two values with `==`, used later (M11's tests) to assert which policy was actually applied.

### 7.8 Explanation of every function

#### `MemoryEngine::open(path: impl AsRef<Path>) -> Result<Self>`
- **Input**: `path` — anything that can be viewed as a filesystem `Path` (a `&str`, `String`, `PathBuf`, etc. all work, thanks to `impl AsRef<std::path::Path>` — this is idiomatic Rust for "accept many ownership/string-like types without forcing the caller to convert first").
- **Output**: `Result<Self>` — either a fully-constructed, ready-to-use `MemoryEngine`, or a `MemoliteError`.
- **Ownership**: takes `path` by value (but since it's a generic bound, not a concrete owned type, the actual ownership semantics depend on what the caller passes — e.g. passing a `&str` literal is fine, no cloning required by the caller).
- **Thread safety**: this is an `async fn`; it does not block the calling thread while awaiting `reconcile_vector_index` (§0.8) internally, though the SQLite operations themselves (via `rusqlite`, which is synchronous) do block whichever thread executes them for their duration — this is a known, accepted characteristic of `rusqlite`-based code and is not something Step 0 attempts to fix.
- **Failure cases**: SQLite file cannot be opened/created (permissions, disk full, corrupt file) → `rusqlite::Error` wrapped via `#[from]` into `MemoliteError::Database`; migrations fail → same; the embedder fails to load its model → whatever error `Embedder::new()` returns; reconciliation fails (e.g. corrupted stored embedding blob) → `MemoliteError::Corruption`-shaped errors from `reconcile_vector_index` (§0.8) propagate up.
- **Side effects**: creates the SQLite file on disk if it doesn't exist; creates/verifies tables; may download the embedding model on first run (handled inside `Embedder::new()`, not new to Step 0).
- **Complexity**: dominated by (a) SQLite file I/O, (b) embedding model load time (can be seconds on first run), (c) O(n) reconciliation over every existing memory row (only relevant when reopening a pre-existing database with data in it — an empty new database reconciles trivially).
- **Body walkthrough**: calls `Self::open_with_store_internal(path, None, BackfillPolicy::ReplaceAll).await` and returns whatever it returns. `None` means "no caller-supplied backend, construct the default in-memory one." `BackfillPolicy::ReplaceAll` is always safe here per §7.7's reasoning.

#### `MemoryEngine::open_with_store_internal(path, store_override, backfill) -> Result<Self>` (private — note the absence of `pub`)
- **Input**: `path` — same as above. `store_override: Option<Arc<dyn VectorStore>>` — `None` for the default path, `Some(...)` when a caller (starting M11) wants to supply their own backend. `backfill: BackfillPolicy` — how to reconcile the (possibly caller-supplied, possibly shared) backend against SQLite.
- **Output**: `Result<Self>`.
- **Why private (`async fn`, no `pub`)**: this function is an implementation detail shared by two *public* entry points — `open()` (Step 0, calls it with `None`/`ReplaceAll`) and the future `open_with_store()` (M11, calls it with `Some(...)`/caller-chosen policy). Keeping it private means external users of the crate can only reach it through one of those two well-defined, documented public functions, never with arbitrary parameter combinations.
- **Line-by-line**:
  1. `let mut raw_conn = rusqlite::Connection::open(path)?;` — opens (or creates) the SQLite file at `path`. `mut` because migrations need `&mut Connection` to run transactions. The `?` propagates any `rusqlite::Error` as `MemoliteError::Database` (via the existing `#[from]` conversion in `error.rs`, unchanged by Step 0).
  2. `crate::migrations::run_migrations(&mut raw_conn)?;` — calls into `migrations.rs` (§0.7) to create/verify the schema. This line is why `migrations.rs` must exist and be registered in `lib.rs` before `engine.rs` can compile.
  3. `let embedder = crate::embedder::Embedder::new()?;` — constructs the embedding model wrapper (pre-existing code, unchanged).
  4. `let dim = embedder.dimension();` — reads the embedding dimension (e.g. `384`) that this particular embedding model produces. This value is the "contract" every vector in this database must conform to.
  5. `let vector_store: Arc<dyn VectorStore> = match store_override { ... };` — this is a `match` on an `Option`, producing a value of the trait-object type `Arc<dyn crate::vector_store::VectorStore>` in both arms:
     - `Some(store) => { ... store }` — if the caller supplied a backend, first check `store.dimension() != dim`. If the caller's backend was built for a different embedding size than this embedder actually produces, every future `insert`/`search` call would either fail validation or (worse, if unchecked) silently misbehave — so this is checked eagerly, at open time, with a clear `InvalidArgument` error naming both dimensions. If it matches, the supplied `store` (already an `Arc<dyn VectorStore>`) is used as-is.
     - `None => std::sync::Arc::new(crate::vector_store::InMemoryVectorStore::new(dim))` — constructs a brand new default backend, sized correctly, and wraps it in a fresh `Arc`. This is the concrete type getting **coerced** to the trait object type `Arc<dyn VectorStore>` — Rust performs this coercion automatically here because the `match` expression's two arms must produce the same type, and the surrounding `let vector_store: Arc<dyn VectorStore> = ...` annotation tells the compiler which type to coerce toward.
  6. `let conn = std::sync::Mutex::new(raw_conn);` — wraps the now-fully-migrated connection in a `Mutex`, converting it into the type the struct field expects.
  7. `reconcile_vector_index(&conn, &vector_store, backfill).await?;` — see §0.8 in full; this is the call that makes the (possibly freshly-constructed, possibly caller-supplied) vector store's contents agree with whatever SQLite currently contains, according to the chosen `backfill` policy. Note: this call happens **before** `Self { ... }` is constructed — i.e., before the engine formally "exists" — because `reconcile_vector_index` is written as a free function taking `&Mutex<Connection>` and `&Arc<dyn VectorStore>` directly, not as a method on `&self` (there is no `self` yet at this point in the function).
  8. `Ok(Self { conn, embedder: Mutex::new(embedder), vector_store: RwLock::new(vector_store), maintenance_running: Arc::new(AtomicBool::new(false)) })` — assembles the final struct. Note `conn` is used directly (already a `Mutex<Connection>` from step 6, matching the field's exact type — no wrapping needed here); `embedder` gets wrapped in a fresh `Mutex` right here in the struct literal; `vector_store` gets wrapped in a fresh `RwLock`; `maintenance_running` starts as `false` (no maintenance controller running yet, since none has been started).

### 7.9 The two locking rules you must follow for the rest of the crate (stated now, enforced later)

V6 states these as absolute rules that every subsequent milestone (M3 onward) must follow. They don't have code to add in Step 0 itself beyond what's already above, but you must understand them now because `open_with_store_internal` above is the first piece of code that follows them, and getting them wrong is the single most common category of bug in the rest of the plan.

**Rule for `conn`:**
```rust
let conn = self.conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
// ...conn.prepare / conn.execute / conn.query_row...
// guard MUST be dropped before any .await in the same function
```
A `std::sync::MutexGuard` is **not** `Send` across an `.await` point in the general case in a way that's safe — holding a synchronous lock across an `await` risks deadlocks (the task could be suspended mid-lock, and another task on the same thread trying to acquire the same lock would hang forever) and is flagged by clippy's `await_holding_lock` lint. Every function that touches `conn` must finish all its SQLite work, let the guard go out of scope (typically by wrapping the lock-acquire-and-use code in its own `{ ... }` block), and only *then* `.await` anything.

**Rule for `vector_store`:**
```rust
let store = {
    let guard = self.vector_store.read().map_err(|_| MemoliteError::Internal("vector-store lock poisoned".into()))?;
    std::sync::Arc::clone(&*guard)
};
store.search(&query_vec, k).await?;
```
Same idea: acquire the `RwLock` read guard, clone the `Arc` inside it (cheap — just increments a reference count, does not clone the underlying `InMemoryVectorStore`'s data), let the guard drop at the end of the `{ ... }` block, and only then call `.await` on the cloned `Arc`'s trait methods.

`reconcile_vector_index` (§0.8) is the first function in the codebase that must follow the `conn` rule, and it does — verify this yourself when you write it: its SQL read phase is wrapped in its own block and fully collects results into an owned `Vec` before the block ends, guaranteeing the `MutexGuard` is dropped before the function's first `.await`.

### 7.10 Checkpoint 0.5 (partial — full engine.rs checkpoint comes after §0.8)

At this point, `engine.rs` references `crate::migrations::run_migrations` (not yet written — §0.7) and calls `reconcile_vector_index` (not yet written — §0.8), so **`cargo build` will still fail** if you try it right now. This is expected. Do not attempt a full build yet. Instead, do a narrower syntax sanity check:

```bash
cargo check 2>&1 | grep -E "error\[E0(432|433|425)\]" | head -20
```

You should see errors specifically naming `migrations` and `reconcile_vector_index` as unresolved — and **no other kind of error** (no mismatched-type errors inside the code you just wrote, no missing-field errors on the `Self { ... }` struct literal). If you see mismatched-type or missing-field errors on your new code, fix those before proceeding — they indicate a typo relative to §7.6's exact text.

Continue to §0.6 and §0.7 next; the full checkpoint for `engine.rs` is in §0.8's checkpoint, once `reconcile_vector_index` also exists.

### 7.11 Common mistakes

- **Forgetting `mut` on `raw_conn`**: `run_migrations(&mut raw_conn)` requires a mutable reference; if `raw_conn` wasn't declared `let mut raw_conn = ...`, you get `error[E0596]: cannot borrow 'raw_conn' as mutable, as it is not declared as mutable`.
- **Wrapping `conn` in `Mutex::new` twice**: some engineers accidentally write `conn: std::sync::Mutex::new(std::sync::Mutex::new(raw_conn))` by copy-paste error — watch for this; `conn` in the struct literal should be the *already-`Mutex`-wrapped* local variable from step 6, used as-is.
- **Returning `Self { ... }` without `Ok(...)`** — the function returns `Result<Self>`, not `Self`; every success path must be wrapped in `Ok(...)`.
- **Making `open_with_store_internal` `pub`** — it must stay private (no `pub` keyword) in Step 0. It becomes reachable from other modules only in M11, and even then, V6's own technical review flagged that making it merely private (not `pub(crate)`) blocks M11's cross-module call — but that fix belongs to M11, not Step 0. Do not add `pub(crate)` preemptively; leave it exactly as shown (private, no visibility modifier) for Step 0 and let M11 change it when M11 actually needs to.

---

## 8. Section 0.6 — `src/recall.rs` (NEW FILE, Step-0-relevant portion only)

### 8.1 Why this file (partially) exists in Step 0

V6 introduces two small, self-contained pieces of code in Step 0 that logically belong in a file called `recall.rs`, even though the *rest* of that file (the `RecallQuery`/`RecallItem`/`RecallResult` types and the `recall_query()` method) doesn't arrive until M4. V6 explicitly calls this out: "0.6 — Column-order constant and candidate-pool sizing." The two pieces are:

1. `MEMORY_COLUMNS`, a `const` string listing the exact SQL column order used whenever a row is read back out of the `memories` table. This lives in `engine.rs` in V6's own text (not `recall.rs`) — see the clarification in §8.4 below.
2. `MAX_CANDIDATES` and `candidate_pool_size(limit: usize) -> usize`, which **do** belong in `recall.rs` per V6's explicit file-path comment (`// src/recall.rs`).

### 8.2 Is this a new file or existing file?

**New file** (partial — it will be substantially extended in M4; Step 0 only creates the file and puts these two items in it).

### 8.3 Exact path

```text
src/recall.rs
```

### 8.4 Complete file contents (Step 0 version)

```rust
/// The maximum number of candidate vector-search hits `recall_query()`
/// (introduced in Milestone M4) will ever request from the active
/// `VectorStore` in a single call, regardless of how large a `limit` the
/// caller asks for. Declared now, in Step 0, even though nothing calls
/// `candidate_pool_size` yet, because it is a pure, dependency-free
/// constant/function pair that the rest of the plan treats as foundational.
pub const MAX_CANDIDATES: usize = 500;

/// Given the number of *final* results a caller wants (`limit`), returns
/// how many raw candidates should be requested from the vector backend
/// before filtering/ranking narrows them down. Over-fetches by 5x (a
/// caller asking for 5 results causes up to 50 candidates to be pulled),
/// with a floor of 50 (so small limits still get a reasonable candidate
/// pool to filter from) and a ceiling of `MAX_CANDIDATES` (so a very large
/// `limit` can never force an unbounded, expensive vector-store scan).
pub fn candidate_pool_size(limit: usize) -> usize {
    limit.saturating_mul(5).max(50).min(MAX_CANDIDATES)
}
```

### 8.5 Explanation of the constant and function

- **`pub const MAX_CANDIDATES: usize = 500;`** — a compile-time constant (not a `static`, because it has no identity/address that needs to be unique — `const` values are inlined at every use site). `usize` is chosen because it's used for collection sizes/lengths throughout the crate, matching `Vec::len()`'s return type.
- **`candidate_pool_size(limit: usize) -> usize`**:
  - **Input**: `limit` — how many final, ranked results the caller ultimately wants back from a future recall operation.
  - **Output**: how many raw candidates to fetch from the vector backend *before* filtering (by importance, type, expiry, etc. — filtering logic arrives in M4) and ranking narrow that pool down to `limit`.
  - **`limit.saturating_mul(5)`** — multiplies by 5, but uses *saturating* multiplication instead of the `*` operator: if `limit` were something absurd like `usize::MAX`, a plain `*` would panic (in debug builds) or silently wrap around (in release builds) on overflow; `saturating_mul` instead clamps to `usize::MAX` without panicking or wrapping. This is defensive coding for an input that, in practice, will always be small (single/double-digit `limit` values), but costs nothing to guard against.
  - **`.max(50)`** — ensures the pool is never smaller than 50, even for a `limit` of 1 or 2 — this guarantees the *filtering* step (added in M4) always has a reasonably sized pool to work with, rather than starving on a `limit=1` request that only fetched 5 raw candidates.
  - **`.min(MAX_CANDIDATES)`** — caps the pool at 500 regardless of how large `limit` is, protecting the (currently O(n) linear-scan) `InMemoryVectorStore::search` from being asked to return an unbounded number of results.
  - **Why it's declared now, not in M4**: it has zero dependencies on any type introduced later (no `RecallQuery`, no `Memory`, nothing) — it's pure arithmetic. V6 treats this as part of the foundational, always-available toolkit, alongside `MAX_CANDIDATES`.

### 8.6 A note on `MEMORY_COLUMNS` — do NOT add it to `recall.rs`

V6's §0.6 text shows two code blocks under one heading; the *first* one (`const MEMORY_COLUMNS: &str = "id, content, ..."`) is, per V6's own build-order table and every later milestone's usage (e.g., M7's `query_by_time_range` writes `format!("SELECT {MEMORY_COLUMNS} FROM memories WHERE ...")` from inside `engine.rs`), a constant that lives in **`engine.rs`**, not `recall.rs` — it is used exclusively by SQL-row-reading code, which lives in `engine.rs` throughout the whole plan. Because **no Step 0 function actually reads memory rows back out of SQLite yet** (that starts in M3 with `store()`/`get()`/`recall()`), `MEMORY_COLUMNS` has **no caller anywhere in Step 0's own code**. Introducing an unused `const` is harmless (Rust does not warn about unused `pub const` items in a library the way it warns about unused `let` bindings), but for a handbook that must produce a compiling, checkpoint-verified Step 0, the simplest correct choice is: **do not add `MEMORY_COLUMNS` in Step 0 at all.** Add it in M3, at the exact moment the first function that needs it (`store_id`'s companion read path, or `get()`) is written. This defers zero architecture and avoids introducing dead code with no test coverage this early. If you want to add it anyway for completeness, it is safe to do so — but it is **not required** for Step 0's checkpoint, and this handbook's checkpoint commands below do not assume it exists.

### 8.7 Checkpoint 0.6

`recall.rs` has zero dependencies on anything else in the crate (no `use crate::...` lines at all), so it can be verified standalone:

```bash
rustc --edition 2021 --crate-type lib src/recall.rs 2>&1
```

**Expected result:** no output (clean compile) other than possibly `warning: unused` notices, which do not occur here since both items are `pub`. If you see any error, it is a direct transcription mistake — re-copy §8.4 exactly.

You cannot yet run `cargo test` against this file specifically in the context of the full crate, because `recall.rs` is not yet registered in `lib.rs` — that happens in §0.9. Proceed to §0.7.

---

## 9. Section 0.7 — `src/migrations.rs` (NEW FILE)

### 9.1 Why this file exists

Every time `MemoryEngine::open()` runs, the SQLite schema must be guaranteed to exist and be in the shape the rest of the code expects (`memories` table, `embeddings` table, indexes). `migrations.rs` centralizes this logic in one function, `run_migrations`, called once at the top of `open_with_store_internal` (§7.6, step 2). This is what makes opening a brand-new empty database file and re-opening an existing one with prior data behave identically from the caller's point of view — both end up with a correct, fully-indexed schema.

**Critical correction versus V6's literal text (see §0.0.1):** V6's own Step 0.7 text includes a call to `crate::confidence::repair_confidence_column(conn)?;` at the end of `run_migrations`. **You must NOT include that call in Step 0.** `src/confidence.rs` does not exist until Milestone M6. Including that line now produces:
```text
error[E0433]: failed to resolve: use of undeclared crate or module `confidence`
```
This handbook's version of `run_migrations` performs **only** V6's "migration 1" (baseline schema: `memories`, `embeddings`, indexes, and recording `schema_migrations` version 1). The confidence-column repair (V6's "migration 2") is added later, in Milestone M6, in the same milestone that creates `src/confidence.rs` — at that point, M6's instructions will show you the exact one-line addition to make to this same function. Step 0's job is only to make sure the *rest* of the schema (everything except `confidence`) is correct and versioned.

### 9.2 Is this a new file or existing file?

**New file.**

### 9.3 Exact path

```text
src/migrations.rs
```

### 9.4 Complete file contents

```rust
pub fn run_migrations(conn: &mut rusqlite::Connection) -> crate::error::Result<()> {
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
        [],
    )?;
    fn has_table(tx: &rusqlite::Transaction, name: &str) -> crate::error::Result<bool> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![name], |r| r.get(0),
        )?;
        Ok(count > 0)
    }
    fn has_migration(tx: &rusqlite::Transaction, version: i64) -> crate::error::Result<bool> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
            rusqlite::params![version], |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    // --- migration 1: baseline memories/embeddings ---
    let tx = conn.transaction()?;
    if !has_table(&tx, "memories")? {
        tx.execute_batch(
            "CREATE TABLE memories (
                id              TEXT PRIMARY KEY,
                content         TEXT NOT NULL,
                type            TEXT NOT NULL CHECK(type IN ('semantic','episodic','procedural','working')),
                importance      REAL NOT NULL DEFAULT 0.5 CHECK(importance BETWEEN 0.0 AND 1.0),
                access_count    INTEGER NOT NULL DEFAULT 0,
                created_at      INTEGER NOT NULL,
                last_accessed   INTEGER NOT NULL,
                expires_at      INTEGER,
                superseded_by   TEXT REFERENCES memories(id),
                metadata        TEXT NOT NULL DEFAULT '{}'
            );"
        )?;
    }
    if !has_table(&tx, "embeddings")? {
        tx.execute_batch(
            "CREATE TABLE embeddings (
                memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
                vector    BLOB NOT NULL,
                dimension INTEGER NOT NULL
            );"
        )?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
         CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
         CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type);
         CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at);
         CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);"
    )?;
    if !has_migration(&tx, 1)? {
        tx.execute("INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
            rusqlite::params![chrono::Utc::now().timestamp()])?;
    }
    tx.commit()?;

    // NOTE (Step 0 scope): migration 2 (the `confidence` column repair) is
    // intentionally NOT called here. It is added in Milestone M6, in the
    // same step that creates `src/confidence.rs`. Adding that call before
    // that module exists breaks compilation. See handbook §0.0.1 / §9.1.

    Ok(())
}
```

### 9.5 Explanation of every part

#### Function signature: `pub fn run_migrations(conn: &mut rusqlite::Connection) -> crate::error::Result<()>`
- **Not `async`** — every operation inside is synchronous `rusqlite` I/O; there is no `.await` anywhere in this function, so it is a plain `fn`.
- **`conn: &mut rusqlite::Connection`** — takes a *mutable borrow* of the connection (not ownership) because `rusqlite::Connection::transaction()` requires `&mut self`. The caller (`open_with_store_internal`, §7.6) still owns `raw_conn` after this call returns.
- **Return type**: the crate's own `Result<()>` alias (`crate::error::Result`, from §0.2) — either `Ok(())` on success or an `Err(MemoliteError)` (via the existing `#[from] rusqlite::Error` conversion — every `?` on a `rusqlite::Result` in this function automatically converts into `MemoliteError::Database` because of that pre-existing `#[from]` attribute in `error.rs`, unchanged by Step 0).

#### `conn.execute("PRAGMA foreign_keys = ON", [])?;`
SQLite disables foreign-key constraint enforcement by default (for backward compatibility reasons baked into SQLite itself). This line turns it on for this connection. It matters because the `embeddings` table declares `memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE` — without foreign keys enabled, `ON DELETE CASCADE` would silently do nothing, and deleting a memory row would leave an orphaned embedding row behind. The empty `[]` is `rusqlite`'s way of saying "no bound parameters for this query."

#### `conn.execute("CREATE TABLE IF NOT EXISTS schema_migrations (...)", [])?;`
Creates the migration-tracking table itself, unconditionally (using `IF NOT EXISTS` so re-running this on an already-migrated database is a no-op, not an error). Two columns: `version INTEGER PRIMARY KEY` (which migration number this row represents) and `applied_at INTEGER NOT NULL` (a Unix timestamp of when it was applied, useful for auditing).

#### `fn has_table(tx: &rusqlite::Transaction, name: &str) -> crate::error::Result<bool>` (nested helper function)
Declared *inside* `run_migrations`'s body — this is valid Rust (functions can be nested inside other functions) and is a deliberate scoping choice: `has_table` is only ever meaningful in the context of running migrations, so it's kept private to this function rather than exposed at module level. It queries SQLite's built-in `sqlite_master` catalog table (which lists every table, index, view, and trigger in the database) for a row where `type='table' AND name=<the table name you're checking>`, and returns whether the count is greater than zero.

#### `fn has_migration(tx: &rusqlite::Transaction, version: i64) -> crate::error::Result<bool>` (nested helper function)
Same idea, but checks the `schema_migrations` tracking table instead of SQLite's catalog, to see whether a specific migration version number has already been recorded as applied.

#### `let tx = conn.transaction()?;`
Opens a SQLite transaction. Every statement executed against `tx` from this point until `tx.commit()` (or an implicit rollback on drop, if an error occurs and the function returns early via `?`) is part of one atomic unit — either the whole schema gets created/updated, or none of it does, even if the process crashes partway through.

#### The `memories` table creation block
```rust
if !has_table(&tx, "memories")? {
    tx.execute_batch("CREATE TABLE memories ( ... );")?;
}
```
Only creates the table if it doesn't already exist — this makes `run_migrations` **idempotent**: calling it on a database that already has the table (e.g., every time an existing user reopens their database) is a safe no-op for this block. Column-by-column:

| Column | Type | Constraint | Meaning |
|---|---|---|---|
| `id` | `TEXT` | `PRIMARY KEY` | The memory's UUID, stored as its string representation. |
| `content` | `TEXT` | `NOT NULL` | The raw text of the memory. |
| `type` | `TEXT` | `NOT NULL CHECK(type IN (...))` | One of the four `MemoryType` variants, stored as lowercase strings. The `CHECK` constraint means SQLite itself will reject an `INSERT`/`UPDATE` that tries to store any other string — a database-level safety net in addition to Rust's own type system. |
| `importance` | `REAL` | `NOT NULL DEFAULT 0.5 CHECK(importance BETWEEN 0.0 AND 1.0)` | The caller-supplied importance score. Defaults to `0.5` if not specified (though every Step-0-and-later Rust code path always supplies it explicitly); the `CHECK` enforces the `[0.0, 1.0]` range at the database level too. |
| `access_count` | `INTEGER` | `NOT NULL DEFAULT 0` | How many times this memory has been returned by a recall operation (incremented starting M3/M4). |
| `created_at` | `INTEGER` | `NOT NULL` | Unix timestamp (seconds) of when the memory was stored. |
| `last_accessed` | `INTEGER` | `NOT NULL` | Unix timestamp of the most recent recall (initialized to `created_at` at store time). |
| `expires_at` | `INTEGER` | *(nullable — no `NOT NULL`)* | Unix timestamp after which the memory is eligible for purging; `NULL` means "never expires." |
| `superseded_by` | `TEXT` | `REFERENCES memories(id)` | If this memory has been replaced by a newer one (starting M5's `update()`), this holds the newer memory's id; otherwise `NULL`. The `REFERENCES` clause is a foreign key pointing back at this same table (a self-referencing foreign key). |
| `metadata` | `TEXT` | `NOT NULL DEFAULT '{}'` | A JSON-encoded string (via `serde_json::to_string`) holding arbitrary caller-supplied key/value metadata. Stored as `TEXT` because SQLite has no native JSON column type in the version/feature-set this project targets — the JSON encoding/decoding happens entirely in Rust. |

#### The `embeddings` table creation block
```rust
if !has_table(&tx, "embeddings")? {
    tx.execute_batch("CREATE TABLE embeddings ( ... );")?;
}
```

| Column | Type | Constraint | Meaning |
|---|---|---|---|
| `memory_id` | `TEXT` | `PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE` | Which memory this embedding belongs to. Because it's the primary key, each memory can have **at most one** embedding row — this is the exact invariant later milestones (M9's compression, the corruption-detection logic in §0.8) depend on: "every memory has exactly one embedding." `ON DELETE CASCADE` means deleting a row from `memories` automatically deletes its matching row here too, *provided* `PRAGMA foreign_keys = ON` is active (which it is, per the first line of this function). |
| `vector` | `BLOB` | `NOT NULL` | The `bincode`-serialized `Vec<f32>` embedding, stored as raw bytes. |
| `dimension` | `INTEGER` | `NOT NULL` | The length of the vector, stored redundantly alongside the blob so it can be checked/read without first deserializing the whole blob. |

Why are `memories` and `embeddings` two separate tables instead of one table with a `BLOB` column? Because most queries (listing memories, filtering by type/importance, checking expiry) never need the embedding data, and keeping it in a separate table means those queries don't have to read/skip large binary blobs they don't need — a normal database-design practice for "hot, frequently-read metadata" versus "large, rarely-individually-read binary payloads."

#### The index-creation block
```rust
tx.execute_batch(
    "CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
     CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
     CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type);
     CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at);
     CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);"
)?;
```
This block runs **every time**, not just on first creation (there's no `if !has_table` guard around it), because `CREATE INDEX IF NOT EXISTS` is itself already idempotent — running it again on a database that already has the index is a safe no-op. This means even a database that had its `memories` table created by some earlier, index-less version of the code will get these indexes added retroactively the next time it's opened. Each index speeds up the specific `WHERE`/`ORDER BY` clauses that later milestones use: `created_at`/`last_accessed` for temporal queries (M7), `type` for type-filtered recall (M4), `expires_at` for purge queries (M3), `superseded_by` for finding non-superseded ("active") memories (M4/M7).

#### Recording migration 1
```rust
if !has_migration(&tx, 1)? {
    tx.execute("INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
        rusqlite::params![chrono::Utc::now().timestamp()])?;
}
```
Only inserts the `version = 1` row into `schema_migrations` if it isn't already there — this is what makes the whole function safe to call on every single `open()`, not just the first one ever. `rusqlite::params![...]` is `rusqlite`'s macro for building a parameter list to bind against the `?1` placeholder in the SQL string — using bound parameters (instead of string-formatting the value directly into the SQL) is the standard defense against SQL injection and also lets SQLite reuse its query plan.

#### `tx.commit()?;`
Finalizes the transaction, making every change above permanent. If this line is never reached (because an earlier `?` returned an error), the transaction is automatically rolled back when `tx` is dropped at the end of the function (this is `rusqlite::Transaction`'s documented `Drop` behavior) — so a failure partway through never leaves the schema half-created.

### 9.6 Explanation of every test — **none exist yet for this file in Step 0**

V6's own Step 0.7 text does not include unit tests directly inside `migrations.rs`; the restart/migration test coverage for this logic comes from `engine.rs`'s own test suite starting in M3 (a "restart test": store data, drop the engine, reopen the same path, confirm everything is still there — this exercises `run_migrations` indirectly). Because Step 0 does not yet have a working `store()`/`open()` round trip usable from a test (the rest of `MemoryEngine`'s methods are M3), **there is no meaningful standalone test to write for `migrations.rs` in Step 0 alone.** Do not invent one; wait for M3.

### 9.7 Checkpoint 0.7

`migrations.rs` depends only on `rusqlite` and `chrono` (both already crate dependencies) and `crate::error::Result` (§0.2, already done). It has no dependency on anything created in §0.3–§0.6. Verify it compiles standalone by temporarily adding `pub mod migrations;` to `lib.rs` (again, this is folded into the final §0.9 edit — this is just an isolated sanity check):

```bash
cargo build 2>&1 | tail -40
```

**Expected result:** no errors originating from `src/migrations.rs`. If you see `error[E0433]: failed to resolve: use of undeclared crate or module 'confidence'`, you accidentally pasted V6's literal (uncorrected) text including the `repair_confidence_column` call — go back to §9.4 and remove that line; it must not be present in Step 0.

### 9.8 Common mistakes

- **Including the `confidence` call** (see above) — the single most likely mistake if you're cross-referencing V6's raw text instead of this handbook's corrected version.
- **Forgetting `PRAGMA foreign_keys = ON`** — the schema will still create successfully, but `ON DELETE CASCADE` will silently not work, which won't surface as a compile error or even an immediate test failure — it will surface much later (M3's `forget()` tests) as orphaned rows left behind in `embeddings` after deleting from `memories`. Get this right now.
- **Using `conn.execute_batch` instead of `tx.execute_batch`** for the table/index creation — this would run those statements *outside* the transaction, defeating the atomicity guarantee. Always call these methods on `tx`, not `conn`, between `conn.transaction()?` and `tx.commit()?`.

---

## 10. Section 0.8 — `reconcile_vector_index` (addition to `src/engine.rs`)

### 10.1 Why this function exists

This is the concrete implementation of "make the vector index agree with SQLite," parameterized by the `BackfillPolicy` from §0.5. It's what `open_with_store_internal` calls (§7.6, step 7) right before assembling the final `MemoryEngine`. Its most important design decision — and the one place where this handbook deviates from a naive re-reading of "just join the two tables" — is how it treats a memory row that has **no** matching embedding row: it treats this as **database corruption**, not as something to silently skip. This is a deliberate architectural choice (present in V6, and confirmed correct by V6's own technical review) that closes a whole class of "silently missing data" bugs that affected earlier plan drafts.

### 10.2 Existing file / new addition

You are adding this to the **already-modified** `src/engine.rs` (the same file from §0.5). It is a free function (not a method on `MemoryEngine`), placed at module scope, typically directly below the `impl MemoryEngine { ... }` block from §7.6, or above it — either position compiles identically since Rust does not require items to be declared before their first use within the same module (this is different from some other languages). This handbook places it **after** the `impl MemoryEngine` block for readability.

### 10.3 Exact insertion point

At the bottom of `src/engine.rs` (after the closing `}` of the `impl MemoryEngine { ... pub async fn open ... async fn open_with_store_internal ... }` block from §7.6), add:

```rust
/// Reads every memory row and, via a LEFT JOIN, every embedding row that
/// should exist for it. A NULL on the embedding side means a memory row
/// exists with no embedding — since every writer of memory+embedding rows
/// (starting Milestone M3/M5) always writes both in the same SQLite
/// transaction, this is only reachable via external corruption of the
/// SQLite file (e.g. hand-editing it, a crash mid-write outside this
/// crate's own transactional writers, or a bug in a future version), and
/// is reported as such rather than silently dropped from the index.
///
/// Read phase is fully synchronous and collects into a `Vec` before any
/// `.await`, so no SQLite `MutexGuard` is ever held across an await point
/// (see the "Rule for `conn`" in this handbook's engine.rs section).
async fn reconcile_vector_index(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    store: &std::sync::Arc<dyn crate::vector_store::VectorStore>,
    policy: BackfillPolicy,
) -> Result<()> {
    use crate::vector_store::VectorEntry;

    let entries: Vec<VectorEntry> = {
        let conn = conn.lock().map_err(|_| MemoliteError::Internal("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector, e.dimension, m.metadata
             FROM memories m LEFT JOIN embeddings e ON e.memory_id = m.id"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id_str, bytes, stored_dim, metadata_json) = row?;
            let id = Uuid::parse_str(&id_str)?;
            let (Some(bytes), Some(stored_dim)) = (bytes, stored_dim) else {
                return Err(MemoliteError::Corruption(format!(
                    "memory {id} has no matching embedding row"
                )));
            };
            let vector: Vec<f32> = bincode::deserialize(&bytes).map_err(|e| MemoliteError::EmbeddingDecode(e.to_string()))?;
            if vector.len() != stored_dim as usize {
                return Err(MemoliteError::Corruption(format!(
                    "stored vector for {id} has dimension {} but its row says {}", vector.len(), stored_dim
                )));
            }
            let metadata: HashMap<String, Value> = serde_json::from_str(&metadata_json)?;
            out.push(VectorEntry { id, vector, metadata });
        }
        out
    }; // MutexGuard dropped here — before any await below

    match policy {
        BackfillPolicy::ExistingOnly => Ok(()),
        BackfillPolicy::UpsertLocal => {
            for e in entries {
                store.insert(e.id, &e.vector, e.metadata).await?;
            }
            Ok(())
        }
        BackfillPolicy::ReplaceAll => store.replace_all(entries).await,
    }
}
```

### 10.4 A new error variant this function requires: `MemoliteError::Corruption`

Look closely at the code above: it constructs `MemoliteError::Corruption(format!(...))` twice. **This variant was not added in §0.2's edit to `error.rs`.** You must go back to `src/error.rs` now and add it. This is intentional sequencing in this handbook (not an oversight) — §0.2 covered the variants V6 groups under "0.2 — Error variants," and `Corruption` is introduced by V6 later in the same overall Step 0, specifically alongside the reconciliation logic, because it's conceptually part of "what this function needs," not part of the generic error set. Reopen `src/error.rs` and, inside the same `pub enum MemoliteError { ... }` block you edited in §4.4, add one more arm (after `CompensationFailed`, before the enum's closing `}`):

```rust
    /// The database has a memory row with no matching embedding row. Every
    /// writer of memory+embedding pairs (starting Milestone M3/M5) always
    /// writes both in one SQLite transaction, so this can only mean the
    /// on-disk file was corrupted or hand-edited outside the library.
    #[error("data invariant violated: {0}")]
    Corruption(String),
```

Re-run `cargo build` after this addition before continuing — `reconcile_vector_index` will not compile without it (`error[E0599]: no variant or associated item named 'Corruption' found for enum 'MemoliteError'`).

### 10.5 Required imports at the top of `engine.rs`

`reconcile_vector_index` uses `Uuid`, `HashMap`, `Value`, and `MemoliteError`/`Result` without fully-qualifying them (e.g. it writes `Uuid::parse_str`, not `uuid::Uuid::parse_str`). Confirm the top of `src/engine.rs` has these `use` statements (add any that are missing — some may already be present from the pre-existing file):

```rust
use crate::error::{MemoliteError, Result};
use std::collections::HashMap;
use serde_json::Value;
use uuid::Uuid;
```

If your file already has these (likely, since pre-existing CRUD code needs `Uuid` and `HashMap` too), you don't need to add them again — just confirm they're present. If `engine.rs` currently only imports `Uuid` without `HashMap`/`Value`, add the missing ones.

### 10.6 Explanation of the function, in full

#### Signature
```rust
async fn reconcile_vector_index(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    store: &std::sync::Arc<dyn crate::vector_store::VectorStore>,
    policy: BackfillPolicy,
) -> Result<()>
```
- **Not a method** — takes `conn` and `store` as explicit parameters (both **borrowed references**, `&Mutex<...>` and `&Arc<...>`) instead of `&self`, because it's called from `open_with_store_internal` (§7.6) **before** a `MemoryEngine` value exists to have a `&self` of. This is why it's a free function at module scope, not `impl MemoryEngine { async fn reconcile_vector_index(&self, ...) }`.
- **`conn: &std::sync::Mutex<rusqlite::Connection>`** — a reference to the (already-migrated, already-`Mutex`-wrapped) connection.
- **`store: &std::sync::Arc<dyn crate::vector_store::VectorStore>`** — a reference to the `Arc` holding the (possibly-freshly-constructed, possibly-caller-supplied) backend.
- **`policy: BackfillPolicy`** — which of the three reconciliation strategies to apply (§0.5/§7.7).
- **Return**: `Result<()>` — success, or the first error encountered (SQLite failure, corrupt UUID, corrupt vector blob, dimension mismatch discovered during decode, or any error from the chosen `store` operation).

#### The read phase
```rust
let entries: Vec<VectorEntry> = {
    let conn = conn.lock().map_err(...)?;
    let mut stmt = conn.prepare("SELECT m.id, e.vector, e.dimension, m.metadata FROM memories m LEFT JOIN embeddings e ON e.memory_id = m.id")?;
    let rows = stmt.query_map([], |row| { ... })?;
    let mut out = Vec::new();
    for row in rows { ... out.push(VectorEntry { ... }); }
    out
};
```
- **`{ ... }` block wrapping everything, assigned to `let entries: Vec<VectorEntry> = ...`** — this whole block is a Rust **block expression**: its final expression (`out`, the last line with no trailing semicolon) becomes the value the block evaluates to. This is the pattern that guarantees the `conn` `MutexGuard` (the local variable shadowing the outer `conn` parameter, created by `conn.lock()?`) is **dropped at the closing `}`** of this block — well before the `.await` calls that happen afterward, satisfying the crate-wide "Rule for `conn`" from §7.9.
- **The SQL: `SELECT m.id, e.vector, e.dimension, m.metadata FROM memories m LEFT JOIN embeddings e ON e.memory_id = m.id`** — a `LEFT JOIN`, not an `INNER JOIN`. This is the single most important detail in this function, and it is a deliberate correction of a documented historical bug: an `INNER JOIN` would silently *exclude* any `memories` row that has no matching `embeddings` row from the result set entirely — meaning such a row would never even be *looked at*, let alone flagged as a problem. A `LEFT JOIN` instead includes **every** row from `memories`, and for any that have no matching `embeddings` row, the `e.*` columns come back as SQL `NULL`. This is what lets the code below **detect** the missing-embedding case instead of silently never seeing it.
- **`stmt.query_map([], |row| { ... })?`** — `[]` again means no bound parameters. The closure maps each raw SQLite row into a Rust tuple. Note the column types requested: `row.get::<_, String>(0)` for `m.id` (always present, since it's from the non-nullable side of the join), but `row.get::<_, Option<Vec<u8>>>(1)` for `e.vector` and `row.get::<_, Option<i64>>(2)` for `e.dimension` — wrapped in `Option` specifically because a `LEFT JOIN` can produce `NULL` for these columns, and `rusqlite` requires you to declare that possibility in the type you ask for, or it will return a runtime error trying to convert SQL `NULL` into a non-`Option` Rust type.
- **The `for row in rows { ... }` loop body**:
  ```rust
  let (id_str, bytes, stored_dim, metadata_json) = row?;
  let id = Uuid::parse_str(&id_str)?;
  let (Some(bytes), Some(stored_dim)) = (bytes, stored_dim) else {
      return Err(MemoliteError::Corruption(format!("memory {id} has no matching embedding row")));
  };
  ```
  - `row?` — each item yielded by `query_map`'s iterator is itself a `rusqlite::Result<...>`; `?` unwraps it or propagates the error.
  - `Uuid::parse_str(&id_str)?` — converts the stored `TEXT` id back into a real `Uuid`; if the stored string is somehow not a valid UUID (further corruption), this also propagates an error (via the existing `#[from] uuid::Error` conversion already in `error.rs` from before Step 0).
  - **The `let-else` pattern**: `let (Some(bytes), Some(stored_dim)) = (bytes, stored_dim) else { return Err(...); };` — this is Rust's "let-else" syntax. It attempts to destructure the tuple `(bytes, stored_dim)` (both `Option`s) into the pattern `(Some(bytes), Some(stored_dim))`. If **both** are `Some`, the pattern matches, and the *inner* values are rebound to the names `bytes: Vec<u8>` and `stored_dim: i64` (shadowing the outer `Option`-typed names) for the rest of the loop iteration. If **either** is `None` (meaning the `LEFT JOIN` found no matching `embeddings` row, or found one with a `NULL` vector/dimension, which shouldn't happen given the schema's `NOT NULL` constraints but is defensively handled the same way), the `else` branch runs, which **returns early from the whole function** with `Err(MemoliteError::Corruption(...))` — aborting the read phase immediately, with a message naming exactly which memory id is missing its embedding.
  - `bincode::deserialize::<Vec<f32>>(&bytes)` — decodes the raw bytes back into the embedding vector, using the same binary format `bincode::serialize` will use when *writing* embeddings (starting M3). Any decode failure (e.g. genuinely corrupted bytes) becomes `MemoliteError::EmbeddingDecode`.
  - The dimension cross-check: `if vector.len() != stored_dim as usize { return Err(Corruption(...)); }` — an extra sanity check comparing the *actual* decoded vector length against the `dimension` column's stored value; if they disagree, something is wrong with the row (the schema is supposed to keep these in sync, but this defends against any writer bug that stored a mismatched pair).
  - `serde_json::from_str::<HashMap<String, Value>>(&metadata_json)?` — decodes the metadata column back into a real `HashMap`.
  - `out.push(VectorEntry { id, vector, metadata });` — collects the fully-validated entry into the result vector.

#### The reconciliation-strategy phase
```rust
match policy {
    BackfillPolicy::ExistingOnly => Ok(()),
    BackfillPolicy::UpsertLocal => {
        for e in entries {
            store.insert(e.id, &e.vector, e.metadata).await?;
        }
        Ok(())
    }
    BackfillPolicy::ReplaceAll => store.replace_all(entries).await,
}
```
This runs **after** the block above has ended (so the `conn` lock is already released) and is where the function's `.await` calls happen — legal per the locking rule, since no SQLite guard is held here.

- **`ExistingOnly => Ok(())`** — does nothing to the store at all; simply succeeds immediately. (Never actually reached by any Step 0 caller, since `open()` always passes `ReplaceAll` — but the branch must exist because `policy` is a parameter that can, in principle, be any of the three variants, and the `match` must be exhaustive over all of them or the code won't compile — Rust requires every `match` on an enum to cover every variant, unless a wildcard `_` arm is present, which this one deliberately does not use, so that adding a fourth `BackfillPolicy` variant in the future would force a compile error here as a reminder to handle it.)
- **`UpsertLocal => { for e in entries { store.insert(...).await?; } Ok(()) }`** — calls `insert` once per entry, using each entry's `id`, a *borrow* of its `vector` (`&e.vector`), and takes ownership of its `metadata` (moved into `insert`, since `insert`'s signature takes `metadata: HashMap<...>` by value, not by reference). If any single `insert` fails, the loop stops immediately and the error propagates (the `?` inside the loop body).
- **`ReplaceAll => store.replace_all(entries).await`** — hands the entire `Vec<VectorEntry>` (by value — ownership moves into the call) to `replace_all` in one call, and returns whatever that call returns directly (no extra `Ok(...)` wrapper needed here, since `replace_all` itself already returns `Result<()>`, matching this match arm's required type).

### 10.7 Full updated `src/engine.rs` — structural summary (not full text, since pre-existing methods vary per commit)

By the end of §0.5 and §0.8, `src/engine.rs` contains, in this order:
1. `use` statements (§7.6, §10.5).
2. `pub enum BackfillPolicy { ExistingOnly, UpsertLocal, ReplaceAll }` (§7.6).
3. `pub struct MemoryEngine { conn, embedder, vector_store, maintenance_running }` (§7.4).
4. `impl MemoryEngine { pub async fn open(...); async fn open_with_store_internal(...); }` (§7.6) — **and nothing else, in Step 0**.
5. `async fn reconcile_vector_index(...) -> Result<()> { ... }` (§10.3), a free function, not inside any `impl` block.

Any pre-existing methods on `MemoryEngine` from before Step 0 (`store`, `recall`, `forget`, `get`, etc.) either need to be temporarily disabled (per the note at the end of §7.6) or will fail to compile until M3 rewrites them — this is expected and out of scope for Step 0's own checkpoint.

### 10.8 Checkpoint 0.8 (this is the real, full Step-0 `engine.rs` checkpoint)

First, ensure `lib.rs` registers every module Step 0 has created so far. Jump ahead and complete §0.9 now (it's short), then come back here, or read §0.9 first — either order works since §0.9 doesn't introduce new logic, only registration. This handbook presents §0.9 next for exactly this reason.

---

## 11. Section 0.9 — `src/lib.rs`

### 11.1 Current purpose of this file

`src/lib.rs` is the crate root. It declares which files are part of the crate (via `mod`/`pub mod` statements) and which items are re-exported at the crate's top level for external users (via `pub use`). It currently (pre-Step-0) contains at least:

```rust
pub mod error;
pub mod embedder;
pub mod engine;
pub mod memory;

pub use engine::MemoryEngine;
pub use error::{MemoliteError, Result};
pub use memory::{Memory, MemoryType};
```

(Exact pre-existing content may vary slightly, but these four `mod` lines and these three `pub use` lines are, per V6's own explicit warning, load-bearing and must not be deleted.)

### 11.2 Why this modification exists, and why it differs from V6's literal text

V6's own text proposes writing the crate's **final** `lib.rs` (covering all twelve milestones) in Step 0, as a single "authoritative" block. As explained in §0.0.1, this breaks Step 0's own compile checkpoint, because it declares `pub mod ranking;`, `pub mod requests;`, `pub mod confidence;`, `pub mod streaming;`, `pub mod compression;`, `pub mod maintenance;`, `pub mod stats;` — none of which exist as files yet — and `pub use` statements referencing types (`RecallQuery`, `StoreRequest`, `ConfidenceLevel`, `MaintenanceHandle`, `MemoryStats`, `StreamIngestor`, ...) that aren't defined anywhere yet either. It also omits `pub mod memory;` and its re-export, which **must** stay, since `Memory`/`MemoryType` are used throughout `engine.rs` already.

This handbook's version of `lib.rs` registers **exactly** what exists at the end of Step 0 — no more, no less — so that `cargo build`/`cargo test` genuinely pass at this checkpoint, matching V6's own stated promise ("Checkpoint 0: `cargo build && cargo test` green"). Every later milestone (M3, M4, M5, ...) will add its own lines to this file; this handbook's job ends at Step 0's correct version.

### 11.3 Exact location inside the file

Open `src/lib.rs`. You are editing the whole top-level module/re-export block.

### 11.4 Full replacement content for Step 0

Replace the entire contents of `src/lib.rs` with:

```rust
pub mod embedder;
pub mod engine;
pub mod error;
pub mod memory;
pub mod migrations;
pub mod recall;
pub mod vector_store;

pub use engine::{BackfillPolicy, MemoryEngine};
pub use error::{MemoliteError, Result};
pub use memory::{Memory, MemoryType};
pub use vector_store::{InMemoryVectorStore, VectorEntry, VectorHit, VectorStore};
```

Notes on ordering: Rust does not require `mod`/`pub use` statements to appear in dependency order — they are alphabetized here purely for human readability, which is a common Rust community convention (and matches `rustfmt`'s default behavior if you run it).

### 11.5 Explanation of every line

| Line | Meaning |
|---|---|
| `pub mod embedder;` | Pre-existing, unchanged — exposes `src/embedder.rs`'s `Embedder` type (used internally by `engine.rs`; not itself re-exported at crate root in the pre-existing code, per the assumed baseline — leave as-is unless your actual pre-existing `lib.rs` already re-exports it, in which case keep that too). |
| `pub mod engine;` | Pre-existing, unchanged — exposes `src/engine.rs`. |
| `pub mod error;` | Pre-existing, unchanged — exposes `src/error.rs`. |
| `pub mod memory;` | Pre-existing — **must be kept**; exposes `src/memory.rs`'s `Memory`/`MemoryType`. This is the line V6's literal "final lib.rs" text mistakenly omits (§0.0.1, point 2) — do not omit it. |
| `pub mod migrations;` | **New in Step 0** — exposes `src/migrations.rs` (§0.7), making `crate::migrations::run_migrations` resolvable from `engine.rs`. |
| `pub mod recall;` | **New in Step 0** — exposes `src/recall.rs` (§0.6). Declared `pub` (not just `mod`) even though nothing outside the crate needs `candidate_pool_size` yet, for consistency with the other modules and because M4 will need this module's visibility to grow without another visibility change later. |
| `pub mod vector_store;` | **New in Step 0** — exposes `src/vector_store/mod.rs` (§0.3), and transitively (via that file's own `pub mod in_memory;`) `src/vector_store/in_memory.rs` (§0.4). |
| `pub use engine::{BackfillPolicy, MemoryEngine};` | Re-exports both the engine type and the new `BackfillPolicy` enum (§7.6) at the crate root, so external users write `memolite::BackfillPolicy` / `memolite::MemoryEngine` instead of the fully-qualified `memolite::engine::BackfillPolicy`. |
| `pub use error::{MemoliteError, Result};` | Pre-existing, unchanged. |
| `pub use memory::{Memory, MemoryType};` | Pre-existing, unchanged — **must be kept** (§0.0.1). |
| `pub use vector_store::{InMemoryVectorStore, VectorEntry, VectorHit, VectorStore};` | **New in Step 0** — re-exports the trait itself (`VectorStore`), its two supporting structs (`VectorHit`, `VectorEntry`), and the default implementation (`InMemoryVectorStore`) at the crate root. `validate_vector` is **deliberately not re-exported here** — it's a low-level helper meant for backend implementers (internal or third-party), not typical end users of the crate; it remains reachable as `memolite::vector_store::validate_vector` for anyone who specifically needs it (e.g. a future third-party backend author), without cluttering the crate's top-level namespace. |

### 11.6 What is intentionally NOT in this file yet

Do **not** add any of the following in Step 0 — they belong to later milestones, and adding them now will cause `cargo build` to fail because the underlying files/items don't exist:

```rust
// NOT YET — these belong to later milestones:
pub mod ranking;       // M4
pub mod requests;       // M5
pub mod confidence;     // M6
pub mod streaming;      // M8
pub mod compression;    // M9
pub mod maintenance;    // M10
pub mod stats;          // M9.5

pub use recall::{RecallQuery, RecallItem, RecallResult};   // M4
pub use requests::{StoreRequest, MemoryUpdate, ExpiryPolicy}; // M5
pub use confidence::ConfidenceLevel;                          // M6
pub use streaming::{StreamIngestor, IngestReport, IngestChunk}; // M8
pub use maintenance::{MaintenanceConfig, MaintenanceHandle};    // M10
pub use stats::MemoryStats;                                     // M9.5

#[cfg(feature = "generic-http")]
pub use vector_store::GenericHttpVectorStore;                   // M11
```

Keep this list handy — it is exactly the set of lines you will add, one milestone at a time, as you work through M3 onward in a future session. Each future milestone's own instructions (outside this handbook's Step-0-only scope) will tell you precisely when to add each of these.

### 11.7 Checkpoint 0.9 — THE FULL STEP 0 CHECKPOINT

This is the checkpoint that matters most: it validates everything from §0.1 through §0.9 together, exactly as V6 itself demands ("Checkpoint 0: `cargo build && cargo test` green").

**Step A — clean build:**
```bash
cargo clean
cargo build 2>&1 | tee /tmp/step0_build.log
```
**Expected result:** the log ends with a line like:
```text
   Compiling memolite v0.1.0 (/path/to/memolite)
warning: `memolite` (lib) generated N warnings
    Finished dev [unoptimized + debuginfo] target(s) in ...
```
Zero lines beginning with `error[E...]` or `error:` anywhere in the log. Warnings (e.g. about unused pre-existing methods you may have temporarily disabled per §7.6's note) are acceptable at this checkpoint; **errors are not.**

If you disabled pre-existing `store`/`recall`/`forget`/`get` methods per §7.6's note, you should see no errors at all. If you left them enabled and un-rewritten, you will see errors from those specific methods referencing `self.conn` as a bare `Connection` — that is expected and is **not** a Step 0 failure; it is the boundary between Step 0 and M3. To get a fully clean Step-0-only build with zero errors, comment out or `#[cfg(any())]`-gate those specific pre-existing method bodies for now.

**Step B — clippy (lint check):**
```bash
cargo clippy --all-targets -- -D warnings
```
**Expected result:** V6's own quality gate. At minimum, confirm there are no `clippy::await_holding_lock` warnings — this specifically validates that `reconcile_vector_index` (§10.3) truly drops its `MutexGuard` before any `.await`, per the locking rule in §7.9. If clippy flags this, re-check that the SQL read phase is fully enclosed in its own `{ ... }` block ending before the `match policy { ... }` block that contains the `.await` calls.

**Step C — run every Step-0 test:**
```bash
cargo test vector_store:: 2>&1 | tail -40
```
**Expected result:** the same 8 tests from §6.9, all passing.

**Step D — confirm the default feature set excludes the HTTP backend:**
```bash
cargo tree -e features | grep -i reqwest
```
**Expected result:** no output.

**Step E — confirm the crate documents cleanly:**
```bash
cargo doc --no-deps 2>&1 | tail -20
```
**Expected result:** no errors (warnings about missing doc comments on some pre-existing items are acceptable and not introduced by Step 0).

### 11.8 Common mistakes (whole-file level)

- **Leaving a stray duplicate `mod vector_store;` or `mod migrations;` line** from an earlier temporary sanity-check edit (§6.9, §9.7 both suggested temporarily adding single lines to `lib.rs` for isolated checks) — search the file for the word `vector_store` and `migrations` and make sure each appears exactly once as a `pub mod` line, matching §11.4 exactly.
- **Forgetting to re-export `BackfillPolicy`** — if you only wrote `pub use engine::MemoryEngine;` without `BackfillPolicy` alongside it, later milestones (M11) that write `memolite::BackfillPolicy` from outside the crate will fail to resolve; §11.4's exact `pub use engine::{BackfillPolicy, MemoryEngine};` avoids this now.
- **Accidentally deleting `pub mod memory;`** while replacing the file's contents wholesale — always diff your new `lib.rs` against §11.4 line-by-line after pasting.

---

## 12. End-to-End Diagrams for Step 0

### 12.1 `open()` runtime lifecycle (sequence of events when a caller calls `MemoryEngine::open(path)`)

```text
Caller
  │
  │  MemoryEngine::open("./agent.db").await
  ▼
open()  ──calls──▶  open_with_store_internal(path, None, ReplaceAll)
                          │
                          │ 1. rusqlite::Connection::open(path)?
                          │    -> raw_conn (file created/opened on disk)
                          ▼
                    migrations::run_migrations(&mut raw_conn)?
                          │
                          │  BEGIN TRANSACTION
                          │    CREATE TABLE schema_migrations IF NOT EXISTS
                          │    CREATE TABLE memories IF NOT EXISTS
                          │    CREATE TABLE embeddings IF NOT EXISTS
                          │    CREATE INDEX (x5) IF NOT EXISTS
                          │    INSERT INTO schema_migrations (1, now) IF NOT recorded
                          │  COMMIT
                          ▼
                    Embedder::new()?  -> embedder, dim = embedder.dimension()
                          │
                          ▼
                    store_override match:
                       None -> Arc::new(InMemoryVectorStore::new(dim))
                          │
                          ▼
                    conn = Mutex::new(raw_conn)
                          │
                          ▼
                    reconcile_vector_index(&conn, &vector_store, ReplaceAll).await
                          │
                          │  lock conn ─▶ SELECT ... LEFT JOIN ... ─▶ collect Vec<VectorEntry>
                          │  unlock conn (guard dropped)
                          │  vector_store.replace_all(entries).await
                          │     -> validates each entry, atomically swaps internal HashMap
                          ▼
                    Ok(MemoryEngine {
                        conn,
                        embedder: Mutex::new(embedder),
                        vector_store: RwLock::new(vector_store),
                        maintenance_running: Arc::new(AtomicBool::new(false)),
                    })
                          │
                          ▼
Caller receives Ok(MemoryEngine { ... })
```

### 12.2 Lock acquisition diagram (which locks exist, and the rule for using them)

```text
MemoryEngine
 ├── conn:  Mutex<rusqlite::Connection>
 │         RULE: lock -> use synchronously -> guard drops -> THEN .await (never both)
 │
 ├── embedder: Mutex<Embedder>
 │         RULE: same as conn (not exercised by any Step 0 function body directly,
 │               but the field's shape enforces this for every future caller)
 │
 ├── vector_store: RwLock<Arc<dyn VectorStore>>
 │         RULE: read-lock -> Arc::clone the pointer -> guard drops -> THEN .await
 │               on the cloned Arc's async trait methods
 │
 └── maintenance_running: Arc<AtomicBool>
           No lock needed — atomic operations are lock-free by construction.
           Unused by any Step 0 logic.
```

### 12.3 Module / compile-dependency diagram (final, annotated with file status)

```text
lib.rs (MODIFIED)
 ├─▶ mod error          (existing, MODIFIED: +4 new variants incl. Corruption)
 ├─▶ mod memory          (existing, UNCHANGED)
 ├─▶ mod embedder         (existing, UNCHANGED)
 ├─▶ mod migrations        (NEW FILE) ──depends on──▶ error
 ├─▶ mod recall             (NEW FILE) ──depends on──▶ (nothing)
 ├─▶ mod vector_store        (NEW DIR/FILE) ──depends on──▶ error
 │     └─▶ mod in_memory       (NEW FILE) ──depends on──▶ error, vector_store::mod
 └─▶ mod engine              (existing, MODIFIED) ──depends on──▶ error, memory,
                                                                    embedder, migrations,
                                                                    vector_store (both files)
```

### 12.4 SQLite / vector-store reconciliation transaction diagram

```text
                    ┌─────────────────────────────┐
                    │   memories (SQLite table)    │
                    │  id | ... | superseded_by ... │
                    └───────────────┬───────────────┘
                                    │ LEFT JOIN ON e.memory_id = m.id
                    ┌───────────────▼───────────────┐
                    │  embeddings (SQLite table)     │
                    │  memory_id | vector | dimension│
                    └───────────────┬───────────────┘
                                    │
                     row present, embedding present  ──▶ VectorEntry { id, vector, metadata }
                     row present, embedding NULL      ──▶ Err(Corruption(...))  [hard stop]
                                    │
                                    ▼
                         Vec<VectorEntry> (fully validated, in memory)
                                    │
                     ┌──────────────┼───────────────────┐
                     ▼              ▼                    ▼
              ExistingOnly    UpsertLocal            ReplaceAll
              (no-op)         (insert each,           (single atomic
                               keep extras)             HashMap swap)
```

---

## 13. Final Full-Repository Checkpoint

Run the complete verification sequence one more time, from a clean state, to confirm Step 0 is genuinely finished and stable:

```bash
cd memolite
cargo clean
cargo build 2>&1 | tail -30
cargo clippy --all-targets -- -D warnings 2>&1 | tail -30
cargo test 2>&1 | tail -60
cargo doc --no-deps 2>&1 | tail -20
cargo tree -e features | grep -i reqwest   # expect: no output
git diff --stat                             # sanity-check exactly which files changed
```

**Expected final state:**
- `cargo build` — zero errors.
- `cargo clippy -- -D warnings` — zero errors (no `await_holding_lock`, no unresolved paths).
- `cargo test` — all pre-existing tests plus the 8 new `vector_store::in_memory::tests::*` tests pass; total test count increased by exactly 8 relative to your pre-Step-0 baseline (assuming you did not add tests for `migrations.rs`/`recall.rs`, per §9.6/§8.7's guidance that none are meaningful yet).
- `cargo doc --no-deps` — completes without errors.
- `git diff --stat` shows changes to exactly: `Cargo.toml`, `src/error.rs`, `src/engine.rs`, `src/lib.rs`, plus three new files: `src/vector_store/mod.rs`, `src/vector_store/in_memory.rs`, `src/migrations.rs`, `src/recall.rs` (four new files total, matching §1.2's tree).

If every one of these checks passes, **Step 0 is complete**, and the repository is in the exact state V6 (as corrected by this handbook for sequencing) requires before Milestone M3 begins. Do not start M3 work until every command above has been run and verified on a genuinely clean (`cargo clean`) build — this guarantees no stale build artifacts are masking a real error.

---

## 14. Summary Table — Every File Touched in Step 0

| # | File | New/Modified | What was added | Section |
|---|---|---|---|---|
| 1 | `Cargo.toml` | Modified | `async-trait`, `tokio-util`, `tracing`, optional `reqwest`/`urlencoding`, `[features] generic-http`, dev-deps `wiremock`/`criterion` | §3 |
| 2 | `src/error.rs` | Modified | `InvalidArgument`, `VectorStore`, `Internal`, `CompensationFailed`, `Corruption` variants | §4, §10.4 |
| 3 | `src/vector_store/mod.rs` | Created | `VectorHit`, `VectorEntry`, `validate_vector`, `VectorStore` trait | §5 |
| 4 | `src/vector_store/in_memory.rs` | Created | `InMemoryVectorStore` + 8 unit tests | §6 |
| 5 | `src/recall.rs` | Created | `MAX_CANDIDATES`, `candidate_pool_size` | §8 |
| 6 | `src/migrations.rs` | Created | `run_migrations` (migration 1 only — confidence deferred to M6) | §9 |
| 7 | `src/engine.rs` | Modified | `BackfillPolicy` enum, `MemoryEngine` struct (final shape), `open()`, `open_with_store_internal()`, `reconcile_vector_index()` | §7, §10 |
| 8 | `src/lib.rs` | Modified | Step-0-correct module registration and re-exports (memory module preserved) | §11 |

**End of Step 0. Do not proceed to M3 until §13's checkpoint passes in full.**