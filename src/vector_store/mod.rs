use std::collections::HashMap;

use async_trait::async_trait;
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

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn insert(
        &self,
        id: Uuid,
        vector: &[f32],
        metadata: HashMap<String, Value>,
    ) -> Result<()>;

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;

    async fn delete(&self, id: Uuid) -> Result<()>;

    async fn contains(&self, id: Uuid) -> Result<bool>;

    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()>;

    fn dimension(&self) -> usize;

    async fn clear(&self) -> Result<()> {
        self.replace_all(Vec::new()).await
    }
}