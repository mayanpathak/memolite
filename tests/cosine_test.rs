//! External, public-API tests for `InMemoryVectorStore`'s cosine-similarity
//! hardening (Phase 1, Step 8): f64 accumulation, zero-norm handling, and
//! non-finite rejection on both insert and search.
//!
//! These only touch `InMemoryVectorStore`'s public trait methods -- never a
//! private field -- so, unlike lock-poisoning, they can live in `tests/`.

use memolite::{InMemoryVectorStore, MemoliteError, VectorStore};
use uuid::Uuid;

#[tokio::test]
async fn zero_norm_vector_yields_zero_similarity_not_an_error() {
    let store = InMemoryVectorStore::new(2);
    let id = Uuid::new_v4();
    store
        .insert(id, &[0.0, 0.0], Default::default())
        .await
        .expect("inserting a zero vector must not fail validation");

    let hits = store.search(&[1.0, 0.0], 1).await.expect("search should succeed");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].score, 0.0);
}

#[tokio::test]
async fn large_finite_vectors_do_not_overflow_cosine() {
    // f32::MAX squared overflows plain f32 accumulation but not f64 -- this
    // proves cosine() accumulates in f64 rather than f32 (Step 8).
    let store = InMemoryVectorStore::new(2);
    let id = Uuid::new_v4();
    let big = f32::MAX / 2.0;

    store
        .insert(id, &[big, big], Default::default())
        .await
        .expect("inserting a large-but-finite vector must not fail");

    let hits = store.search(&[big, big], 1).await.expect("search should succeed");
    assert!(
        hits[0].score.is_finite(),
        "identical large-but-finite vectors must still produce a finite similarity score"
    );
    assert!((hits[0].score - 1.0).abs() < 1e-3);
}

#[tokio::test]
async fn non_finite_insert_is_rejected() {
    let store = InMemoryVectorStore::new(2);
    let result = store
        .insert(Uuid::new_v4(), &[f32::NAN, 0.0], Default::default())
        .await;
    assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
}

#[tokio::test]
async fn non_finite_query_is_rejected() {
    let store = InMemoryVectorStore::new(2);
    store
        .insert(Uuid::new_v4(), &[1.0, 0.0], Default::default())
        .await
        .unwrap();

    let result = store.search(&[f32::INFINITY, 0.0], 1).await;
    assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
}

#[tokio::test]
async fn wrong_dimension_query_is_rejected_not_silently_truncated() {
    let store = InMemoryVectorStore::new(3);
    store
        .insert(Uuid::new_v4(), &[1.0, 0.0, 0.0], Default::default())
        .await
        .unwrap();

    let result = store.search(&[1.0, 0.0], 1).await;
    assert!(matches!(result, Err(MemoliteError::VectorStore(_))));
}