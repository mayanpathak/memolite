Okay, first — breathe. The doc you were given (ContextMemory) is a Rust systems project; the example repo (`samiksha0shukla/context-memory`) is a Python/FAISS/LLM version of a similar idea. They're solving the same conceptual problem in different languages, which is why cross-referencing them felt like reading two different subjects. Let's fix the mental model first, then break it into a literal step-by-step build.

## The mental model (read this twice, then forget the jargon)

Forget "vector databases" and "embeddings" for a second. Think of it like this:

You're building a **notebook for a forgetful assistant**. Every time it talks to a user, it should be able to:
1. **Write something down** ("this person prefers Rust") — that's `store()`
2. **Flip back through the notebook and find the relevant pages** when a new question comes in — that's `recall()`
3. **Decide what's worth mentioning**, not by cramming every old note into every answer, but by asking "how similar, how important, how recent, how often reused" — that's the ranking formula
4. **Cross out old notes that got replaced**, without literally erasing them — that's contradiction tracking

That's it. That's the whole project. Everything else (SQLite, embeddings, vector search) is just **implementation detail for "how do I search the notebook fast."**

The one genuinely new concept for you is **embeddings**: a way to turn text into a list of numbers (a vector) such that texts with similar *meaning* end up as numbers that are close together mathematically. "User loves Rust" and "User is a Rust fan" become nearly identical vectors even though the words differ. That's the only "AI" part of this whole system — everything else is normal backend engineering (schemas, traits, APIs) that you already know how to do from OasysDB/VecLite.

---

## How to use the milestone plan

Each milestone ends with a **checkpoint**: something you literally run and see pass/fail before moving on. Don't let yourself move to the next milestone until the checkpoint is green. That discipline is what turns "100 overwhelming steps" into "100 boring, doable steps."

---

## Milestone 0 — Skeleton, no AI at all (Steps 1–10)
Goal: a Rust project that compiles and has the right shape, storing zero real data.

1. `cargo new context-memory --lib`
2. Add `tokio`, `serde`, `serde_json`, `anyhow`, `thiserror`, `chrono`, `uuid` to `Cargo.toml`
3. Create `src/memory.rs` — define the `Memory` struct (id, content, memory_type, importance, access_count, created_at, last_accessed, expires_at, metadata, superseded_by) — no logic yet, just data
4. Define `enum MemoryType { Semantic, Episodic, Procedural, Working }` with `#[derive(Debug, Clone, Serialize, Deserialize)]`
5. Create `src/engine.rs` — empty `struct MemoryEngine {}` with a stub `async fn open(path) -> Result<Self>`
6. Add stub methods on `MemoryEngine`: `store()`, `recall()`, `get()` — each just `todo!()`
7. Write a `src/lib.rs` that re-exports these publicly
8. Write `examples/basic.rs` that calls `MemoryEngine::open()` and prints "ok" — get it to compile even though nothing works yet
9. Run `cargo build` — fix all compiler errors until it's green
10. **Checkpoint:** `cargo run --example basic` prints "ok" without panicking. This proves your types and module structure are sound before you add any real behavior.

## Milestone 1 — Real storage, no search yet (Steps 11–25)
Goal: you can `store()` a memory and `get()` it back from SQLite. No embeddings, no ranking.

11. Add `rusqlite` (with `bundled` feature so you don't need SQLite installed system-wide)
12. Write the `memories` table SQL from the doc (copy it — schema design isn't the hard part here)
13. In `MemoryEngine::open()`, open/create the SQLite file and run the `CREATE TABLE IF NOT EXISTS` on startup
14. Implement `store()`: generate a UUID, compute `created_at = now`, insert a row, return the id
15. Implement `get(id)`: `SELECT` by id, map the row back into a `Memory` struct
16. Implement `forget(id)`: `DELETE` by id
17. Write your first real test: store a memory, `get()` it, assert the content matches
18. Write a test: `forget()` a memory, then `get()` it, assert it returns `None`
19. Add TTL/decay defaults per type (just a `match memory_type { ... }` returning a `Duration`, nothing fancy)
20. On `store()`, compute and set `expires_at = created_at + ttl` for the type
21. Implement `purge_expired()`: `DELETE WHERE expires_at < now`
22. Test: manually insert a memory with `expires_at` in the past, call `purge_expired()`, assert it's gone
23. Wrap all SQLite errors properly using `thiserror` — no `.unwrap()` on database calls
24. Run `cargo clippy` and fix warnings — habit-build this now, not at the end
25. **Checkpoint:** Full `cargo test` green. You now have a working, typed, persistent, self-expiring key-value store. Zero AI. This is 50% of the "systems" credit already banked.

## Milestone 2 — Embeddings, the one truly new concept (Steps 26–40)
Goal: turn text into a vector, and understand why.

26. Read *one* explainer on embeddings (any intro article/video is fine) — just enough to answer "why does 'I like dogs' end up numerically close to 'dogs are great'?"
27. Add `fastembed` crate (this runs a small local ONNX model — no API key, no internet call per request)
28. Write a throwaway `examples/embed_test.rs`: embed the string "I love Rust", print the resulting `Vec<f32>` and its length
29. Embed two similar sentences and two unrelated ones; manually compute cosine similarity between each pair (write the formula yourself — it's ~5 lines: dot product / (norm_a * norm_b))
30. **Checkpoint (the "aha" moment):** print all four similarity scores. Similar sentences should score notably higher than unrelated ones. If they don't, something's misconfigured — don't move on until this makes intuitive sense to you.
31. Wrap embedding generation into a small `Embedder` struct with one method: `embed(&self, text: &str) -> Vec<f32>`
32. Add the `embeddings` table from the doc, storing the vector as a `bincode`-encoded blob
33. On `store()`, call `embedder.embed(content)` and insert into `embeddings` alongside the memory row
34. Test: store a memory, then directly query the `embeddings` table, assert a blob exists with the right dimension
35. Handle the first-run model download gracefully (log a message, don't crash if slow)
36. Implement a basic `dimension()` getter
37. Add error handling for embedding failures (empty string, etc.)
38. Refactor: `Embedder` should be constructed once in `MemoryEngine::open()` and reused, not recreated per call (model load is expensive)
39. Benchmark roughly how long one embed call takes (just `Instant::now()` around it) — get a feel for the cost
40. **Checkpoint:** every `store()` call now silently also produces and persists an embedding. `cargo test` still green.

## Milestone 3 — The `VectorStore` trait + naive search (Steps 41–55)
Goal: `recall("some query")` returns *something*, using pure cosine similarity, no ranking yet.

41. Define the `VectorStore` trait exactly as in the doc: `insert`, `search`, `delete`, `dimension`
42. Implement `InMemoryVectorStore`: internally just a `HashMap<String, (Vec<f32>, HashMap<String,Value>)>`
43. Implement `insert()`: just `.insert(id, (vector, metadata))`
44. Implement `search(query, k)`: loop over all entries, compute cosine similarity to `query`, sort descending, take top `k`
45. Implement `delete()`: `.remove(id)`
46. Write a unit test directly against `InMemoryVectorStore` (not the whole engine) — insert 3 vectors, search, assert the closest one comes back first
47. Wire `MemoryEngine` to own a `Box<dyn VectorStore>` field
48. On `store()`, after computing the embedding, also call `vector_store.insert()`
49. Implement `recall(query: &str)`: embed the query text, call `vector_store.search()`, get back a list of ids
50. For each returned id, `get()` the full `Memory` row from SQLite (this is the "join" step from the architecture diagram)
51. Return this as a plain `Vec<Memory>` for now (no `RecallResult` formatting yet)
52. Write an integration test: store 3 unrelated memories + 1 relevant one, `recall()` a query related to the fourth, assert it comes back in position 1
53. Handle the empty-store case (recall before anything is stored) without panicking
54. Update `access_count` and `last_accessed` on every memory returned from `recall()` — this feeds the ranking formula next
55. **Checkpoint:** you can `store()` five different facts and `recall()` a natural-language query and get the right one back. This is a working (if naive) memory system already — genuinely, most people stop here and call it done.

## Milestone 4 — The ranking formula (the "real" engineering, Steps 56–70)
Goal: replace raw similarity ranking with the four-factor formula.

56. Implement `recency_factor()` as its own testable function: `e^(-decay_rate * days_since_last_access)`
57. Hardcode decay rates per type from the doc (episodic ~14-day half-life, semantic ~693 days, etc.) — derive `decay_rate = ln(2) / half_life` yourself, don't just copy a number blindly, so you understand it
58. Implement `reinforcement_factor()`: `1.0 + ln(1 + access_count) * 0.1`
59. Unit test `recency_factor`: a memory accessed today should score ~1.0, one from 60 days ago should score much lower
60. Unit test `reinforcement_factor`: access_count=0 → 1.0, access_count=10 → notably higher but not runaway
61. Combine all four into `final_score = similarity * importance * recency * reinforcement`
62. Recreate the doc's worked example as a test: two synthetic memories, assert the "less similar but more durable" one outranks the "more similar but stale" one
63. Refactor `recall()`: fetch a larger candidate pool from the vector store (say top 20), then re-rank all 20 by `final_score`, then truncate to requested `top_k`
64. Add a `RecallQuery` builder: `.top_k(n)`, `.min_importance(x)`, `.memory_type(t)` — just struct fields + a fluent builder, nothing exotic
65. Apply these filters before scoring in `recall()`
66. Write a test for `.memory_type(Semantic)` filtering out episodic results even if more "similar"
67. Write a test for `.min_importance()` filtering
68. Add `RecallResult` struct wrapping `Vec<(Memory, f32 /* score */)>`
69. Implement `as_prompt_context()`: format the top results as a plain numbered/bulleted string, ready to paste into an LLM prompt
70. **Checkpoint:** run the doc's worked example end-to-end through the real engine (not a hand test) and confirm the ranking output matches your manual reasoning. This is the milestone your manager will actually be evaluating — get a second pair of eyes on it if you can.

## Milestone 5 — Contradiction tracking (Steps 71–78)
Goal: `superseded_by` actually gets set instead of silently overwritten data.

71. Implement `update(id, new_content)`: create a *new* memory row, set the *old* row's `superseded_by = new_id`
72. Don't delete the old row — that's the whole point (preserving history)
73. Modify `recall()` to exclude superseded memories from normal results by default
74. Add a `include_superseded: bool` option to `RecallQuery` for when someone explicitly wants history
75. Test: store "uses Python", then "update" it to "uses Rust", `recall()` normally → only Rust comes back; `recall()` with history → both come back, old one flagged
76. (Optional, skip if short on time) naive contradiction *detection*: if a new memory's embedding is highly similar to an existing one but the text differs, flag it as a possible contradiction for the caller to confirm — don't auto-guess semantics, that's genuinely hard and out of scope
77. Document clearly in code comments what counts as "detected" vs "explicitly told" contradiction — don't oversell this part
78. **Checkpoint:** history-preserving update test passes.

## Milestone 6 — Background decay/purge worker (Steps 79–85)
Goal: `Working` memories expire on their own without you calling `purge_expired()` manually.

79. Spawn a `tokio::task` in `MemoryEngine::open()` that loops with `tokio::time::interval`
80. Each tick, call `purge_expired()` internally
81. Make sure the task doesn't hold a lock that blocks normal `store`/`recall` calls while running
82. Test this is annoying to unit-test cleanly — instead, write an integration test with a very short artificial TTL (e.g. 2 seconds) and `tokio::time::sleep` to observe it get purged automatically
83. Add a graceful shutdown path (don't worry too much about perfection here — a documented known-limitation is fine)
84. Log (via `tracing` or plain `eprintln!` for now) whenever the worker purges something, so it's observable
85. **Checkpoint:** run the app, store a short-TTL memory, wait, confirm it's gone without you calling anything.

## Milestone 7 — Polish, docs, and the OasysDB adapter (Steps 86–100)
86. Write rustdoc `///` comments on every public struct/method in `MemoryEngine`
87. Add `open_with_config()` allowing custom TTLs/decay rates instead of hardcoded defaults
88. Add `open_with_store()` so callers can inject any `VectorStore` implementation
89. Behind `#[cfg(feature = "oasysdb")]`, write `OasysDbVectorStore` as a thin `reqwest`-based REST client implementing the same trait
90. Point it at your existing OasysDB instance (you already have this running from your other project — reuse it)
91. Write one end-to-end test that runs *only* when the `oasysdb` feature is enabled (use `#[cfg_attr]`/feature-gated tests) proving the swap requires no API changes
92. Write `examples/agent_loop.rs` simulating a mini chat loop: user says things, engine stores them, later queries recall relevant facts
93. Run `cargo clippy --all-features` and clean up every warning
94. Run `cargo fmt`
95. Write the real README: what it does, quick start, the four memory types, the ranking formula with the worked example, known limitations (be honest, per the doc's own "Risks" section — don't oversell)
96. Write a short "Architecture Decisions" doc: why SQLite, why the trait, why fixed-not-learned ranking weights — this is what turns it into interview material
97. Add basic benchmarks (`cargo bench` or even a manual timed loop) for `store()` and `recall()` at, say, 1k/10k memories — gives you real numbers instead of guesses
98. Tag a `v0.1.0`, publish-dry-run with `cargo publish --dry-run` (don't actually publish unless your manager wants that)
99. Do a full read-through pretending you're the interviewer: for every design choice, can you say the one-sentence justification out loud without notes?
100. **Final checkpoint:** `cargo test --all-features` green, `cargo clippy` clean, README complete, and you can explain milestone 4 (the ranking formula) to someone else from memory. That last part matters more than the code — that's what "understanding it" actually means.

---

A few honest notes so you don't get stuck:

- **Milestones 0–3 are ~60% of the total effort and 100% normal backend work.** You already know how to do all of it from your Rust background — the SQLite schema, the trait, the CRUD. Don't let the word "AI memory system" psych you out of that.
- **Milestone 2 (embeddings) is the only genuinely unfamiliar piece.** Budget real time for step 30's "aha moment" — once cosine similarity clicks for you concretely, the rest of the ranking math (milestone 4) is just arithmetic you're combining, not a new concept.
- If your manager's real deadline is tight, **milestones 0–4 are a legitimately complete, demoable project** (typed storage + real search + explainable ranking). Treat 5–7 as stretch goals, not blockers — the doc itself marks those as "differentiators, build if time allows."

If it'd help, I can also generate the actual starter code for Milestone 0 and 1 right now so you're not staring at a blank `Cargo.toml`.