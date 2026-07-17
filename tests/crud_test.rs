//! CRUD integration tests for `MemoryEngine`.
//!
//! These tests only touch the public API (`open`, `store`, `get`, `forget`)
//! exactly the way a real user of the crate would rather than reaching into
//! internals. Each test opens its own throwaway SQLite file so tests don't
//! interfere with each other when run in parallel.

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

    // Explicitly drop engine before removing the file.
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

    // Explicitly drop engine before removing the file.
    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

/// Test 3: store() rejects empty/whitespace-only content.
#[tokio::test]
async fn store_rejects_empty_content_via_public_api() {
    let path = temp_db_path("empty-content");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("failed to open engine");

    let result = engine
        .store("   ", MemoryType::Working, 0.5)
        .await;

    assert!(matches!(
        result,
        Err(memolite::MemoliteError::InvalidArgument(_))
    ));

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

/// Test 4: store() rejects importance outside [0.0, 1.0].
#[tokio::test]
async fn store_rejects_importance_outside_zero_to_one() {
    let path = temp_db_path("bad-importance");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("failed to open engine");

    let too_high = engine
        .store("valid content", MemoryType::Working, 1.5)
        .await;

    assert!(matches!(
        too_high,
        Err(memolite::MemoliteError::InvalidArgument(_))
    ));

    let too_low = engine
        .store("valid content", MemoryType::Working, -0.1)
        .await;

    assert!(matches!(
        too_low,
        Err(memolite::MemoliteError::InvalidArgument(_))
    ));

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

/// Test 5: each `MemoryType` gets its documented default TTL.
#[tokio::test]
async fn stored_memory_gets_the_correct_ttl_for_its_type() {
    let path = temp_db_path("ttl");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("failed to open engine");

    for (memory_type, expected_days) in [
        (MemoryType::Episodic, 30),
        (MemoryType::Semantic, 365),
        (MemoryType::Procedural, 730),
    ] {
        let id = engine
            .store("ttl check", memory_type, 0.5)
            .await
            .expect("store() failed");

        let memory = engine
            .get(&id)
            .await
            .expect("get() failed")
            .expect("memory should exist");

        let expires_at = memory
            .expires_at
            .expect("this memory type always sets expires_at");

        assert_eq!(
            (expires_at - memory.created_at).num_days(),
            expected_days
        );
    }

    // Working memories use an hours-based TTL.
    let id = engine
        .store("working ttl check", MemoryType::Working, 0.5)
        .await
        .expect("store() failed");

    let memory = engine
        .get(&id)
        .await
        .expect("get() failed")
        .expect("memory should exist");

    let expires_at = memory
        .expires_at
        .expect("Working memories always set expires_at");

    assert_eq!(
        (expires_at - memory.created_at).num_hours(),
        4
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}