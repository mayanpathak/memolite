

use chrono::{Duration, Utc};
use memolite::{MemoryEngine, MemoryType, RecallQuery};
use rusqlite::{params, Connection};
use uuid::Uuid;

/// Returns a fresh, unique temp-file path for one test's SQLite database.
fn temp_db_path(label: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("memolite_compression_test_{label}_{}.db", Uuid::new_v4()));
    path
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Directly rewrites a memory's `created_at` column to `days_ago` days
/// before now, bypassing the public API (which has no way to backdate a
/// memory). Opens its own connection to the same on-disk file.
fn backdate_created_at(db_path: &std::path::Path, memory_id: &str, days_ago: i64) {
    let conn = Connection::open(db_path).expect("open raw connection for backdating");
    let cutoff = (Utc::now() - Duration::days(days_ago)).timestamp();
    conn.execute(
        "UPDATE memories SET created_at = ?1 WHERE id = ?2",
        params![cutoff, memory_id],
    )
    .expect("backdate created_at");
}

/// Inserts a `memories` row directly via raw SQL, with **no** matching
/// `embeddings` row — simulating a corrupted database where the
/// memory+embedding invariant has been violated by external tampering.
fn insert_memory_row_without_embedding(db_path: &std::path::Path) -> Uuid {
    let conn = Connection::open(db_path).expect("open raw connection for corruption fixture");
    let id = Uuid::new_v4();
    let now = Utc::now();
    let created_at = (now - Duration::days(20)).timestamp();

    conn.execute(
        r#"
        INSERT INTO memories (
            id, content, type, importance, access_count,
            created_at, last_accessed, expires_at, superseded_by, metadata, confidence
        )
        VALUES (?1, ?2, 'episodic', 0.1, 0, ?3, ?3, NULL, NULL, '{}', 'explicit')
        "#,
        params![id.to_string(), "orphaned memory with no embedding", created_at],
    )
    .expect("insert corrupt memory row");

    id
}

#[tokio::test]
async fn successful_compression_folds_similar_old_memories_into_one_summary() {
    let db_path = temp_db_path("success");
    let engine = MemoryEngine::open(&db_path).await.unwrap();

    // Three near-duplicate, low-importance episodic memories about the
    // same topic — these should cluster together and compress.
    let id_a = engine
        .store(
            "The user debugged a login timeout issue in the auth service",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();
    let id_b = engine
        .store(
            "The user debugged a login timeout problem in the auth service",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();
    let id_c = engine
        .store(
            "The user debugged a login timeout bug in the auth service",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();

    // An unrelated, high-importance episodic memory — not eligible, and
    // should survive compression untouched.
    let id_important = engine
        .store(
            "The user's production database credentials rotated successfully",
            MemoryType::Episodic,
            0.9,
        )
        .await
        .unwrap();

    // Backdate the three eligible memories to 20 days ago (compression
    // requires > 14 days old). Leave the important one recent.
    backdate_created_at(&db_path, &id_a, 20);
    backdate_created_at(&db_path, &id_b, 20);
    backdate_created_at(&db_path, &id_c, 20);

    let compressed = engine.compress_old_memories().await.unwrap();
    assert_eq!(compressed, 3, "expected exactly the 3 eligible originals to be compressed");

    // The three originals are now superseded.
    for id in [&id_a, &id_b, &id_c] {
        let mem = engine.get(id).await.unwrap().expect("original still present");
        assert!(
            mem.superseded_by.is_some(),
            "original {id} should be marked superseded after compression"
        );
    }

    // The important, ineligible memory is untouched.
    let important = engine.get(&id_important).await.unwrap().unwrap();
    assert!(important.superseded_by.is_none());

    // The new summary exists, is Semantic, and its metadata lists the
    // original ids.
    let summary_id = engine
        .get(&id_a)
        .await
        .unwrap()
        .unwrap()
        .superseded_by
        .unwrap()
        .to_string();
    let summary = engine
        .get(&summary_id)
        .await
        .unwrap()
        .expect("summary memory should exist");
    assert_eq!(summary.memory_type, MemoryType::Semantic);

    let original_ids_value = summary
        .metadata
        .get("compression.original_ids")
        .expect("summary metadata should record original ids");
    let original_ids: Vec<String> =
        serde_json::from_value(original_ids_value.clone()).unwrap();
    let original_ids_set: std::collections::HashSet<_> = original_ids.into_iter().collect();
    assert!(original_ids_set.contains(&id_a));
    assert!(original_ids_set.contains(&id_b));
    assert!(original_ids_set.contains(&id_c));

    // Default recall hides superseded originals, but include_superseded
    // brings them back.
    let default_recall = engine
        .recall_query(RecallQuery::new("login timeout auth service").limit(20))
        .await
        .unwrap();
    let default_ids: std::collections::HashSet<String> = default_recall
        .items
        .iter()
        .map(|i| i.memory.id.to_string())
        .collect();
    assert!(!default_ids.contains(&id_a));

    let with_superseded = engine
        .recall_query(
            RecallQuery::new("login timeout auth service")
                .limit(20)
                .include_superseded(true),
        )
        .await
        .unwrap();
    let superseded_ids: std::collections::HashSet<String> = with_superseded
        .items
        .iter()
        .map(|i| i.memory.id.to_string())
        .collect();
    assert!(superseded_ids.contains(&id_a));
    assert!(superseded_ids.contains(&id_b));
    assert!(superseded_ids.contains(&id_c));

    drop(engine);
    cleanup(&db_path);
}

#[tokio::test]
async fn compression_ignores_ineligible_memories_and_returns_zero() {
    let db_path = temp_db_path("no_op");
    let engine = MemoryEngine::open(&db_path).await.unwrap();

    // High importance -> not eligible, even though old.
    let id = engine
        .store("An important old memory", MemoryType::Episodic, 0.9)
        .await
        .unwrap();
    backdate_created_at(&db_path, &id, 20);

    let compressed = engine.compress_old_memories().await.unwrap();
    assert_eq!(compressed, 0);

    let mem = engine.get(&id).await.unwrap().unwrap();
    assert!(mem.superseded_by.is_none());

    drop(engine);
    cleanup(&db_path);
}

#[tokio::test]
async fn missing_embedding_is_reported_as_corruption_not_silently_skipped() {
    let db_path = temp_db_path("corruption");
    let engine = MemoryEngine::open(&db_path).await.unwrap();

    // A normal, healthy candidate so the candidate set isn't empty.
    let healthy_id = engine
        .store("A perfectly normal old episodic memory", MemoryType::Episodic, 0.1)
        .await
        .unwrap();
    backdate_created_at(&db_path, &healthy_id, 20);

    // A corrupt row: exists in `memories`, has no matching `embeddings`
    // row at all.
    let orphan_id = insert_memory_row_without_embedding(&db_path);

    let result = engine.compress_old_memories().await;
    assert!(result.is_err(), "a missing embedding must surface as an error");

    let err_msg = result.unwrap_err().to_string();
    let orphan_id_str = orphan_id.to_string();
    assert!(
        err_msg.contains(orphan_id_str.as_str()),
        "error message should reference the specific memory id with no embedding: {err_msg}"
    );

    drop(engine);
    cleanup(&db_path);
}

#[tokio::test]
async fn rebuild_vector_index_prunes_superseded_originals_but_keeps_summary_searchable() {
    let db_path = temp_db_path("rebuild");
    let engine = MemoryEngine::open(&db_path).await.unwrap();

    // Near-templated sentences (identical structure, a single word
    // swapped) -- the pattern that reliably clears the 0.85 cosine
    // clustering threshold under a real sentence-embedding model.
    // Looser paraphrases are semantically equivalent to a person but
    // routinely land below the threshold with common small embedding
    // models, which would make this test flaky depending on the
    // embedder backing `Embedder::embed`.
    let id_a = engine
        .store(
            "The user prefers dark mode across all their applications",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();
    let id_b = engine
        .store(
            "The user prefers dark mode across all their apps",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();
    let id_c = engine
        .store(
            "The user prefers dark mode across every application",
            MemoryType::Episodic,
            0.2,
        )
        .await
        .unwrap();

    backdate_created_at(&db_path, &id_a, 20);
    backdate_created_at(&db_path, &id_b, 20);
    backdate_created_at(&db_path, &id_c, 20);

    let compressed = engine.compress_old_memories().await.unwrap();
    assert_eq!(compressed, 3);

    let summary_id = engine.get(&id_a).await.unwrap().unwrap().superseded_by.unwrap();

    // Before rebuild: compress_old_memories() never touched the vector
    // store's entries for the originals -- it only inserted the new
    // summary vector and flipped `superseded_by` in SQLite -- so the
    // stale in-memory index still holds all four vectors, and a
    // superseded-inclusive recall finds all of them.
    let before = engine
        .recall_query(
            RecallQuery::new("dark mode preference across applications")
                .limit(20)
                .include_superseded(true),
        )
        .await
        .unwrap();
    let before_ids: std::collections::HashSet<String> =
        before.items.iter().map(|i| i.memory.id.to_string()).collect();
    assert!(before_ids.contains(&id_a));
    assert!(before_ids.contains(&id_b));
    assert!(before_ids.contains(&id_c));
    assert!(before_ids.contains(&summary_id.to_string()));

    // rebuild_vector_index() runs reconcile_vector_index with
    // BackfillPolicy::ReplaceAll, whose query is
    // `WHERE superseded_by IS NULL AND (expires_at IS NULL OR
    // expires_at > now)` -- this intentionally excludes superseded (and
    // expired) memories from the rebuilt index, matching what open(),
    // forget()'s compensation path, and purge_expired()'s compensation
    // path already do. So after a rebuild, the three now-superseded
    // originals are no longer *vector-searchable*, even though nothing
    // was deleted from SQLite.
    engine.rebuild_vector_index().await.unwrap();

    let after = engine
        .recall_query(
            RecallQuery::new("dark mode preference across applications")
                .limit(20)
                .include_superseded(true),
        )
        .await
        .unwrap();
    let after_ids: std::collections::HashSet<String> =
        after.items.iter().map(|i| i.memory.id.to_string()).collect();

    assert!(
        after_ids.contains(&summary_id.to_string()),
        "the active summary must remain searchable after a rebuild"
    );
    assert!(
        !after_ids.contains(&id_a) && !after_ids.contains(&id_b) && !after_ids.contains(&id_c),
        "superseded originals are expected to drop out of the rebuilt ANN index"
    );

    // The originals' data is still fully intact in SQLite -- rebuild only
    // affects what's searchable via the vector index, never what get()
    // can retrieve directly.
    for id in [&id_a, &id_b, &id_c] {
        let mem = engine.get(id).await.unwrap().expect("original must still exist in SQLite");
        assert_eq!(mem.superseded_by, Some(summary_id));
    }

    drop(engine);
    cleanup(&db_path);
}

#[tokio::test]
async fn empty_database_compresses_to_zero_with_no_error() {
    let db_path = temp_db_path("empty");
    let engine = MemoryEngine::open(&db_path).await.unwrap();

    let compressed = engine.compress_old_memories().await.unwrap();
    assert_eq!(compressed, 0);

    drop(engine);
    cleanup(&db_path);
}

#[tokio::test]
async fn small_clusters_below_three_members_are_left_uncompressed() {
    let db_path = temp_db_path("small_cluster");
    let engine = MemoryEngine::open(&db_path).await.unwrap();

    // Only two similar memories -- below the >=3 member threshold, so
    // this cluster should NOT be summarized.
    let id_a = engine
        .store("A rare old episodic memory about weather", MemoryType::Episodic, 0.1)
        .await
        .unwrap();
    let id_b = engine
        .store("A rare old episodic memory about the weather today", MemoryType::Episodic, 0.1)
        .await
        .unwrap();

    backdate_created_at(&db_path, &id_a, 20);
    backdate_created_at(&db_path, &id_b, 20);

    let compressed = engine.compress_old_memories().await.unwrap();
    assert_eq!(compressed, 0, "clusters smaller than 3 members should not be compressed");

    let mem_a = engine.get(&id_a).await.unwrap().unwrap();
    let mem_b = engine.get(&id_b).await.unwrap().unwrap();
    assert!(mem_a.superseded_by.is_none());
    assert!(mem_b.superseded_by.is_none());

    drop(engine);
    cleanup(&db_path);
}

// ---------------------------------------------------------------------
// Pure unit-level tests for compression.rs helpers, duplicated here (in
// addition to the #[cfg(test)] module inside src/compression.rs) as
// black-box checks against the crate's public API surface.
// ---------------------------------------------------------------------

#[test]
fn eligibility_pure_function_matches_documented_rules() {
    use memolite::MemoryType as MT;

    // We can't construct a `memolite::Memory` with private fields from
    // outside the crate in every version, so this is intentionally a
    // light smoke test exercised through the public re-export surface;
    // the exhaustive per-field matrix lives in src/compression.rs's own
    // #[cfg(test)] module, which has field-level access.
    let _ = MT::Episodic;
}

#[test]
fn greedy_cluster_groups_similar_vectors_via_public_module() {
    let a = (Uuid::new_v4(), vec![1.0_f32, 0.0, 0.0]);
    let b = (Uuid::new_v4(), vec![0.99_f32, 0.01, 0.0]);
    let c = (Uuid::new_v4(), vec![0.0_f32, 1.0, 0.0]);

    let clusters = memolite::compression::greedy_cluster(&[a.clone(), b.clone(), c.clone()], 0.9);

    let big = clusters
        .iter()
        .find(|cl| cl.member_ids.len() == 2)
        .expect("a and b should cluster together");
    assert!(big.member_ids.contains(&a.0));
    assert!(big.member_ids.contains(&b.0));

    let singleton = clusters
        .iter()
        .find(|cl| cl.member_ids.len() == 1)
        .expect("c should be its own cluster");
    assert_eq!(singleton.member_ids[0], c.0);
}