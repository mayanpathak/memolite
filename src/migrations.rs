

//! SQLite schema management.
//!
//! `run_migrations` is idempotent and versioned: it can be called every
//! time `MemoryEngine::open()` runs, whether against a brand-new file or
//! one that already has data, and it always leaves the schema in the
//! correct, fully-indexed shape.
//!
//! Step 0 only ever applies migration 1 (the baseline schema). Nothing in
//! this file references `src/confidence.rs` or any other module that
//! doesn't exist yet. When M6 introduces the `confidence` column, it adds
//! a *second* call at the bottom of this same function -- it does not
//! rewrite this file's migration-1 logic.
//!
//! This is also where a real, previously-silent bug is fixed: the original
//! schema declared `embeddings.memory_id ... ON DELETE CASCADE`, but
//! nothing ever executed `PRAGMA foreign_keys = ON`. SQLite disables
//! foreign-key enforcement by default, so that cascade delete was never
//! actually happening -- deleting a memory left an orphaned embedding row
//! behind. `run_migrations` turns the pragma on for every connection that
//! opens through `MemoryEngine::open()`.

use rusqlite::Connection;

use crate::error::{MemoliteError, Result};

pub fn run_migrations(conn: &mut Connection) -> Result<()> {
    // PRAGMAs are per-connection, not persisted in the database file --
    // this must run on every open, not just the first one.
    conn.execute("PRAGMA foreign_keys = ON", [])?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )",
        [],
    )?;

    let tx = conn.transaction()?;

    let already_applied: bool = {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = 1",
            [],
            |r| r.get(0),
        )?;
        count > 0
    };

    // migration 1: baseline memories/embeddings + indexes. Unchanged from
    // the pre-Step-0 schema, just relocated out of engine.rs and made
    // idempotent/versioned.
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memories (
            id              TEXT PRIMARY KEY,
            content         TEXT NOT NULL,
            type            TEXT NOT NULL CHECK(type IN ('semantic','episodic','procedural','working')),
            importance      REAL NOT NULL DEFAULT 0.5 CHECK(importance BETWEEN 0.0 AND 1.0),
            access_count    INTEGER NOT NULL DEFAULT 0,
            created_at      INTEGER NOT NULL,
            last_accessed   INTEGER NOT NULL,
            expires_at      INTEGER,
            superseded_by   TEXT REFERENCES memories(id),
            metadata        TEXT DEFAULT '{}'
        );

        CREATE TABLE IF NOT EXISTS embeddings (
            memory_id   TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
            vector      BLOB NOT NULL,
            dimension   INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
        CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
        CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(type);
        CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at);
        CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);
        "#,
    )?;

    if !already_applied {
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
            rusqlite::params![chrono::Utc::now().timestamp()],
        )?;
    }

    tx.commit()?;

    // Orphan/FK-drift detection (Phase 1, Step 5). `embeddings.memory_id`
    // has always declared `ON DELETE CASCADE`, but before this file existed
    // nothing ever turned `PRAGMA foreign_keys = ON`, so old databases can
    // still be carrying orphaned `embeddings` rows from before enforcement
    // was ever active. This check runs on every `open()`, not just once,
    // and fails loudly instead of letting a corrupted file silently limp
    // along.
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let violations: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    if !violations.is_empty() {
        return Err(MemoliteError::Corruption(format!(
            "foreign key violations detected in table(s): {}",
            violations.join(", ")
        )));
    }

    // Migration 2 (the `confidence` column) is added in M6, alongside
    // src/confidence.rs, as ONE MORE LINE appended right here:
    //
    //     crate::confidence::repair_confidence_column(conn)?;
    //
    // Do not add that call now -- confidence.rs is still an empty
    // placeholder and this would fail to compile.

    Ok(())
}