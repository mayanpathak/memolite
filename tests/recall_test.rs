//! Integration tests for `MemoryEngine::recall()` (M3's temporary but real
//! cosine-based semantic recall) -- Phase 6, Steps 25-38 of the M3 file
//! edit sequence.
//!
//! These only touch the public API (`MemoryEngine`, `InMemoryVectorStore`)
//! plus, where a test needs to manufacture a state `store()` itself can
//! never produce directly (an already-expired row, an already-superseded
//! row, a stale vector-store hit with no matching SQLite row), a second raw
//! `rusqlite::Connection` to the same database file -- the same pattern
//! already used by `tests/purge_test.rs` and `tests/forget_test.rs`.
//!
//! Step map:
//!   25 -> recall_on_empty_engine_returns_empty
//!   26 -> whitespace_only_query_is_rejected
//!   27 -> relevant_memory_ranks_above_unrelated_memories
//!   28 -> recall_never_exceeds_default_recall_limit
//!   29 -> restart_reconciliation_excludes_expired_noise_so_valid_memory_surfaces  (Test A)
//!   30 -> same_session_starvation_from_freshly_expired_vectors_is_a_known_limitation  (Test B, #[ignore])
//!   31 -> stale_vector_hit_with_no_sqlite_row_is_silently_excluded
//!   32 -> expired_memories_are_excluded_from_recall
//!   33 -> superseded_memories_are_excluded_from_recall
//!   34 -> expired_candidate_gets_no_access_count_bump / superseded_candidate_gets_no_access_count_bump
//!   35 -> recall_increments_access_count_and_reflects_it_in_the_return_value
//!   36 -> recall_updates_last_accessed_forward
//!   37 -> vector_store_search_failure_leaves_access_stats_untouched  (#[ignore], see doc comment)
//!   38 -> recalls_underlying_vector_search_handles_large_finite_vectors_without_overflow

use chrono::{Duration, Utc};
use memolite::recall::DEFAULT_RECALL_LIMIT;
use memolite::{InMemoryVectorStore, MemoliteError, MemoryEngine, MemoryType, VectorStore};
use rusqlite::{Connection, params};
use uuid::Uuid;

fn temp_db_path(test_name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "memolite-recall-test-{test_name}-{}.db",
        Uuid::new_v4()
    ))
}

/// Inserts an already-expired "noise" row directly via raw SQL, bypassing
/// `store()` entirely. No matching `embeddings` row is written -- and none
/// is needed, since `reconcile_vector_index()`'s `WHERE` clause excludes
/// non-active rows before their (absent) embedding is ever inspected.
fn insert_expired_noise_row(conn: &Connection, content: &str, now_ts: i64, past_ts: i64) {
    let id = Uuid::new_v4().to_string();
    conn.execute(
        r#"
        INSERT INTO memories (
            id, content, type, importance, access_count,
            created_at, last_accessed, expires_at, superseded_by, metadata
        )
        VALUES (?1, ?2, 'episodic', 0.3, 0, ?3, ?3, ?4, NULL, '{}')
        "#,
        params![id, content, now_ts, past_ts],
    )
    .expect("manual noise insert should succeed");
}

// ---------------------------------------------------------------------
// Step 25 -- empty engine
// ---------------------------------------------------------------------

#[tokio::test]
async fn recall_on_empty_engine_returns_empty() {
    let engine = MemoryEngine::open(":memory:")
        .await
        .expect("engine should open");

    let results = engine
        .recall("anything at all")
        .await
        .expect("recall should succeed on an empty engine");
    assert!(results.is_empty());
}

// ---------------------------------------------------------------------
// Step 26 -- whitespace-only query rejected
// ---------------------------------------------------------------------

#[tokio::test]
async fn whitespace_only_query_is_rejected() {
    let engine = MemoryEngine::open(":memory:")
        .await
        .expect("engine should open");

    let result = engine.recall("   ").await;
    assert!(matches!(result, Err(MemoliteError::InvalidArgument(_))));
}

// ---------------------------------------------------------------------
// Step 27 -- semantic ordering (real embedder, non-strict)
// ---------------------------------------------------------------------

#[tokio::test]
async fn relevant_memory_ranks_above_unrelated_memories() {
    let engine = MemoryEngine::open(":memory:")
        .await
        .expect("engine should open");

    engine
        .store(
            "Rust ownership prevents data races.",
            MemoryType::Semantic,
            0.7,
        )
        .await
        .expect("store should succeed");
    engine
        .store("Bananas are yellow tropical fruit.", MemoryType::Semantic, 0.7)
        .await
        .expect("store should succeed");
    engine
        .store(
            "The Pacific Ocean is the largest ocean.",
            MemoryType::Semantic,
            0.7,
        )
        .await
        .expect("store should succeed");
    let rust_id = engine
        .store(
            "My Rust program has a borrow checker error.",
            MemoryType::Semantic,
            0.7,
        )
        .await
        .expect("store should succeed");

    let results = engine
        .recall("How does Rust borrowing work?")
        .await
        .expect("recall should succeed");

    assert!(!results.is_empty());
    // Don't assert exact float similarity ordering -- just that a
    // Rust-related memory is the top hit and the specific borrow-checker
    // memory is present somewhere in the result set.
    assert!(
        results[0].content.contains("Rust"),
        "expected a Rust-related memory to rank first, got: {}",
        results[0].content
    );
    assert!(results.iter().any(|m| m.id.to_string() == rust_id));
}

// ---------------------------------------------------------------------
// Step 28 -- never exceeds DEFAULT_RECALL_LIMIT
// ---------------------------------------------------------------------

#[tokio::test]
async fn recall_never_exceeds_default_recall_limit() {
    let engine = MemoryEngine::open(":memory:")
        .await
        .expect("engine should open");

    // 15 near-duplicate, mutually relevant memories -- comfortably more
    // than DEFAULT_RECALL_LIMIT (10), and well inside candidate_pool_size
    // (50), so every one of them is a genuine candidate.
    for i in 0..15 {
        engine
            .store(
                &format!("the user's favorite programming language is Rust, note {i}"),
                MemoryType::Semantic,
                0.6,
            )
            .await
            .expect("store should succeed");
    }

    let results = engine
        .recall("what programming language does the user like?")
        .await
        .expect("recall should succeed");

    assert!(
        results.len() <= DEFAULT_RECALL_LIMIT,
        "recall() must never return more than DEFAULT_RECALL_LIMIT results, got {}",
        results.len()
    );
    assert_eq!(
        results.len(),
        DEFAULT_RECALL_LIMIT,
        "with 15 eligible near-identical matches, recall() should return exactly the limit"
    );
}

// ---------------------------------------------------------------------
// Step 29 -- restart starvation, Test A (deterministic, required)
// ---------------------------------------------------------------------

#[tokio::test]
async fn restart_reconciliation_excludes_expired_noise_so_valid_memory_surfaces() {
    let path = temp_db_path("restart-starvation");

    let valid_id = {
        let engine = MemoryEngine::open(&path)
            .await
            .expect("first open should succeed");
        let valid_id = engine
            .store(
                "the user's preferred database is memolite",
                MemoryType::Semantic,
                0.8,
            )
            .await
            .expect("store should succeed");

        // Flood SQLite with 55 already-expired rows -- more than
        // candidate_pool_size(DEFAULT_RECALL_LIMIT) (50). These are
        // inserted directly (not via store()), so this test isolates
        // exactly one thing: reconcile_vector_index()'s active-only
        // filter (Step 48) at open() time. Whether or not a row was ever
        // in the live index before is irrelevant to that guarantee.
        {
            let raw_conn = Connection::open(&path).expect("raw connection should open");
            let now_ts = Utc::now().timestamp();
            let past_ts = (Utc::now() - Duration::hours(1)).timestamp();
            for i in 0..55 {
                insert_expired_noise_row(
                    &raw_conn,
                    &format!("expired noise memory number {i}"),
                    now_ts,
                    past_ts,
                );
            }
        }

        valid_id
        // engine dropped here, closing the file
    };

    let engine = MemoryEngine::open(&path)
        .await
        .expect("second open should succeed");

    let results = engine
        .recall("what database does the user prefer?")
        .await
        .expect("recall should succeed after reopening");

    assert!(
        results.iter().any(|m| m.id.to_string() == valid_id),
        "a still-active memory must surface after restart even with 55 expired rows alongside it"
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

// ---------------------------------------------------------------------
// Step 30 -- same-session starvation, Test B (#[ignore], documented limitation)
// ---------------------------------------------------------------------

#[tokio::test]
#[ignore = "documents a known M3 limitation: within a single session, a \
            memory that expires (or is superseded) is never evicted from \
            the *live* vector index until the next restart or \
            purge_expired() call, so it can still occupy a candidate-pool \
            slot and crowd out a still-valid memory. This is NOT asserted \
            as a guaranteed-passing test -- see the 'Known Limitations' \
            section of the M3 plan / ARCHITECTURE.md. Run explicitly with \
            `cargo test -- --ignored` if you want to observe it."]
async fn same_session_starvation_from_freshly_expired_vectors_is_a_known_limitation() {
    let path = temp_db_path("same-session-starvation");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("engine should open");

    // 55 near-duplicate noise memories, stored through the real API so
    // they land in the *live* vector index this session (candidate_pool_size
    // for DEFAULT_RECALL_LIMIT is 50, so 55 is enough to fill every slot).
    let mut noise_ids = Vec::new();
    for i in 0..55 {
        let id = engine
            .store(
                &format!("the user's favorite database is memolite, duplicate {i}"),
                MemoryType::Semantic,
                0.6,
            )
            .await
            .expect("store should succeed");
        noise_ids.push(id);
    }

    let valid_id = engine
        .store(
            "the user's favorite database is memolite",
            MemoryType::Semantic,
            0.9,
        )
        .await
        .expect("store should succeed");

    // Backdate every noise memory's expires_at to the past. This makes them
    // ineligible in SQLite, but does NOT remove them from the live vector
    // index -- only forget(), purge_expired(), or a restart do that -- so
    // they still occupy candidate-pool slots for the rest of this session.
    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        let past_ts = (Utc::now() - Duration::hours(1)).timestamp();
        for id in &noise_ids {
            raw_conn
                .execute(
                    "UPDATE memories SET expires_at = ?1 WHERE id = ?2",
                    params![past_ts, id],
                )
                .expect("backdating expires_at should succeed");
        }
    }

    let results = engine
        .recall("what database does the user prefer?")
        .await
        .expect("recall should succeed");

    // This is the limitation itself: even though every noise memory is now
    // expired in SQLite, none of them were evicted from the live vector
    // index this session, so they can still occupy every one of the
    // candidate-pool slots search() returns -- crowding the still-valid
    // memory out of the results entirely.
    assert!(
        results.iter().all(|m| m.id.to_string() != valid_id),
        "documents same-session candidate starvation -- if this assertion \
         ever fails, the limitation may have been fixed and this test (and \
         its #[ignore]) should be revisited"
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

// ---------------------------------------------------------------------
// Step 31 -- stale vector hit (no matching SQLite row) silently excluded
// ---------------------------------------------------------------------

#[tokio::test]
async fn stale_vector_hit_with_no_sqlite_row_is_silently_excluded() {
    let path = temp_db_path("stale-hit");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("engine should open");

    let id = engine
        .store(
            "a very particular fact about narwhals",
            MemoryType::Semantic,
            0.7,
        )
        .await
        .expect("store should succeed");

    // Sanity check: findable before we manufacture the stale-hit scenario.
    let before = engine
        .recall("narwhals")
        .await
        .expect("recall should succeed");
    assert!(before.iter().any(|m| m.id.to_string() == id));

    // Delete the SQLite row directly, bypassing forget(). forget() is the
    // only public path that removes an entry from the live vector index, so
    // this leaves that in-memory entry behind -- simulating a vector-store
    // hit with no corresponding SQLite row (e.g. a partially-failed
    // concurrent delete).
    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        raw_conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])
            .expect("manual delete should succeed");
    }

    let after = engine
        .recall("narwhals")
        .await
        .expect("recall must not error on a stale vector-store hit");
    assert!(
        after.iter().all(|m| m.id.to_string() != id),
        "a vector-store hit with no matching SQLite row must be silently dropped, never returned"
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

// ---------------------------------------------------------------------
// Step 32 -- expired memories excluded from recall results
// ---------------------------------------------------------------------

#[tokio::test]
async fn expired_memories_are_excluded_from_recall() {
    let path = temp_db_path("expired-excluded");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("engine should open");

    // store() always computes a future expires_at, so backdate it directly
    // via a raw connection, same pattern as tests/purge_test.rs.
    let id = engine
        .store(
            "this fact will be force-expired before recall",
            MemoryType::Working,
            0.5,
        )
        .await
        .expect("store should succeed");

    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        let past_ts = (Utc::now() - Duration::hours(1)).timestamp();
        raw_conn
            .execute(
                "UPDATE memories SET expires_at = ?1 WHERE id = ?2",
                params![past_ts, id],
            )
            .expect("backdating expires_at should succeed");
    }

    let results = engine
        .recall("this fact will be force-expired before recall")
        .await
        .expect("recall should succeed");

    assert!(
        results.iter().all(|m| m.id.to_string() != id),
        "an expired memory must never be returned by recall()"
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

// ---------------------------------------------------------------------
// Step 33 -- superseded memories excluded from recall results
// ---------------------------------------------------------------------

#[tokio::test]
async fn superseded_memories_are_excluded_from_recall() {
    let path = temp_db_path("superseded-excluded");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("engine should open");

    let old_id = engine
        .store("the user's old preference", MemoryType::Semantic, 0.6)
        .await
        .expect("store should succeed");
    let new_id = engine
        .store("the user's new preference", MemoryType::Semantic, 0.6)
        .await
        .expect("store should succeed");

    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        raw_conn
            .execute(
                "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
                params![new_id, old_id],
            )
            .expect("setting superseded_by should succeed");
    }

    let results = engine
        .recall("the user's preference")
        .await
        .expect("recall should succeed");

    assert!(
        results.iter().all(|m| m.id.to_string() != old_id),
        "a superseded memory must never be returned by recall()"
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

// ---------------------------------------------------------------------
// Step 34 -- expired/superseded candidates get NO access-stat bump
// ---------------------------------------------------------------------

#[tokio::test]
async fn expired_candidate_gets_no_access_count_bump() {
    let path = temp_db_path("expired-no-bump");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("engine should open");

    let id = engine
        .store(
            "a fact that will expire before recall runs",
            MemoryType::Working,
            0.5,
        )
        .await
        .expect("store should succeed");

    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        let past_ts = (Utc::now() - Duration::hours(1)).timestamp();
        raw_conn
            .execute(
                "UPDATE memories SET expires_at = ?1 WHERE id = ?2",
                params![past_ts, id],
            )
            .expect("backdating expires_at should succeed");
    }

    let _ = engine
        .recall("a fact that will expire before recall runs")
        .await
        .expect("recall should succeed");

    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        let access_count: i64 = raw_conn
            .query_row(
                "SELECT access_count FROM memories WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .expect("row should still exist");
        assert_eq!(
            access_count, 0,
            "an expired candidate must never have its access_count bumped by recall()"
        );
        // raw_conn dropped here -- must happen before remove_file, or
        // Windows refuses to delete a file that's still open (error 32).
    }

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

#[tokio::test]
async fn superseded_candidate_gets_no_access_count_bump() {
    let path = temp_db_path("superseded-no-bump");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("engine should open");

    let old_id = engine
        .store(
            "the old preference that gets superseded",
            MemoryType::Semantic,
            0.6,
        )
        .await
        .expect("store should succeed");
    let new_id = engine
        .store(
            "the new preference that replaces it",
            MemoryType::Semantic,
            0.6,
        )
        .await
        .expect("store should succeed");

    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        raw_conn
            .execute(
                "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
                params![new_id, old_id],
            )
            .expect("setting superseded_by should succeed");
    }

    let _ = engine
        .recall("the old preference that gets superseded")
        .await
        .expect("recall should succeed");

    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        let access_count: i64 = raw_conn
            .query_row(
                "SELECT access_count FROM memories WHERE id = ?1",
                params![old_id],
                |r| r.get(0),
            )
            .expect("row should still exist");
        assert_eq!(
            access_count, 0,
            "a superseded candidate must never have its access_count bumped by recall()"
        );
        // raw_conn dropped here -- must happen before remove_file, or
        // Windows refuses to delete a file that's still open (error 32).
    }

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

// ---------------------------------------------------------------------
// Step 35 -- access_count reflects immediately (1 -> 2)
// ---------------------------------------------------------------------

#[tokio::test]
async fn recall_increments_access_count_and_reflects_it_in_the_return_value() {
    let engine = MemoryEngine::open(":memory:")
        .await
        .expect("engine should open");

    let id = engine
        .store("the user prefers dark mode", MemoryType::Semantic, 0.8)
        .await
        .expect("store should succeed");

    let results = engine
        .recall("what interface theme does the user like?")
        .await
        .expect("recall should succeed");

    let found = results
        .iter()
        .find(|m| m.id.to_string() == id)
        .expect("stored memory should be recalled");
    assert_eq!(
        found.access_count, 1,
        "the returned Memory must already reflect the bump this call made, not a pre-increment value"
    );

    let results_again = engine
        .recall("what interface theme does the user like?")
        .await
        .expect("recall should succeed");
    let found_again = results_again
        .iter()
        .find(|m| m.id.to_string() == id)
        .expect("stored memory should still be recalled");
    assert_eq!(found_again.access_count, 2);
}

// ---------------------------------------------------------------------
// Step 36 -- last_accessed updates forward
// ---------------------------------------------------------------------

#[tokio::test]
async fn recall_updates_last_accessed_forward() {
    let path = temp_db_path("last-accessed");
    let engine = MemoryEngine::open(&path)
        .await
        .expect("engine should open");

    let id = engine
        .store("some fact about the user", MemoryType::Semantic, 0.5)
        .await
        .expect("store should succeed");

    // Backdate last_accessed so "did recall() bump it forward" is
    // unambiguous, without relying on a sleep().
    let old_ts = (Utc::now() - Duration::days(1)).timestamp();
    {
        let raw_conn = Connection::open(&path).expect("raw connection should open");
        raw_conn
            .execute(
                "UPDATE memories SET last_accessed = ?1 WHERE id = ?2",
                params![old_ts, id],
            )
            .expect("backdating last_accessed should succeed");
    }

    let results = engine
        .recall("some fact about the user")
        .await
        .expect("recall should succeed");
    let found = results
        .iter()
        .find(|m| m.id.to_string() == id)
        .expect("stored memory should be recalled");

    assert!(
        found.last_accessed.timestamp() > old_ts,
        "recall() must bump last_accessed forward"
    );

    drop(engine);
    std::fs::remove_file(&path).expect("failed to remove temp db file");
}

// ---------------------------------------------------------------------
// Step 37 -- vector-store search failure leaves access stats untouched
// ---------------------------------------------------------------------

#[tokio::test]
#[ignore = "cannot be exercised from this integration-test crate as written: \
            proving that a vector-store search() failure leaves \
            access_count/last_accessed untouched requires constructing a \
            MemoryEngine over a forced-failure VectorStore double (e.g. \
            tests/common::FakeVectorStore), which requires \
            MemoryEngine::open_with_store_internal(). That constructor is \
            deliberately pub(crate) (M3 Phase 1, Step 9) -- an external \
            `tests/` file is a separate crate and cannot call it, and \
            reaching in via `#[path]` is explicitly the wrong tool per the \
            M3 plan. The real coverage for this guarantee belongs in \
            engine.rs's own `#[cfg(test)]` module (alongside \
            AlwaysFailsInsert/AlwaysFailsDelete), the same place \
            store()'s and forget()'s compensation paths are already \
            tested. This stub is kept here, ignored, purely so the gap is \
            visible in `cargo test -- --list` rather than silently \
            missing."]
async fn vector_store_search_failure_leaves_access_stats_untouched() {
    unimplemented!(
        "see the #[ignore] reason above -- implement the real version as an \
         in-crate #[cfg(test)] in src/engine.rs using a local VectorStore \
         double whose search() always fails, mirroring AlwaysFailsInsert / \
         AlwaysFailsDelete already there"
    );
}

// ---------------------------------------------------------------------
// Step 38 -- large-finite vector overflow (cosine hardening), re-checked
// at recall()'s actual dependency boundary
// ---------------------------------------------------------------------

#[tokio::test]
async fn recalls_underlying_vector_search_handles_large_finite_vectors_without_overflow() {
    // recall() delegates similarity search directly to the engine's
    // configured VectorStore -- for M3 that's always InMemoryVectorStore,
    // via exactly this call shape: store.search(&query_vector, pool_size).
    // cosine_test.rs already proves cosine()'s f64 accumulation (Step 8)
    // in isolation; this re-checks the same guarantee at the entry point
    // recall() itself depends on, using DEFAULT_RECALL_LIMIT as the `k`
    // recall() would actually request.
    let store = InMemoryVectorStore::new(2);
    let id = Uuid::new_v4();
    let big = f32::MAX / 2.0;

    store
        .insert(id, &[big, big], Default::default())
        .await
        .expect("inserting a large-but-finite vector must not fail");

    let hits = store
        .search(&[big, big], DEFAULT_RECALL_LIMIT)
        .await
        .expect("search should succeed, the same call recall() itself makes");

    assert!(
        hits[0].score.is_finite(),
        "a large-but-finite vector pair must still produce a finite similarity score for recall() to consume"
    );
    assert!((hits[0].score - 1.0).abs() < 1e-3);
}