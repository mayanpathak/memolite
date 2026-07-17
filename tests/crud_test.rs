//! CRUD integration tests for `MemoryEngine`.
//!
//! These tests exercise only the public API (`open`, `store`, `get`,
//! `forget`) exactly as downstream users would.
//!
//! Every test creates its own temporary SQLite database through `TempDb`,
//! making the suite safe to run in parallel while ensuring automatic cleanup
//! even if a test panics.

mod common;

use common::TempDb;
use memolite::{MemoryEngine, MemoryType};

/// Test 1: store a memory, then retrieve it and verify every important field.
#[tokio::test]
async fn store_then_get_returns_matching_memory() {
    let db = TempDb::new("store-get");

    let engine = MemoryEngine::open(db.path())
        .await
        .expect("failed to open engine");

    // Store a memory.
    let id = engine
        .store(
            "Rust is my favorite language",
            MemoryType::Semantic,
            0.9,
        )
        .await
        .expect("store() failed");

    // Retrieve it.
    let fetched = engine
        .get(&id)
        .await
        .expect("get() failed")
        .expect("expected stored memory");

    assert_eq!(fetched.content, "Rust is my favorite language");
    assert_eq!(fetched.memory_type, MemoryType::Semantic);
    assert_eq!(fetched.importance, 0.9);
    assert_eq!(fetched.id.to_string(), id);

    // TempDb automatically removes the database file.
}

/// Test 2: forget() removes an existing memory.
#[tokio::test]
async fn forget_removes_the_memory() {
    let db = TempDb::new("forget");

    let engine = MemoryEngine::open(db.path())
        .await
        .expect("failed to open engine");

    let id = engine
        .store(
            "temporary note",
            MemoryType::Working,
            0.2,
        )
        .await
        .expect("store() failed");

    assert!(
        engine
            .get(&id)
            .await
            .expect("get() failed")
            .is_some()
    );

    engine
        .forget(&id)
        .await
        .expect("forget() failed");

    let fetched = engine
        .get(&id)
        .await
        .expect("get() failed");

    assert!(fetched.is_none());
}

/// Test 3: store() rejects empty or whitespace-only content.
#[tokio::test]
async fn store_rejects_empty_content_via_public_api() {
    let db = TempDb::new("empty-content");

    let engine = MemoryEngine::open(db.path())
        .await
        .expect("failed to open engine");

    let result = engine
        .store(
            "   ",
            MemoryType::Working,
            0.5,
        )
        .await;

    assert!(matches!(
        result,
        Err(memolite::MemoliteError::InvalidArgument(_))
    ));
}

/// Test 4: store() rejects importance outside the valid [0.0, 1.0] range.
#[tokio::test]
async fn store_rejects_importance_outside_zero_to_one() {
    let db = TempDb::new("bad-importance");

    let engine = MemoryEngine::open(db.path())
        .await
        .expect("failed to open engine");

    let too_high = engine
        .store(
            "valid content",
            MemoryType::Working,
            1.5,
        )
        .await;

    assert!(matches!(
        too_high,
        Err(memolite::MemoliteError::InvalidArgument(_))
    ));

    let too_low = engine
        .store(
            "valid content",
            MemoryType::Working,
            -0.1,
        )
        .await;

    assert!(matches!(
        too_low,
        Err(memolite::MemoliteError::InvalidArgument(_))
    ));
}

/// Test 5: every MemoryType receives its documented default TTL.
#[tokio::test]
async fn stored_memory_gets_the_correct_ttl_for_its_type() {
    let db = TempDb::new("ttl");

    let engine = MemoryEngine::open(db.path())
        .await
        .expect("failed to open engine");

    for (memory_type, expected_days) in [
        (MemoryType::Episodic, 30),
        (MemoryType::Semantic, 365),
        (MemoryType::Procedural, 730),
    ] {
        let id = engine
            .store(
                "ttl check",
                memory_type,
                0.5,
            )
            .await
            .expect("store() failed");

        let memory = engine
            .get(&id)
            .await
            .expect("get() failed")
            .expect("memory should exist");

        let expires_at = memory
            .expires_at
            .expect("memory type should have an expiration");

        assert_eq!(
            (expires_at - memory.created_at).num_days(),
            expected_days
        );
    }

    // Working memories use an hours-based TTL.
    let id = engine
        .store(
            "working ttl check",
            MemoryType::Working,
            0.5,
        )
        .await
        .expect("store() failed");

    let memory = engine
        .get(&id)
        .await
        .expect("get() failed")
        .expect("memory should exist");

    let expires_at = memory
        .expires_at
        .expect("Working memories should have an expiration");

    assert_eq!(
        (expires_at - memory.created_at).num_hours(),
        4
    );
}