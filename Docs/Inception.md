# ContextMemory

> **Persistent, typed, ranked memory for AI agents — written entirely in Rust.**
> No Python. No cloud dependency. No sidecar service required. `cargo add context-memory` and your agent remembers things across sessions.

---

## Table of Contents

1. [The Idea, In Plain English](#1-the-idea-in-plain-english)
2. [What This Project Actually Is](#2-what-this-project-actually-is)
3. [Why Build This (And Why In Rust)](#3-why-build-this-and-why-in-rust)
4. [Complete Technical Architecture](#4-complete-technical-architecture)
5. [Memory Model](#5-memory-model)
6. [The Ranking Algorithm — The Core Differentiator](#6-the-ranking-algorithm--the-core-differentiator)
7. [Storage Design](#7-storage-design)
8. [Public API](#8-public-api)
9. [The Vector Backend Trait — Pluggable Storage](#9-the-vector-backend-trait--pluggable-storage)
10. [Feature List — What We Are Building](#10-feature-list--what-we-are-building)
11. [What We Are Deliberately NOT Building (v1)](#11-what-we-are-deliberately-not-building-v1)
12. [Tech Stack](#12-tech-stack)
13. [System Diagram](#13-system-diagram)
14. [Build Roadmap](#14-build-roadmap)
15. [Why This Project Is a Strong Engineering Signal](#15-why-this-project-is-a-strong-engineering-signal)
16. [Comparison to Existing Projects](#16-comparison-to-existing-projects)
17. [Risks and Honest Limitations](#17-risks-and-honest-limitations)

---

## 1. The Idea, In Plain English

Imagine you have a friend with amnesia that resets every single time you talk to them. Every conversation, you'd have to re-explain who you are, what you like, what you're working on, and what you talked about last time. That's an LLM today. Every chat starts from a blank slate unless *you* manually paste the whole history back in — which gets slow, expensive, and eventually impossible as the history grows.

**ContextMemory is the fix.** It's a small, fast piece of software that sits next to an AI agent and does what a human brain does automatically:

- **Remembers facts** ("this user prefers Rust over Python") the way you'd remember a preference — these barely fade.
- **Remembers events** ("we discussed the auth bug on Tuesday") the way you'd remember a conversation — these fade fast, because yesterday's small talk matters less each day.
- **Remembers skills and patterns** ("this user always structures their Axum apps a certain way") the way you'd remember a habit — these fade very slowly.
- **Forgets what's no longer useful** — old scratch notes don't get kept forever.
- **Decides what to bring up right now** — not everything it knows, only what's relevant to the current question, weighted by how important it is, how recent it is, and how often it's proven useful before.
- **Tracks when facts change** — if the user said "I use Python" a year ago and "I mainly use Rust now" last week, the system doesn't just silently overwrite one fact with another; it keeps track of the fact that a change happened.

None of this requires calling out to an LLM every time something is stored (which is slow and costly) and none of it requires shipping the user's data to a third-party cloud service. It runs as a library, embedded directly inside the Rust application that's running the AI agent — the same way a database library like SQLite runs embedded inside your app, rather than as a separate server you have to manage.

---

## 2. What This Project Actually Is

**ContextMemory** is an **embeddable Rust library** (`cargo add context-memory`) that gives any Rust-based AI agent, chatbot, or LLM application persistent, typed, ranked memory — without a server process, without a cloud API key, and without external infrastructure for the default configuration.

It is the **memory layer** in a larger local-first AI memory stack. On its own, it stores memories in a local SQLite file and does similarity search in-process. When an application outgrows that (roughly beyond ~100k memories), it can be pointed at **OasysDB**, a companion self-hosted vector database (also Rust, also part of this stack) via a two-line configuration change — no data migration, no API rewrite.

The core insight that makes this more than "a vector database with extra steps" is: **similarity search alone produces bad memory retrieval.** A memory that's semantically close to the current query but six months stale and never referenced again is usually *less* useful than a moderately similar memory from this morning that's been recalled ten times. ContextMemory's job is to combine similarity, importance, recency, and reinforcement into a single ranking — the same instinct a person uses when deciding what's actually worth mentioning right now versus what's technically "related" but not helpful.

---

## 3. Why Build This (And Why In Rust)

**The gap this fills:** there is no embeddable Rust library that gives typed AI-agent memory with a clean API, backed by a swappable vector store, with zero mandatory external dependencies. Existing options are either:

- Python-first cloud services (Mem0, Zep/Graphiti) — great features, but cloud-dependent, billed per use, and the wrong language for a Rust-native agent stack.
- Rust *binaries* built for coding-agent tooling (ICM, engram-mcp, MemX) — good at what they do, but distributed as standalone processes/MCP servers, not as a library you embed directly in your own Rust application.
- Rust *ports* of Python tools (mem0-rust) — still require external infrastructure like Qdrant, Postgres, or Redis to actually run.

**Why Rust specifically matters here, technically — not just "because it's fast":**

- **Async concurrency** — an agent needs to read and write memory while also streaming a response and calling the LLM; Tokio gives predictable, safe concurrency for that without a GC pause or a runtime crash under load.
- **Predictable memory footprint** — an in-process vector index for up to ~100k embeddings needs to live comfortably in RAM without unpredictable allocation spikes; Rust's ownership model makes that reliable.
- **No sidecar process** — a Python memory service means running, monitoring, and deploying another process. An embedded Rust library means the memory layer ships *inside* the binary that ships the agent.
- **Correctness under concurrent access** — multiple agent tasks may read/write memory simultaneously; Rust's borrow checker catches data races that would otherwise be a runtime bug in a scripting language.

This is deliberately **not** a research project about inventing new memory architectures, and it is **not** a full rewrite of a Python framework line-by-line. It is a focused, production-shaped systems project: typed data model, a defensible ranking algorithm, a pluggable storage trait, and an honest, documented API surface.

---

## 4. Complete Technical Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      AI AGENT (your code)                   │
│   engine.store("fact", MemoryType::Semantic, 0.8)           │
│   engine.recall(RecallQuery::new("query").top_k(5))         │
└───────────────────────────────┬─────────────────────────────┘
                                │
                                ▼
┌───────────────────────────────────────────────────────────────┐
│                    MemoryEngine (public API)                  │
│  ┌──────────────┐  ┌─────────────────┐  ┌─────────────────┐   │
│  │  Ingestion   │  │    Retrieval    │  │ Context Assembly│   │
│  │  - validate  │  │  - embed query  │  │  - rank results │   │
│  │  - embed     │  │  - vector search│  │  - apply decay  │   │
│  │  - TTL calc  │  │  - SQLite join  │  │  - format output│   │
│  │  - persist   │  │  - rerank       │  │                 │   │
│  └──────────────┘  └────────┬────────┘  └─────────────────┘   │
│                             │                                 │
│  ┌──────────────────────────▼──────────────────────────────┐  │
│  │              VectorStore trait (interface)              │  │
│  └──────┬──────────────────────────────────────────────────┘  │
│         │                                                     │
│    ┌────▼────────────────────┐   ┌───────────────────────────┐│
│    │ InMemoryVectorStore     │   │ OasysDbVectorStore        ││
│    │ (default, flat cosine)  │   │ (feature = "oasysdb")     ││
│    └─────────────────────────┘   └────────────┬──────────────┘│
└──────────────────────────────────────────────  │──────────────┘
                                                 │ HTTP REST
                                                 ▼
                                   ┌───────────────────────────┐
                                   │   OasysDB (companion      │
                                   │   self-hosted vector DB)  │
                                   └───────────────────────────┘
                                                 │
                                                 ▼
                                          SQLite (.db file on disk)
```

**Layer breakdown:**

- **Ingestion** — takes raw text, generates an embedding locally (no external API call required by default), calculates the memory's expiry/decay parameters based on its type, and writes it to SQLite.
- **Retrieval** — embeds the incoming query, performs a vector similarity search against candidate memories, then joins the results with their metadata (importance, access count, timestamps) from SQLite.
- **Context Assembly** — applies the ranking formula to re-sort retrieved candidates, then formats the final result as a string ready to inject directly into an LLM prompt.
- **VectorStore trait** — the seam that keeps ContextMemory decoupled from any specific vector backend. The default `InMemoryVectorStore` requires nothing external. The optional `OasysDbVectorStore` (behind a Cargo feature flag) is a thin REST client that swaps in when an application needs to scale past what fits comfortably in memory.

---

## 5. Memory Model

Memories are not all the same, and treating them as such is the single biggest weakness of most existing "AI memory" tools. This project uses four typed categories, each with its own default lifetime and forgetting curve:

| Type | What it represents | Default TTL | Decay speed | Example |
|---|---|---|---|---|
| **Semantic** | Facts, preferences, timeless knowledge | 365 days | Very slow | "User prefers Rust over Python" |
| **Episodic** | Specific events, conversations, time-bound occurrences | 30 days | Fast | "User asked about auth bugs on Tuesday" |
| **Procedural** | Skills, habits, recurring patterns of behavior | 730 days | Extremely slow | "User always structures Axum apps as controller/service/model" |
| **Working** | Short-lived scratch context for the current session | 4 hours | Hard-expires | "User is currently debugging a Redis connection issue" |

Every stored memory carries:

```rust
pub struct Memory {
    pub id: String,                 // UUID v4
    pub content: String,            // the raw text of the memory
    pub memory_type: MemoryType,
    pub importance: f32,            // 0.0–1.0, set at store time
    pub access_count: u32,          // incremented every time it's recalled
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub superseded_by: Option<String>, // set when a newer memory contradicts this one
}
```

The `superseded_by` field is what enables **contradiction tracking**: when a new memory conflicts with an old one (e.g. "uses Python" → "uses Rust now"), the old memory isn't silently deleted — it's marked as superseded, preserving the *history* of how the user's facts evolved, rather than pretending they were always true.

---

## 6. The Ranking Algorithm — The Core Differentiator

Plain similarity search is the weakest part of every "AI memory" tool on the market today. This project's real engineering contribution is a small, explainable scoring function that combines four signals:

```
final_score = similarity × importance × recency_factor × reinforcement_factor
```

**Similarity** — standard cosine similarity between the query embedding and the memory's embedding, in `[0, 1]`.

**Importance** — set explicitly at store time (or inferred later), in `[0, 1]`. A high-importance memory can outrank a more "similar" but trivial one.

**Recency (exponential decay)**:
```
recency = e^(-decay_rate × days_since_last_access)
```
Decay rate is type-dependent — episodic memories fade in roughly 14 days (half-life), semantic memories take roughly 693 days, procedural memories roughly 1,386 days.

**Reinforcement (logarithmic, diminishing returns)**:
```
reinforcement = 1.0 + ln(1 + access_count) × 0.1
```
A memory that's been recalled 10 times gets a modest, capped boost — reflecting that repeated relevance is a signal of real usefulness, without letting one popular memory permanently dominate every query.

**Worked example** — why this matters in practice:

> Memory A: *"User prefers dark mode"* — semantic, importance 0.8, 30 days old, accessed 5 times, similarity to query 0.82 → **final score ≈ 0.753**
>
> Memory B: *"User asked about dark mode 60 days ago"* — episodic, importance 0.5, 60 days old, accessed once, similarity to query 0.91 → **final score ≈ 0.024**

Memory B is *more* semantically similar to the query, but Memory A wins the ranking — correctly, because a durable, recent preference is more useful to surface than a stale, one-off event. This is the difference between a system that does similarity search and a system that actually *remembers* the way a person would.

---

## 7. Storage Design

ContextMemory uses **SQLite** (via `rusqlite`) as its single-file, zero-configuration persistence layer — no external database server required.

```sql
CREATE TABLE memories (
    id              TEXT PRIMARY KEY,
    content         TEXT NOT NULL,
    type            TEXT NOT NULL CHECK(type IN ('semantic','episodic','procedural','working')),
    importance      REAL NOT NULL DEFAULT 0.5 CHECK(importance BETWEEN 0.0 AND 1.0),
    access_count    INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    last_accessed   INTEGER NOT NULL,
    expires_at      INTEGER,
    superseded_by   TEXT REFERENCES memories(id),
    metadata        TEXT DEFAULT '{}'
);

CREATE TABLE embeddings (
    memory_id   TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
    vector      BLOB NOT NULL,          -- bincode-encoded Vec<f32>
    dimension   INTEGER NOT NULL
);

CREATE INDEX idx_memories_type       ON memories(type);
CREATE INDEX idx_memories_expires    ON memories(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX idx_memories_importance ON memories(importance DESC);
```

Embeddings are kept in a separate table so they can be loaded in bulk into the in-memory vector index at startup without dragging along every text field and metadata blob.

---

## 8. Public API

The entire point of this library is that using it should feel almost trivially simple, while the internals handle real complexity.

```toml
[dependencies]
context-memory = "0.1"
```

```rust
use context_memory::{MemoryEngine, MemoryType, RecallQuery};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let engine = MemoryEngine::open("./agent-memory").await?;

    engine.store(
        "user prefers dark mode and concise responses",
        MemoryType::Semantic,
        0.8,
    ).await?;

    let context = engine
        .recall(RecallQuery::new("UI preferences").top_k(5))
        .await?;

    // Ready to inject straight into an LLM prompt
    println!("{}", context.as_prompt_context());

    Ok(())
}
```

Full surface:

```rust
impl MemoryEngine {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self>;
    pub async fn open_with_config(path: impl AsRef<Path>, config: Config) -> Result<Self>;
    pub async fn open_with_store(path: impl AsRef<Path>, store: impl VectorStore) -> Result<Self>;

    pub async fn store(&self, content: &str, memory_type: MemoryType, importance: f32) -> Result<String>;
    pub async fn store_with_options(&self, request: StoreRequest) -> Result<String>;

    pub async fn recall(&self, query: RecallQuery) -> Result<RecallResult>;
    pub async fn get(&self, id: &str) -> Result<Option<Memory>>;
    pub async fn update(&self, id: &str, update: MemoryUpdate) -> Result<()>;
    pub async fn forget(&self, id: &str) -> Result<()>;
    pub async fn forget_many(&self, filter: MemoryFilter) -> Result<usize>;
    pub async fn purge_expired(&self) -> Result<usize>;
    pub async fn stats(&self) -> Result<MemoryStats>;
}
```

---

## 9. The Vector Backend Trait — Pluggable Storage

The seam that keeps this project honest as *systems* engineering, not just an API wrapper:

```rust
#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn insert(&self, id: &str, vector: &[f32], metadata: HashMap<String, serde_json::Value>) -> Result<()>;
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    async fn delete(&self, id: &str) -> Result<()>;
    fn dimension(&self) -> usize;
}
```

Two implementations ship by default:

- **`InMemoryVectorStore`** — flat cosine similarity over an in-memory `HashMap`. Zero external dependencies. Comfortable up to roughly 100,000 vectors.
- **`OasysDbVectorStore`** *(optional, `features = ["oasysdb"]`)* — a thin REST client against the companion **OasysDB** self-hosted vector database, for when an application needs to scale beyond what fits in-process. Switching backends is a two-line change with no API changes and no data-format migration:

```rust
// Before: default, in-memory
let engine = MemoryEngine::open("./agent.db").await?;

// After: backed by OasysDB
let store = OasysDbVectorStore::new("http://localhost:8080", "oasys_sk_...", "agent-memories", 384);
let engine = MemoryEngine::open_with_store("./agent.db", store).await?;
```

---

## 10. Feature List — What We Are Building

### Must-ship (v1 core)
- [ ] Typed memory model (`Semantic` / `Episodic` / `Procedural` / `Working`) with per-type TTL and decay rate
- [ ] SQLite-backed persistence layer with migrations
- [ ] Local embedding generation (ONNX-based, runs on CPU, no API key required)
- [ ] `InMemoryVectorStore` with cosine similarity search
- [ ] Hybrid ranking engine: `similarity × importance × recency × reinforcement`
- [ ] `RecallQuery` builder: filter by type, minimum importance, max age
- [ ] Context assembly: `RecallResult::as_prompt_context()` formatted output
- [ ] `MemoryEngine` public API: `store`, `recall`, `get`, `update`, `forget`, `forget_many`, `purge_expired`, `stats`
- [ ] `VectorStore` trait + pluggable backend architecture
- [ ] `OasysDbVectorStore` adapter behind a feature flag, with an end-to-end integration test
- [ ] Full rustdoc coverage + working `examples/` (basic usage, agent loop)

### High-signal differentiators (build if time allows)
- [ ] **Contradiction tracking** — detect and mark superseded memories instead of silent overwrite
- [ ] **Temporal querying** — "what changed recently," "what's now stale," not just semantic lookup
- [ ] **Streaming ingestion** — incremental memory formation from a token stream via Tokio channels, instead of only batch `store()` calls
- [ ] **Memory compression** — periodic clustering/summarization of old, low-importance memories so retrieval quality doesn't degrade as the store grows
- [ ] **Confidence scoring** — track whether a memory was explicit, inferred, or repeated, and weight retrieval accordingly
- [ ] **Async background decay/purge worker** — a Tokio task that periodically purges expired `Working` memories without blocking the main API

### Explicitly out of scope for v1
- Bring-your-own LLM extraction per write (design choice — this is not a Mem0 clone)
- Multi-agent shared memory bus / distributed memory
- Graph-based relationship modeling between memories
- A hosted/cloud version of the service
- A UI or dashboard

---

## 11. What We Are Deliberately NOT Building (v1)

To keep this shippable and focused rather than an endless "memory OS," the following are explicitly deferred:

- No custom-trained embedding models — a local pretrained ONNX model is enough
- No distributed/multi-node memory
- No graph database / relationship-edge modeling between memories
- No fine-tuning or LoRA-based compression (referenced in some competitor research, not required here)
- No GPU dependency — this runs entirely on CPU by design, so it stays genuinely local-first

---

## 12. Tech Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust | Memory safety, predictable performance, no GC pauses, embeddable |
| Async runtime | Tokio | Concurrent ingestion + retrieval without blocking |
| Persistence | SQLite (`rusqlite`) | Zero-config, single-file, no external server |
| Embeddings | `fastembed-rs` (ONNX, local) | No API key, no per-call cost, runs on CPU |
| Serialization | `serde`, `bincode` | Fast, compact vector encoding |
| Vector backend (default) | In-process flat cosine index | Zero dependencies, good to ~100k vectors |
| Vector backend (scale-up) | OasysDB (companion Rust vector DB, REST) | Swap-in backend behind the same trait |
| Error handling | `thiserror` / `anyhow` | Structured, typed errors throughout |

---

## 13. System Diagram

```
              ┌────────────────────────────┐
              │        AI Agent            │
              │  (your Rust application)   │
              └─────────────┬──────────────┘
                            │
                 store() /  recall()
                            │
              ┌─────────────▼──────────────┐
              │       MemoryEngine         │
              │  ingestion · retrieval ·   │
              │      context assembly      │
              └──────┬───────────────┬─────┘
                     │               │
         ┌───────────▼───┐   ┌───────▼────────────┐
         │   SQLite       │   │  VectorStore trait │
         │ memories table │   │  (pluggable)       │
         └────────────────┘   └───────┬────────────┘
                                       │
                     ┌─────────────────┴─────────────────┐
                     ▼                                    ▼
          ┌─────────────────────┐              ┌─────────────────────┐
          │ InMemoryVectorStore │              │  OasysDbVectorStore │
          │  (default, no deps) │              │  (scale-up, REST)   │
          └─────────────────────┘              └─────────────────────┘
```

---

## 14. Build Roadmap

| Phase | Focus | Deliverable |
|---|---|---|
| 1 | Foundation | SQLite schema, typed `Memory` model, CRUD without embeddings |
| 2 | Embeddings + retrieval | Local ONNX embedding, `InMemoryVectorStore`, working `recall()` |
| 3 | Ranking | Full hybrid scoring formula, `RecallQuery` filtering, decay tests |
| 4 | Context assembly | `as_prompt_context()`, structured + prompt-ready output formats |
| 5 | Scale-up path | `OasysDbVectorStore` adapter, feature flag, integration test |
| 6 | Differentiators | Contradiction tracking, temporal queries, streaming ingestion |
| 7 | Polish | Full rustdoc, examples, benchmarks, README, `crates.io` publish |

---

## 15. Why This Project Is a Strong Engineering Signal

This project is built to demonstrate, concretely and defensibly:

- **Async Rust** — the whole engine is async end-to-end, with background workers for decay/purge
- **Trait-based abstraction** — the `VectorStore` trait decouples memory logic from storage backend
- **Storage engine thinking** — SQLite schema design, index strategy, bincode-encoded vector blobs
- **API design** — a small, clean public surface (`store`/`recall`/`forget`) hiding real internal complexity
- **Algorithmic reasoning** — a ranking formula that can be explained, justified, and tested, not a black box
- **Systems tradeoffs** — clear, documented reasoning for when in-memory search is enough vs. when to scale to a dedicated vector store

In an interview, every one of these choices has a one-sentence justification ready — that's the actual difference between "a portfolio project" and "a project that gets you the interview."

---

## 16. Comparison to Existing Projects

| Project | Embeddable library | Own vector store | Zero external deps | Typed memory | Hybrid ranking |
|---|---|---|---|---|---|
| Mem0 (cloud) | ✗ | ✗ | ✗ | Partial | ✗ |
| ICM | ✗ (binary) | ✗ | ✓ | Partial | ✗ |
| engram-mcp | ✗ (binary) | ✗ | ✓ | Partial | ✗ |
| mem0-rust | ✓ | ✗ (needs Qdrant) | ✗ | ✗ | ✗ |
| **ContextMemory (this project)** | **✓** | **via trait** | **✓** | **✓** | **✓** |

---

## 17. Risks and Honest Limitations

- **The in-memory vector store does not scale past roughly 100k vectors.** This is a documented limit, not a bug — it's the intentional upgrade trigger to a dedicated backend like OasysDB.
- **Local embedding model download (~100MB) on first run** can be a rough first-run experience on a slow connection; this will be clearly documented, with an option to supply a pre-downloaded model path.
- **Ranking weights are currently fixed constants**, not learned or auto-tuned — this is a deliberate simplicity tradeoff for v1, and is called out explicitly rather than oversold.
- **No recall-vs-ground-truth benchmarking exists yet** — retrieval quality claims in this README describe the *design intent* of the ranking formula, not a formally measured recall/precision number. That measurement is a planned follow-up, not a hidden gap.

---

*This README describes the target system. Sections will be updated as each phase in the roadmap ships, with real benchmark numbers replacing design-intent claims once measured.*