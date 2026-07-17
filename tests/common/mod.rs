//! Shared test fixtures for M3 integration tests (Phase 2, Step 8).
//!
//! This file is named `mod.rs` inside `tests/common/` specifically so
//! `cargo test` does NOT treat it as its own standalone test binary --
//! it's only compiled when a real test file does `mod common;`.
//!
//! Three fixtures live here:
//!
//! - [`FakeVectorStore`]: a deterministic `VectorStore` test double with
//!   forced-failure switches, used everywhere a test needs to prove the
//!   engine's compensation/error-surfacing behavior on a *controlled*
//!   backend failure (something a real `InMemoryVectorStore` can't do).
//! - [`FakeEmbedder`]: a hash-based deterministic embedder. NOT injectable
//!   into `MemoryEngine` (its `embedder` field is a concrete
//!   `Mutex<Embedder>`, not a trait object -- Step 0 froze that shape).
//!   It exists for building test vectors cheaply, without loading the real
//!   ONNX model, wherever a test needs *some* vector and doesn't care that
//!   it carries real semantic meaning.
//! - [`TempDb`]: an RAII guard around a throwaway SQLite file. Replaces the
//!   old pattern of a `temp_db_path()` helper plus a manual
//!   `std::fs::remove_file(&path)` at the very end of a test body -- a
//!   manual cleanup call never runs if an earlier `assert!`/`expect()` in
//!   the same test panics, so failed tests were silently leaving `.db`
//!   files behind in the OS temp dir. Dropping `TempDb` removes the file
//!   unconditionally, panic or not (Phase 7, Step 43).
//!
//! Every test file that wants these must start with `mod common;`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use memolite::{MemoliteError, Result, VectorEntry, VectorHit, VectorStore};
use serde_json::Value;
use uuid::Uuid;

type StoredVector = (Vec<f32>, HashMap<String, Value>);

/// Deterministic, in-process `VectorStore` test double.
///
/// Behaves like a real backend (insert/search/delete/contains/replace_all
/// all work against a plain `HashMap`, with the same dimension/finite
/// checks a real backend is required to do) but exposes three
/// `Mutex<bool>` switches that force the *next and all subsequent* calls
/// of the matching kind to fail. This is how Phase 2/3 tests exercise the
/// engine's compensation and error-surfacing paths without needing a real
/// backend that can actually be made to fail on demand.
pub struct FakeVectorStore {
    data: Mutex<HashMap<Uuid, StoredVector>>,
    dim: usize,
    pub fail_insert: Mutex<bool>,
    pub fail_delete: Mutex<bool>,
    pub fail_search: Mutex<bool>,
}

impl FakeVectorStore {
    pub fn new(dim: usize) -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            dim,
            fail_insert: Mutex::new(false),
            fail_delete: Mutex::new(false),
            fail_search: Mutex::new(false),
        }
    }

    /// Convenience constructor for tests that want `insert` to fail from
    /// the very first call (e.g. `store()`'s compensation test).
    pub fn always_failing_insert(dim: usize) -> Self {
        let store = Self::new(dim);
        *store.fail_insert.lock().unwrap() = true;
        store
    }

    /// Convenience constructor for tests exercising `forget()`'s
    /// compensation path.
    pub fn always_failing_delete(dim: usize) -> Self {
        let store = Self::new(dim);
        *store.fail_delete.lock().unwrap() = true;
        store
    }

    /// Convenience constructor for tests exercising `recall()`'s
    /// search-failure path (proves no access-stat mutation happens).
    pub fn always_failing_search(dim: usize) -> Self {
        let store = Self::new(dim);
        *store.fail_search.lock().unwrap() = true;
        store
    }

    /// How many entries are currently in the fake store. Useful for
    /// asserting compensation/reconciliation actually changed something.
    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test-only escape hatch: insert directly into the fake backend,
    /// bypassing the `fail_insert` flag and dimension/finite validation.
    /// Used to simulate a vector-store entry with no matching SQLite row
    /// (the "stale hit" scenario in recall tests), or to pre-seed state
    /// without going through `MemoryEngine::store()` at all.
    pub fn insert_raw(&self, id: Uuid, vector: Vec<f32>) {
        self.data
            .lock()
            .unwrap()
            .insert(id, (vector, HashMap::new()));
    }

    fn should_fail(flag: &Mutex<bool>) -> bool {
        *flag.lock().unwrap()
    }
}

#[async_trait]
impl VectorStore for FakeVectorStore {
    async fn insert(
        &self,
        id: Uuid,
        vector: &[f32],
        metadata: HashMap<String, Value>,
    ) -> Result<()> {
        if Self::should_fail(&self.fail_insert) {
            return Err(MemoliteError::VectorStore(
                "simulated insert failure".into(),
            ));
        }
        if vector.len() != self.dim {
            return Err(MemoliteError::VectorStore(format!(
                "vector for {id} has dimension {} but store expects {}",
                vector.len(),
                self.dim
            )));
        }
        if !vector.iter().all(|v| v.is_finite()) {
            return Err(MemoliteError::VectorStore(format!(
                "vector for {id} contains a non-finite value"
            )));
        }

        self.data
            .lock()
            .unwrap()
            .insert(id, (vector.to_vec(), metadata));
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        if Self::should_fail(&self.fail_search) {
            return Err(MemoliteError::VectorStore(
                "simulated search failure".into(),
            ));
        }
        if query.len() != self.dim {
            return Err(MemoliteError::VectorStore(format!(
                "query has dimension {} but store expects {}",
                query.len(),
                self.dim
            )));
        }

        let guard = self.data.lock().unwrap();
        let mut hits: Vec<VectorHit> = guard
            .iter()
            .map(|(id, (vector, _))| VectorHit {
                id: *id,
                score: fake_cosine(query, vector),
            })
            .collect();

        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(k);
        Ok(hits)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        if Self::should_fail(&self.fail_delete) {
            return Err(MemoliteError::VectorStore(
                "simulated delete failure".into(),
            ));
        }
        self.data.lock().unwrap().remove(&id);
        Ok(())
    }

    async fn contains(&self, id: Uuid) -> Result<bool> {
        Ok(self.data.lock().unwrap().contains_key(&id))
    }

    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()> {
        let mut replacement = HashMap::with_capacity(entries.len());
        for entry in entries {
            if entry.vector.len() != self.dim {
                return Err(MemoliteError::VectorStore(format!(
                    "entry for {} has dimension {} but store expects {}",
                    entry.id,
                    entry.vector.len(),
                    self.dim
                )));
            }
            if !entry.vector.iter().all(|v| v.is_finite()) {
                return Err(MemoliteError::VectorStore(format!(
                    "entry for {} contains a non-finite value",
                    entry.id
                )));
            }
            replacement.insert(entry.id, (entry.vector, entry.metadata));
        }
        *self.data.lock().unwrap() = replacement;
        Ok(())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

/// Plain cosine similarity used only inside this fixture's `search()`.
/// Deliberately reimplemented here (rather than calling into
/// `InMemoryVectorStore`'s hardened version) so a bug in the real
/// implementation can never accidentally hide behind a fixture that shares
/// the bug.
fn fake_cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Deterministic, hash-based stand-in for the real `fastembed`-backed
/// `Embedder`.
///
/// NOT injectable into `MemoryEngine` -- its `embedder` field is a
/// concrete `Mutex<Embedder>`, not a trait object, so any test that opens
/// a real `MemoryEngine` still pays for the real ONNX model load. What
/// this type is for: producing cheap, reproducible vectors in tests that
/// build state directly against a `VectorStore` (`FakeVectorStore` or
/// `InMemoryVectorStore`) or via raw SQL against the `embeddings` table,
/// without needing the real model. The same input always produces the
/// same output, and different input (overwhelmingly likely) produces a
/// different output -- enough for ordering/dedup/exclusion tests that
/// don't depend on real semantic meaning.
pub struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Mirrors `Embedder::embed`'s signature and empty-input error
    /// behavior, so call sites can switch between the two with minimal
    /// friction.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            return Err(MemoliteError::EmptyEmbeddingInput);
        }

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut vector = Vec::with_capacity(self.dim);
        for i in 0..self.dim {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            i.hash(&mut hasher);
            let bits = hasher.finish();
            // Spread the hash into roughly [-1.0, 1.0].
            let scaled = ((bits % 2_000_001) as f32 / 1_000_000.0) - 1.0;
            vector.push(scaled);
        }

        // Normalize to unit length so fixture vectors behave like the real
        // model's output for cosine-similarity purposes.
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vector {
                *v /= norm;
            }
        }

        Ok(vector)
    }
}

/// RAII guard around a throwaway SQLite file used by an integration test.
///
/// `TempDb::new(test_name)` picks a unique path under `std::env::temp_dir()`
/// tagged with `test_name` -- nothing is created on disk yet; that happens
/// the first time something (typically `MemoryEngine::open`) opens the
/// path. Whenever the guard is dropped -- normal end of test, early
/// `return`, or an `assert!`/`expect()` panic partway through -- its `Drop`
/// impl removes the file. This replaces every test's old hand-written
/// `std::fs::remove_file(&path).expect(...)` tail call, which only ran on a
/// clean, non-panicking exit and therefore left files behind on any test
/// failure.
///
/// Usage:
/// ```ignore
/// let db = TempDb::new("my-test");
/// let engine = MemoryEngine::open(db.path()).await.unwrap();
/// // ... use engine ...
/// // no manual cleanup needed -- `db` removes the file when it drops.
/// ```
///
/// If a test needs the engine's file handle released before doing more
/// work against the same path (e.g. opening a second raw `Connection`, or
/// reopening via `MemoryEngine::open` again), it still needs an explicit
/// `drop(engine)` at that point -- `TempDb` only controls the *file*, not
/// how long the `MemoryEngine` holding it stays alive.
pub struct TempDb {
    path: std::path::PathBuf,
}

impl TempDb {
    /// Builds a unique, not-yet-existing path under the OS temp dir. The
    /// `test_name` tag is purely for readability if a leftover file is ever
    /// inspected by hand; a random UUID guarantees no collision between
    /// parallel `cargo test` runs even when several tests pass the same
    /// `test_name`.
    pub fn new(test_name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "memolite-test-{test_name}-{}.db",
            Uuid::new_v4()
        ));
        Self { path }
    }

    /// The path this guard owns, for passing straight into
    /// `MemoryEngine::open` or `rusqlite::Connection::open`.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        // Best-effort: the file may never have been created (e.g. the test
        // failed before `MemoryEngine::open` ran), so a missing-file error
        // here is not itself a problem worth failing the test over.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod fixture_self_tests {
    use super::*;

    #[tokio::test]
    async fn fake_vector_store_insert_and_search_round_trip() {
        let store = FakeVectorStore::new(3);
        let id = Uuid::new_v4();
        store
            .insert(id, &[1.0, 0.0, 0.0], HashMap::new())
            .await
            .unwrap();

        let hits = store.search(&[1.0, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn fake_vector_store_forced_insert_failure() {
        let store = FakeVectorStore::always_failing_insert(3);
        let result = store.insert(Uuid::new_v4(), &[1.0, 0.0, 0.0], HashMap::new()).await;
        assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn fake_vector_store_forced_delete_failure_leaves_entry_in_place() {
        let store = FakeVectorStore::always_failing_delete(3);
        let id = Uuid::new_v4();
        store
            .insert(id, &[1.0, 0.0, 0.0], HashMap::new())
            .await
            .unwrap();

        let result = store.delete(id).await;
        assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
        assert!(store.contains(id).await.unwrap());
    }

    #[tokio::test]
    async fn fake_vector_store_forced_search_failure() {
        let store = FakeVectorStore::always_failing_search(3);
        let result = store.search(&[1.0, 0.0, 0.0], 5).await;
        assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
    }

    #[test]
    fn fake_embedder_is_deterministic() {
        let embedder = FakeEmbedder::new(8);
        let a = embedder.embed("hello world").unwrap();
        let b = embedder.embed("hello world").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 8);
    }

    #[test]
    fn fake_embedder_rejects_empty_input() {
        let embedder = FakeEmbedder::new(8);
        assert!(matches!(
            embedder.embed("   "),
            Err(MemoliteError::EmptyEmbeddingInput)
        ));
    }

    #[test]
    fn fake_embedder_differs_for_different_text() {
        let embedder = FakeEmbedder::new(8);
        let a = embedder.embed("alpha").unwrap();
        let b = embedder.embed("beta").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn temp_db_removes_its_file_on_drop() {
        let path = {
            let db = TempDb::new("self-test");
            std::fs::write(db.path(), b"placeholder").unwrap();
            assert!(db.path().exists());
            db.path().to_path_buf()
        }; // db dropped here
        assert!(
            !path.exists(),
            "TempDb must remove its file when dropped"
        );
    }
}