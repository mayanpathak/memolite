//! Integration tests for `MemoryEngine::forget()` beyond the basic
//! "get() returns None afterward" check already in `crud_test.rs`:
//! malformed-id rejection with zero side effects, no-op on a well-formed
//! but missing id, and (indirectly, via `recall()`, since the live vector
//! index isn't part of the public API) removal from the vector store too.
//!
//! The one test here that touches a real file on disk uses `TempDb`
//! (Phase 7, Step 46) instead of a manual `std::fs::remove_file` tail call.
//! The other two open the engine against `:memory:`, so there is no file to
//! clean up at all.

mod common;

use common::TempDb;
use memolite::{MemoliteError, MemoryEngine, MemoryType};
use uuid::Uuid;

#[tokio::test]
async fn forget_rejects_a_malformed_id_with_zero_side_effects() {
    let db = TempDb::new("malformed");
    let engine = MemoryEngine::open(db.path())
        .await
        .expect("engine should open");

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

    // No manual cleanup needed -- `db` removes the file when it drops.
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