use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use super::{VectorEntry, VectorHit, VectorStore, validate_vector};
use crate::error::{MemoliteError, Result};

type MetadataMap = HashMap<String, Value>;
type StoredVector = (Vec<f32>, MetadataMap);
type VectorMap = HashMap<Uuid, StoredVector>;

pub struct InMemoryVectorStore {
    dim: usize,
    data: RwLock<VectorMap>,
}

impl InMemoryVectorStore {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            data: RwLock::new(HashMap::new()),
        }
    }

    fn cosine(a: &[f32], b: &[f32]) -> Result<f32> {
        for &v in a.iter().chain(b.iter()) {
            if !v.is_finite() {
                return Err(MemoliteError::VectorStore(
                    "non-finite value in cosine input".into(),
                ));
            }
        }

        let mut dot: f64 = 0.0;
        let mut norm_a: f64 = 0.0;
        let mut norm_b: f64 = 0.0;
        for (&x, &y) in a.iter().zip(b.iter()) {
            let (x, y) = (x as f64, y as f64);
            dot += x * y;
            norm_a += x * x;
            norm_b += y * y;
        }
        let norm_a = norm_a.sqrt();
        let norm_b = norm_b.sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            return Ok(0.0);
        }

        let similarity = dot / (norm_a * norm_b);
        if !similarity.is_finite() {
            return Err(MemoliteError::VectorStore(
                "non-finite cosine similarity computed".into(),
            ));
        }

        Ok(similarity as f32)
    }

    fn lock_read(&self) -> Result<std::sync::RwLockReadGuard<'_, VectorMap>> {
        self.data
            .read()
            .map_err(|_| MemoliteError::VectorStore("vector store lock poisoned".into()))
    }

    fn lock_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, VectorMap>> {
        self.data
            .write()
            .map_err(|_| MemoliteError::VectorStore("vector store lock poisoned".into()))
    }
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn insert(
        &self,
        id: Uuid,
        vector: &[f32],
        metadata: HashMap<String, Value>,
    ) -> Result<()> {
        validate_vector(&format!("vector for {id}"), vector, self.dim)?;
        self.lock_write()?.insert(id, (vector.to_vec(), metadata));
        Ok(())
    }

    async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        validate_vector("query", query, self.dim)?;

        let guard = self.lock_read()?;
        let mut hits: Vec<VectorHit> = guard
            .iter()
            .map(|(id, (vector, _))| {
                Self::cosine(query, vector).map(|score| VectorHit { id: *id, score })
            })
            .collect::<Result<Vec<VectorHit>>>()?;

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

        for entry in entries {
            validate_vector(&format!("entry for {}", entry.id), &entry.vector, self.dim)?;
            replacement.insert(entry.id, (entry.vector, entry.metadata));
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

        assert!(
            store
                .insert(Uuid::new_v4(), &[1.0, 0.0], HashMap::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn non_finite_insert_is_rejected() {
        let store = InMemoryVectorStore::new(2);

        assert!(
            store
                .insert(Uuid::new_v4(), &[f32::NAN, 0.0], HashMap::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn wrong_dimension_query_is_rejected_not_silently_truncated() {
        let store = InMemoryVectorStore::new(3);

        store
            .insert(Uuid::new_v4(), &[1.0, 0.0, 0.0], HashMap::new())
            .await
            .unwrap();

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
        let kept = Uuid::new_v4();

        store
            .insert(stale, &[1.0, 0.0], HashMap::new())
            .await
            .unwrap();

        store
            .replace_all(vec![VectorEntry {
                id: kept,
                vector: vec![0.0, 1.0],
                metadata: HashMap::new(),
            }])
            .await
            .unwrap();

        assert!(!store.contains(stale).await.unwrap());
        assert!(store.contains(kept).await.unwrap());
    }

    #[tokio::test]
    async fn replace_all_leaves_store_untouched_on_validation_failure() {
        let store = InMemoryVectorStore::new(2);
        let original = Uuid::new_v4();

        store
            .insert(original, &[1.0, 0.0], HashMap::new())
            .await
            .unwrap();

        let bad = VectorEntry {
            id: Uuid::new_v4(),
            vector: vec![1.0],
            metadata: HashMap::new(),
        };

        assert!(store.replace_all(vec![bad]).await.is_err());
        assert!(store.contains(original).await.unwrap());
    }

    #[tokio::test]
    async fn zero_norm_vector_yields_zero_similarity_not_an_error() {
        let store = InMemoryVectorStore::new(2);
        let id = Uuid::new_v4();

        store.insert(id, &[0.0, 0.0], HashMap::new()).await.unwrap();

        let hits = store.search(&[1.0, 0.0], 1).await.unwrap();
        assert_eq!(hits[0].score, 0.0);
    }

    #[tokio::test]
    async fn large_finite_vectors_do_not_overflow_cosine() {
        let store = InMemoryVectorStore::new(2);
        let id = Uuid::new_v4();
        let big = f32::MAX / 2.0;

        store
            .insert(id, &[big, big], HashMap::new())
            .await
            .unwrap();

        let hits = store.search(&[big, big], 1).await.unwrap();
        assert!(
            hits[0].score.is_finite(),
            "identical large-but-finite vectors must still produce a finite similarity score"
        );
        assert!((hits[0].score - 1.0).abs() < 1e-3);
    }

    #[tokio::test]
    async fn lock_poisoning_surfaces_as_vectorstore_error_not_internal() {
        let store = std::sync::Arc::new(InMemoryVectorStore::new(2));
        let store_clone = std::sync::Arc::clone(&store);

        let _ = std::thread::spawn(move || {
            let _guard = store_clone.data.write().unwrap();
            panic!("intentionally poisoning the lock");
        })
        .join();

        let result = store.insert(Uuid::new_v4(), &[1.0, 0.0], HashMap::new()).await;
        assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
    }
}