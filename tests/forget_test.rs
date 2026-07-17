//! Integration tests for `MemoryEngine::forget()` beyond the basic
//! "get() returns None afterward" check already in `crud_test.rs`:
//! malformed-id rejection with zero side effects, no-op on a well-formed
//! but missing id, and (indirectly, via `recall()`, since the live vector
//! index isn't part of the public API) removal from the vector store too.

use memolite::{MemoliteError, MemoryEngine, MemoryType};
use uuid::Uuid;

fn temp_db_path(test_name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "memolite-forget-test-{test_name}-{}.db",
        Uuid::new_v4()
    ))
}

#[tokio::test]
async fn forget_rejects_a_malformed_id_with_zero_side_effects() {
    let path = temp_db_path("malformed");
    let engine = MemoryEngine::open(&path).await.expect("engine should open");

    let id = engine
        .store(
            "should survive a malformed forget() call",
            MemoryType::Working,
            0.5,
        )
        .await
        .expect("store should succeed");

    let result = engine.forget("not-a-uuid").await;
    assert!(matches!(result, Err(MemoliteError::InvalidUuid(_))));

    // Zero side effects: the real memory is untouched.
    assert!(
        engine
            .get(&id)
            .await
            .expect("get should succeed")
            .is_some()
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

#[tokio::test]
async fn forget_on_a_wellformed_but_nonexistent_id_is_a_silent_noop() {
    let engine = MemoryEngine::open(":memory:")
        .await
        .expect("engine should open");

    let result = engine.forget(&Uuid::new_v4().to_string()).await;
    assert!(
        result.is_ok(),
        "forgetting a well-formed but missing id must not be an error"
    );
}

#[tokio::test]
async fn forgotten_memory_is_removed_from_both_sqlite_and_the_live_vector_index() {
    let engine = MemoryEngine::open(":memory:")
        .await
        .expect("engine should open");

    let id = engine
        .store(
            "a very particular fact about ferrets",
            MemoryType::Semantic,
            0.7,
        )
        .await
        .expect("store should succeed");

    // Sanity check it's actually findable before forgetting it.
    let before = engine.recall("ferrets").await.expect("recall should succeed");
    assert!(before.iter().any(|m| m.id.to_string() == id));

    engine.forget(&id).await.expect("forget should succeed");

    // Gone from SQLite...
    assert!(
        engine
            .get(&id)
            .await
            .expect("get should succeed")
            .is_none()
    );

    // ...and gone from the live vector index too -- recall() searches the
    // vector index first, so a stale entry there would still surface here
    // even though SQLite no longer has the row.
    let after = engine.recall("ferrets").await.expect("recall should succeed");
    assert!(after.iter().all(|m| m.id.to_string() != id));
}