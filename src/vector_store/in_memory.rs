use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use super::{validate_vector, VectorEntry, VectorHit, VectorStore};
use crate::error::{MemoliteError, Result};

/// Default, zero-external-dependency `VectorStore`: a `HashMap` guarded by
/// an `RwLock`, computing cosine similarity by brute-force linear scan.
/// O(n) per search -- an accepted, documented limitation, not a hidden one.
pub struct InMemoryVectorStore {
    dim: usize,
    data: RwLock<HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>,
}

impl InMemoryVectorStore {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            data: RwLock::new(HashMap::new()),
        }
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        dot / (na * nb)
    }

    fn lock_read(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>>
    {
        self.data
            .read()
            .map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))
    }

    fn lock_write(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<Uuid, (Vec<f32>, HashMap<String, Value>)>>>
    {
        self.data
            .write()
            .map_err(|_| MemoliteError::Internal("vector store lock poisoned".into()))
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
        let mut hits: Vec<VectorHit> = guard
            .iter()
            .map(|(id, (v, _))| VectorHit {
                id: *id,
                score: Self::cosine(query, v),
            })
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

    async fn replace_all(&self, entries: Vec<VectorEntry>) -> Result<()> {
        let mut replacement = HashMap::with_capacity(entries.len());
        for e in entries {
            validate_vector(&format!("entry for {}", e.id), &e.vector, self.dim)?;
            replacement.insert(e.id, (e.vector, e.metadata));
        }
        *self.lock_write()? = replacement;
        Ok(())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
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

    // search() must validate its OWN input -- not rely on replace_all's
    // validation "covering" it by proximity.
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
        store
            .replace_all(vec![VectorEntry { id: kept, vector: vec![0.0, 1.0], metadata: HashMap::new() }])
            .await
            .unwrap();
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