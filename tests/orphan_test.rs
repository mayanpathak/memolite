//! Integration test for Phase 1, Step 5's orphan/FK-drift detection:
//! `run_migrations` runs `PRAGMA foreign_key_check` on every `open()` and
//! fails loudly with `Err(Corruption)` if it finds anything, instead of
//! silently indexing (or silently ignoring) a corrupted file.

use memolite::{MemoliteError, MemoryEngine};
use rusqlite::{Connection, params};
use uuid::Uuid;

fn temp_db_path(test_name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "memolite-orphan-test-{test_name}-{}.db",
        Uuid::new_v4()
    ))
}

#[tokio::test]
async fn orphaned_embedding_row_fails_open_with_corruption() {
    let path = temp_db_path("orphan");

    // First open creates the schema (and runs a clean foreign_key_check).
    {
        let engine = MemoryEngine::open(&path)
            .await
            .expect("first open should succeed");
        drop(engine);
    }

    // A raw `rusqlite::Connection` does NOT go through `run_migrations`, so
    // it never gets the `PRAGMA foreign_keys = ON` that `MemoryEngine::open()`
    // applies. Whether that means FK enforcement is off by default is
    // build/platform-dependent (some bundled SQLite builds default it ON),
    // so explicitly turn it off here rather than relying on an assumption --
    // this is the "FKs disabled" step the plan called for, made explicit.
    {
        let raw = Connection::open(&path).expect("raw connection should open");
        raw.execute("PRAGMA foreign_keys = OFF", [])
            .expect("disabling foreign_keys on the raw connection should succeed");
        raw.execute(
            "INSERT INTO embeddings (memory_id, vector, dimension) VALUES (?1, ?2, ?3)",
            params![Uuid::new_v4().to_string(), vec![0u8; 4], 1],
        )
        .expect("inserting an orphaned embeddings row should succeed with FK enforcement off");
        drop(raw);
    }

    let result = MemoryEngine::open(&path).await;
    assert!(
        matches!(result, Err(MemoliteError::Corruption(_))),
        "an orphaned embeddings row must fail open() as Corruption, not be silently skipped"
    );

    std::fs::remove_file(&path).expect("failed to remove temp db file");
}