//! Black-box integration tests for Milestone 6 (confidence scoring),
//! exercised entirely through `memolite`'s public API.

use memolite::{ConfidenceLevel, MemoryEngine, MemoryType, MemoryUpdate, RecallQuery, StoreRequest};

#[tokio::test]
async fn stores_and_retrieves_each_confidence_level() {
    let engine = MemoryEngine::open(":memory:").await.unwrap();

    for level in [
        ConfidenceLevel::Explicit,
        ConfidenceLevel::Inferred,
        ConfidenceLevel::Reinforced,
    ] {
        let request = StoreRequest::new("a fact", MemoryType::Semantic, 0.5).with_confidence(level);
        let id = engine.store_with_options(request).await.unwrap();
        let memory = engine.get(&id).await.unwrap().unwrap();
        assert_eq!(memory.confidence, level);
    }
}

#[tokio::test]
async fn plain_store_defaults_to_explicit() {
    let engine = MemoryEngine::open(":memory:").await.unwrap();
    let id = engine
        .store("plain fact via the simple API", MemoryType::Semantic, 0.5)
        .await
        .unwrap();
    let memory = engine.get(&id).await.unwrap().unwrap();
    assert_eq!(memory.confidence, ConfidenceLevel::Explicit);
}

#[tokio::test]
async fn explicit_outranks_otherwise_identical_inferred_memory() {
    let engine = MemoryEngine::open(":memory:").await.unwrap();

    let explicit_id = engine
        .store_with_options(
            StoreRequest::new("the user prefers dark mode", MemoryType::Semantic, 0.8)
                .with_confidence(ConfidenceLevel::Explicit),
        )
        .await
        .unwrap();
    let inferred_id = engine
        .store_with_options(
            StoreRequest::new("the user prefers dark theme", MemoryType::Semantic, 0.8)
                .with_confidence(ConfidenceLevel::Inferred),
        )
        .await
        .unwrap();

    let result = engine
        .recall_query(RecallQuery::new("dark mode preference").limit(2))
        .await
        .unwrap();

    assert_eq!(result.items.len(), 2);
    let explicit_score = result
        .items
        .iter()
        .find(|i| i.memory.id.to_string() == explicit_id)
        .unwrap()
        .score;
    let inferred_score = result
        .items
        .iter()
        .find(|i| i.memory.id.to_string() == inferred_id)
        .unwrap()
        .score;
    assert!(
        explicit_score > inferred_score,
        "explicit ({explicit_score}) should outrank inferred ({inferred_score})"
    );
}

#[tokio::test]
async fn inferred_memory_promotes_to_reinforced_after_five_recalls() {
    let engine = MemoryEngine::open(":memory:").await.unwrap();

    let id = engine
        .store_with_options(
            StoreRequest::new("the user likes tabs over spaces", MemoryType::Semantic, 0.6)
                .with_confidence(ConfidenceLevel::Inferred),
        )
        .await
        .unwrap();

    for i in 0..5u32 {
        let result = engine
            .recall_query(RecallQuery::new("tabs over spaces").limit(1))
            .await
            .unwrap();
        assert_eq!(result.items.len(), 1);
        let memory = &result.items[0].memory;
        assert_eq!(memory.id.to_string(), id);

        if i < 4 {
            assert_eq!(
                memory.confidence,
                ConfidenceLevel::Inferred,
                "should still be inferred after {} recall(s)",
                i + 1
            );
        } else {
            assert_eq!(
                memory.confidence,
                ConfidenceLevel::Reinforced,
                "should be reinforced after 5 recalls"
            );
        }
    }

    // Further recalls keep it reinforced rather than demoting it.
    let result = engine
        .recall_query(RecallQuery::new("tabs over spaces").limit(1))
        .await
        .unwrap();
    assert_eq!(result.items[0].memory.confidence, ConfidenceLevel::Reinforced);
}

#[tokio::test]
async fn update_without_explicit_confidence_downgrades_to_inferred() {
    let engine = MemoryEngine::open(":memory:").await.unwrap();

    let id = engine
        .store_with_options(
            StoreRequest::new("the user uses VS Code", MemoryType::Semantic, 0.7)
                .with_confidence(ConfidenceLevel::Explicit),
        )
        .await
        .unwrap();

    let new_id = engine
        .update(
            &id,
            MemoryUpdate {
                new_content: Some("the user uses Zed".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let updated = engine.get(&new_id).await.unwrap().unwrap();
    assert_eq!(updated.confidence, ConfidenceLevel::Inferred);
}

#[tokio::test]
async fn update_can_explicitly_set_confidence() {
    let engine = MemoryEngine::open(":memory:").await.unwrap();

    let id = engine
        .store_with_options(StoreRequest::new("fact", MemoryType::Semantic, 0.5))
        .await
        .unwrap();

    let new_id = engine
        .update(
            &id,
            MemoryUpdate {
                new_content: Some("updated fact".to_string()),
                new_confidence: Some(ConfidenceLevel::Reinforced),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let updated = engine.get(&new_id).await.unwrap().unwrap();
    assert_eq!(updated.confidence, ConfidenceLevel::Reinforced);
}

/// Exercises the actual migration runner against a real on-disk file (not
/// `:memory:`), across two separate `MemoryEngine::open()` calls, to prove
/// migration 2 (the confidence column) is genuinely idempotent and that
/// previously-stored data survives it intact.
#[tokio::test]
async fn migration_adds_confidence_column_idempotently_to_a_real_file() {
    let mut path = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!(
        "memolite_m6_migration_test_{}_{}.db",
        std::process::id(),
        nanos
    ));

    {
        let engine = MemoryEngine::open(&path).await.unwrap();
        engine
            .store_with_options(StoreRequest::new("fact one", MemoryType::Semantic, 0.5))
            .await
            .unwrap();
    } // engine dropped here, SQLite connection closed

    // Reopen the same file: migrations must re-run without error, and the
    // previously stored memory must still be readable with its default
    // ('explicit') confidence intact.
    {
        let engine = MemoryEngine::open(&path).await.unwrap();
        let results = engine.recall("fact one").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].confidence, ConfidenceLevel::Explicit);
    }

    let _ = std::fs::remove_file(&path);
}