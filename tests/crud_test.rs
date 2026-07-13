//! CRUD integration tests for `MemoryEngine`.
//!
//! These tests only touch the public API (`open`, `store`, `get`, `forget`)
//! -- exactly the way a real user of the crate would -- rather than reaching
//! into internals. Each test opens its own throwaway SQLite file so tests
//! don't interfere with each other when run in parallel.

use memolite::{MemoryEngine, MemoryType};
use uuid::Uuid;

/// Builds a unique path in the OS temp dir for each test run, so parallel
/// `cargo test` runs don't collide on the same `.db` file.
fn temp_db_path(test_name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "context-memory-test-{test_name}-{}.db",
        Uuid::new_v4()
    ))
}

/// Test 1: store a memory, then get() it back, and assert the content matches.
#[tokio::test]
async fn store_then_get_returns_matching_memory() {
    let path = temp_db_path("store-get");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("failed to open engine");

    // Store a memory and keep the returned ID.
    let id = engine
        .store("Rust is my favorite language", MemoryType::Semantic, 0.9)
        .await
        .expect("store() failed");

    // Fetch it back by that ID.
    let fetched = engine
        .get(&id)
        .await
        .expect("get() failed")
        .expect("expected Some(memory), got None");

    // The content we stored should come back unchanged.
    assert_eq!(fetched.content, "Rust is my favorite language");
    assert_eq!(fetched.memory_type, MemoryType::Semantic);
    assert_eq!(fetched.importance, 0.9);
    assert_eq!(fetched.id.to_string(), id);

    // Explicitly drop engine before removing the file
    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

/// Test 2: forget() a memory, then get() it, and assert it returns None.
#[tokio::test]
async fn forget_removes_the_memory() {
    let path = temp_db_path("forget");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("failed to open engine");

    // Store something first so there's a real memory to forget.
    let id = engine
        .store("temporary note", MemoryType::Working, 0.2)
        .await
        .expect("store() failed");

    // Sanity check: it exists before we forget it.
    assert!(engine.get(&id).await.expect("get() failed").is_some());

    // Delete it.
    engine.forget(&id).await.expect("forget() failed");

    // It should no longer be retrievable.
    let fetched = engine.get(&id).await.expect("get() failed");
    assert!(fetched.is_none());

    // Explicitly drop engine before removing the file
    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}
