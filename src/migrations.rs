use rusqlite::Connection;

use crate::error::{MemoliteError, Result};

pub fn run_migrations(conn: &mut Connection) -> Result<()> {
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

    run_confidence_migration(conn)?;

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

    Ok(())
}

fn run_confidence_migration(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;

    let has_confidence_column: bool = {
        let mut stmt = tx.prepare("PRAGMA table_info(memories)")?;
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        columns.iter().any(|c| c == "confidence")
    };

    if !has_confidence_column {
        tx.execute_batch(
            "ALTER TABLE memories ADD COLUMN confidence TEXT NOT NULL DEFAULT 'explicit' \
                CHECK(confidence IN ('explicit', 'inferred', 'reinforced'));",
        )?;
    }

    let already_applied: bool = {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = 2",
            [],
            |r| r.get(0),
        )?;
        count > 0
    };

    if !already_applied {
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (2, ?1)",
            rusqlite::params![chrono::Utc::now().timestamp()],
        )?;
    }

    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn applied_versions(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn fresh_database_gets_confidence_column_and_both_migration_records() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();

        assert!(column_names(&conn, "memories")
            .iter()
            .any(|c| c == "confidence"));
        assert_eq!(applied_versions(&conn), vec![1, 2]);
    }

    #[test]
    fn running_migrations_twice_is_a_harmless_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        run_migrations(&mut conn).unwrap();

        assert_eq!(applied_versions(&conn), vec![1, 2]);
    }

    #[test]
    fn a_database_with_only_migration_1_gets_the_confidence_column_added() {
        let mut conn = Connection::open_in_memory().unwrap();

        conn.execute(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL, type TEXT NOT NULL,
                importance REAL NOT NULL DEFAULT 0.5, access_count INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL, last_accessed INTEGER NOT NULL,
                expires_at INTEGER, superseded_by TEXT, metadata TEXT DEFAULT '{}'
            );
            CREATE TABLE embeddings (
                memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
                vector BLOB NOT NULL, dimension INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (1, 0)",
            [],
        )
        .unwrap();

        assert!(!column_names(&conn, "memories")
            .iter()
            .any(|c| c == "confidence"));

        run_migrations(&mut conn).unwrap();

        assert!(column_names(&conn, "memories")
            .iter()
            .any(|c| c == "confidence"));
        assert_eq!(applied_versions(&conn), vec![1, 2]);
    }

    #[test]
    fn existing_rows_default_to_explicit_confidence() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();

        conn.execute(
            "INSERT INTO memories (id, content, type, importance, access_count, created_at, last_accessed, expires_at, superseded_by, metadata)
             VALUES ('00000000-0000-0000-0000-000000000000', 'x', 'semantic', 0.5, 0, 0, 0, NULL, NULL, '{}')",
            [],
        )
        .unwrap();

        let confidence: String = conn
            .query_row(
                "SELECT confidence FROM memories WHERE id = '00000000-0000-0000-0000-000000000000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(confidence, "explicit");
    }
}