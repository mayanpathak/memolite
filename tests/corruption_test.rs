//! Restart integration tests for `MemoryEngine::open()`'s reconciliation
//! step (Phase 2, Steps 48/56): after a close-and-reopen cycle, active
//! memories must still be recallable, and *expired* memories must not be
//! re-indexed into the live vector store, even though their SQLite rows
//! (deliberately, in this test) haven't been purged yet.
//!
//! `recall()` is used as the externally-visible proxy for "is this id in
//! the live vector index," since the vector store itself isn't part of the
//! public API -- if reconciliation had re-indexed an expired memory, it
//! would still show up here.
//!
//! Both database files are managed by `TempDb` (Phase 7, Step 47) instead
//! of a manual `std::fs::remove_file` tail call.

mod common;

use chrono::{Duration, Utc};
use common::TempDb;
use memolite::{MemoryEngine, MemoryType};
use rusqlite::{Connection, params};

#[tokio::test]
async fn recall_works_immediately_after_reopening() {
    let db = TempDb::new("recall-after-reopen");

    {
        let engine = MemoryEngine::open(db.path())
            .await
            .expect("first open should succeed");
        engine
            .store("user prefers dark mode", MemoryType::Semantic, 0.8)
            .await
            .expect("store should succeed");
        // engine (and its in-RAM vector index) dropped here
    }

    let engine = MemoryEngine::open(db.path())
        .await
        .expect("second open should succeed");

    let results = engine
        .recall("what theme does the user like?")
        .await
        .expect("recall should succeed after reopening");

    assert!(
        results.iter().any(|m| m.content.contains("dark mode")),
        "a memory stored before restart must still be recallable after reopening"
    );

    // No manual cleanup needed -- `db` removes the file when it drops.
}

#[tokio::test]
async fn reconciliation_does_not_reindex_an_already_expired_memory() {
    let db = TempDb::new("no-reindex-expired");

    let expired_id = {
        let engine = MemoryEngine::open(db.path())
            .await
            .expect("first open should succeed");
        let id = engine
            .store(
                "this memory will be force-expired before restart",
                MemoryType::Working,
                0.4,
            )
            .await
            .expect("store should succeed");

        // Backdate it directly -- store() always computes a future
        // expires_at, so the public API alone can't produce this state.
        let raw = Connection::open(db.path()).expect("raw connection should open");
        let past = (Utc::now() - Duration::hours(1)).timestamp();
        raw.execute(
            "UPDATE memories SET expires_at = ?1 WHERE id = ?2",
            params![past, id],
        )
        .expect("backdating expires_at should succeed");

        id
        // engine dropped here
    };

    let engine = MemoryEngine::open(db.path())
        .await
        .expect("second open should succeed");

    // The SQLite row for the expired memory is still there (nothing has
    // purged it) -- but reconcile_vector_index's active-only filter must
    // have skipped it when repopulating the live vector index.
    assert!(
        engine
            .get(&expired_id)
            .await
            .expect("get should succeed")
            .is_some()
    );

    let results = engine
        .recall("force-expired")
        .await
        .expect("recall should succeed after reopening");
    assert!(
        results.iter().all(|m| m.id.to_string() != expired_id),
        "an expired memory must not be re-indexed into the vector store on reopen"
    );

    // No manual cleanup needed -- `db` removes the file when it drops.
}