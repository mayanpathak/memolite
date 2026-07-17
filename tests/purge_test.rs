//! Integration test for `purge_expired()`.
//!
//! Since `MemoryEngine::store()` always computes a *future* `expires_at`
//! from the memory type's TTL, we can't produce an already-expired memory
//! through the public API. Instead we open a second raw `rusqlite`
//! connection to the same database file and insert a row directly with an
//! `expires_at` timestamp in the past, then confirm `purge_expired()`
//! removes it. The database file itself is managed by `TempDb` (Phase 7,
//! Step 45) instead of a manual `std::fs::remove_file` tail call.

mod common;

use chrono::{Duration, Utc};
use common::TempDb;
use memolite::{MemoryEngine, MemoryType};
use rusqlite::{Connection, params};
use uuid::Uuid;

#[tokio::test]
async fn purge_expired_deletes_only_expired_memories() {
    let db = TempDb::new("purge");

    // Opening the engine first creates the `memories` table via its
    // CREATE TABLE IF NOT EXISTS migration.
    let engine = MemoryEngine::open(db.path())
        .await
        .expect("failed to open engine");

    // A normal store() call: this one gets a real future expires_at
    // (Working = 4 hours out), so purge_expired() must NOT delete it.
    let live_id = engine
        .store("this should survive purge", MemoryType::Working, 0.5)
        .await
        .expect("store() failed");

    // Manually insert a second row with expires_at set an hour in the past,
    // bypassing store() so we can control the expiry directly.
    let expired_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let past_expiry = (now - Duration::hours(1)).timestamp();

    {
        let raw_conn = Connection::open(db.path()).expect("failed to open raw connection");
        raw_conn
            .execute(
                r#"
                INSERT INTO memories (
                    id, content, type, importance, access_count,
                    created_at, last_accessed, expires_at, superseded_by, metadata
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                "#,
                params![
                    expired_id,
                    "this should be purged",
                    "episodic",
                    0.3,
                    0i64,
                    now.timestamp(),
                    now.timestamp(),
                    Some(past_expiry),
                    Option::<String>::None,
                    "{}",
                ],
            )
            .expect("manual insert failed");
        // Explicitly drop the raw connection before continuing
        drop(raw_conn);
    }

    // Sanity check: both rows exist before purging.
    assert!(engine.get(&live_id).await.unwrap().is_some());
    assert!(engine.get(&expired_id).await.unwrap().is_some());

    // Purge. Only the manually-inserted expired row should go.
    let deleted_count = engine
        .purge_expired()
        .await
        .expect("purge_expired() failed");
    assert_eq!(deleted_count, 1);

    // The expired one is gone...
    let expired_after = engine.get(&expired_id).await.expect("get() failed");
    assert!(expired_after.is_none());

    // ...but the live one is untouched.
    let live_after = engine.get(&live_id).await.expect("get() failed");
    assert!(live_after.is_some());

    // No manual cleanup needed -- `db` removes the file when it drops.
}