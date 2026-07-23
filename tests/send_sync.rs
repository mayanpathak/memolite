// tests/send_sync.rs
//! Compile-time proof that `MemoryEngine` can cross task/thread boundaries.
//!
//! This is a type-level assertion, not a runtime test: if `MemoryEngine`
//! stops being `Send + Sync`, this file fails to *compile*, not to pass.
//! That's intentional — it must fail before M8's `Arc<MemoryEngine>` +
//! `tokio::spawn` even gets a chance to surface the same problem buried
//! under streaming/maintenance code.

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

#[test]
fn memory_engine_is_send_and_sync() {
    assert_send::<memolite::MemoryEngine>();
    assert_sync::<memolite::MemoryEngine>();
}

// Same proof for the trait object callers actually pass around, since
// `Arc<dyn VectorStore>` is what crosses task boundaries in practice
// (engine.rs's `vector_store: RwLock<Arc<dyn VectorStore>>` field).
#[test]
fn arc_dyn_vector_store_is_send_and_sync() {
    assert_send::<std::sync::Arc<dyn memolite::VectorStore>>();
    assert_sync::<std::sync::Arc<dyn memolite::VectorStore>>();
}

// tests/send_sync.rs (continued)
use std::sync::Arc;

#[tokio::test]
async fn concurrent_store_and_recall_across_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(
        memolite::MemoryEngine::open(dir.path().join("concurrent.db"))
            .await
            .unwrap(),
    );

    let mut handles = Vec::new();
    for i in 0..8 {
        let engine = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            engine
                .store(
                    &format!("concurrent fact {i}"),
                    memolite::MemoryType::Semantic,
                    0.5,
                )
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    let results = engine.recall("concurrent fact").await.unwrap();
    assert_eq!(results.len(), 8);
}