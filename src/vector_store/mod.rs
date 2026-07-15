use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::error::{MemoliteError, Result};

pub mod in_memory;
pub use in_memory::InMemoryVectorStore;

// `generic_http` (M11) is intentionally NOT registered here yet. The file
// exists in the tree as an empty placeholder, but declaring
// `pub mod generic_http;` before it has real content -- and before the
// `generic-http` Cargo feature exists -- is exactly the kind of forward
// reference Step 0 must avoid.

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
/// exactly one validation function in the crate; no method on any backend
/// is allowed to skip calling it just because a sibling method already
/// checks something similar.
pub fn validate_vector(label: &str, v: &[f32], dim: usize) -> Result<()> {
    if v.len() != dim {
        return Err(MemoliteError::VectorStore(format!(
            "{label} has dimension {} but store expects {dim}",
            v.len()
        )));
    }
    if !v.iter().all(|x| x.is_finite()) {
        return Err(MemoliteError::VectorStore(format!(
            "{label} contains a non-finite value"
        )));
    }
    Ok(())
}

/// The seam between the engine and any concrete way of storing/searching
/// vectors. `replace_all` is the single reconciliation primitive used
/// everywhere the engine needs to make a backend's contents agree with
/// SQLite: restart backfill today, forget/purge failure recovery, and
/// later milestones' index rebuild.
/// 
/// 
/// 
/// 
/// 
/// 
///
/// Backend responsibilities (every implementor MUST uphold these):
/// - No duplicate IDs are ever stored -- `insert` is an upsert.
/// - Every similarity score returned from `search` is finite.
/// - `search` returns at most `k` results.
/// - `validate_vector` is called (and its error propagated) before storing
///   or searching any vector -- no implementor skips this because a
///   sibling method already checks something similar.
///
/// Engine behavior in M3 (documented limitation, not an oversight): the
/// engine trusts an in-tree backend's output without defensively
/// re-validating it (re-checking scores are finite, re-checking the
/// result count is `<= k`, etc). Defensive re-validation of backend output
/// is not implemented yet.
/// 
/// 
/// 
/// 
/// 
/// 
/// 
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// MUST be an idempotent upsert. MUST call `validate_vector` on
    /// `vector` before storing anything.
    async fn insert(
        &self,
        id: Uuid,
        vector: &[f32],
        metadata: HashMap<String, Value>,
    ) -> Result<()>;

    /// MUST call `validate_vector` on `query` before searching.
    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;

    /// MUST be idempotent: deleting a missing id is not an error.
    async fn delete(&self, id: Uuid) -> Result<()>;

    async fn contains(&self, id: Uuid) -> Result<bool>;

    /// Replaces the *entire* contents of this store with exactly `entries`.
    /// Any id currently present but absent from `entries` MUST be gone
    /// afterward; every id in `entries` MUST be present and correct
    /// afterward. MUST call `validate_vector` on every entry before storing
    /// anything -- all-or-nothing: a bad entry rejects the whole call and
    /// leaves the store untouched.
    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;

    fn dimension(&self) -> usize;

    /// Removes everything. Default implementation delegates to
    /// `replace_all` with an empty set, so most backends never need a
    /// separate destructive "clear" endpoint.
    async fn clear(&self) -> Result<()> {
        self.replace_all(Vec::new()).await
    }
}
