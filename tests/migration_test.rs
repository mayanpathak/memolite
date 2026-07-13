//! Tests for `migrations::run_migrations`.

use rusqlite::Connection;
use uuid::Uuid;

fn temp_db_path(test_name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "memolite-migration-test-{test_name}-{}.db",
        Uuid::new_v4()
    ))
}

#[tokio::test]
async fn fresh_database_gets_full_schema() {
    let path = temp_db_path("fresh");

    let engine = memolite::MemoryEngine::open(&path)
        .await
        .expect("open should succeed");

    let conn = Connection::open(&path).unwrap();

    let table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM sqlite_master
             WHERE type = 'table'
               AND name IN (
                   'memories',
                   'embeddings',
                   'schema_migrations'
               )",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(table_count, 3);

    let migration_version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();

    assert_eq!(migration_version, 1);

    drop(conn);
    drop(engine);

    std::fs::remove_file(&path).expect("failed to remove temp database");
}

#[tokio::test]
async fn reopening_an_existing_database_does_not_duplicate_migrations() {
    let path = temp_db_path("reopen");

    {
        let engine = memolite::MemoryEngine::open(&path)
            .await
            .expect("first open should succeed");

        engine
            .store("first run", memolite::MemoryType::Semantic, 0.5)
            .await
            .expect("store should succeed");
    }

    {
        let engine = memolite::MemoryEngine::open(&path)
            .await
            .expect("second open should succeed");

        engine
            .store("second run", memolite::MemoryType::Semantic, 0.5)
            .await
            .expect("store should succeed");
    }

    let conn = Connection::open(&path).unwrap();

    let migration_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM schema_migrations
             WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(migration_rows, 1);

    let memory_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .unwrap();

    assert_eq!(memory_rows, 2);

    drop(conn);

    std::fs::remove_file(&path).expect("failed to remove temp database");
}

#[tokio::test]
async fn expected_indexes_exist() {
    let path = temp_db_path("indexes");

    let engine = memolite::MemoryEngine::open(&path)
        .await
        .expect("open should succeed");

    let conn = Connection::open(&path).unwrap();

    let names: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name
                 FROM sqlite_master
                 WHERE type = 'index'
                   AND name LIKE 'idx_memories_%'",
            )
            .unwrap();

        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };

    for expected in [
        "idx_memories_created_at",
        "idx_memories_last_accessed",
        "idx_memories_type",
        "idx_memories_expires_at",
        "idx_memories_superseded_by",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing index {expected}"
        );
    }

    drop(conn);
    drop(engine);

    std::fs::remove_file(&path).expect("failed to remove temp database");
}

#[tokio::test]
async fn forget_cascades_to_the_embeddings_row_because_foreign_keys_are_on() {
    let path = temp_db_path("foreign-key-cascade");

    let engine = memolite::MemoryEngine::open(&path)
        .await
        .expect("open should succeed");

    let id = engine
        .store("will be deleted", memolite::MemoryType::Working, 0.5)
        .await
        .expect("store should succeed");

    engine.forget(&id).await.expect("forget should succeed");

    let conn = Connection::open(&path).unwrap();

    let embedding_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM embeddings
             WHERE memory_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(embedding_rows, 0);

    drop(conn);
    drop(engine);

    std::fs::remove_file(&path).expect("failed to remove temp database");
}
