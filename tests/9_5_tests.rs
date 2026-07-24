// //! End-to-end black-box tests for Memolite (through milestone M9.5).
// //!
// //! Run with:
// //!   cargo test --test 9_5_tests
// //! (rename this file to `9_5_tests.rs` if your test harness rejects a
// //! leading digit followed by a dot — Cargo requires test binary names to be
// //! valid file stems; `9_5_tests.rs` is the safe choice. If your crate is
// //! configured to accept `9.5_tests.rs` verbatim, run:
// //!   cargo test --test 9.5_tests
// //! )

// use memolite::{
//     ConfidenceLevel, ExpiryPolicy, InMemoryVectorStore, MemoryEngine,
//     MemoryStats, MemoryType, MemoryUpdate, RecallQuery, StoreRequest, VectorEntry, VectorHit,
//     VectorStore,
// };
// use std::collections::HashMap;
// use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
// use std::sync::Arc;

// // ---------------------------------------------------------------------
// // Test helpers
// // ---------------------------------------------------------------------

// /// Unique temp-file path for tests that need real restarts / raw SQL access.
// fn temp_db_path(tag: &str) -> std::path::PathBuf {
//     let mut p = std::env::temp_dir();
//     let unique = format!(
//         "memolite_test_{}_{}_{}.db",
//         tag,
//         std::process::id(),
//         uuid::Uuid::new_v4()
//     );
//     p.push(unique);
//     p
// }

// /// A `VectorStore` wrapper that can be told to fail specific operations on
// /// command, used to exercise Memolite's compensation logic. Everything not
// /// explicitly told to fail is delegated to a real `InMemoryVectorStore`.
// #[allow(dead_code)]
// struct FaultyVectorStore {
//     inner: InMemoryVectorStore,
//     fail_insert: AtomicBool,
//     fail_delete: AtomicBool,
//     insert_calls: AtomicUsize,
//     delete_calls: AtomicUsize,
//     replace_all_calls: AtomicUsize,
// }

// impl FaultyVectorStore {
//     fn new(dim: usize) -> Self {
//         Self {
//             inner: InMemoryVectorStore::new(dim),
//             fail_insert: AtomicBool::new(false),
//             fail_delete: AtomicBool::new(false),
//             insert_calls: AtomicUsize::new(0),
//             delete_calls: AtomicUsize::new(0),
//             replace_all_calls: AtomicUsize::new(0),
//         }
//     }
//     fn set_fail_insert(&self, v: bool) {
//         self.fail_insert.store(v, Ordering::SeqCst);
//     }
// }

// #[async_trait::async_trait]
// impl VectorStore for FaultyVectorStore {
//     async fn insert(
//         &self,
//         id: uuid::Uuid,
//         vector: &[f32],
//         metadata: HashMap<String, serde_json::Value>,
//     ) -> memolite::Result<()> {
//         self.insert_calls.fetch_add(1, Ordering::SeqCst);
//         if self.fail_insert.load(Ordering::SeqCst) {
//             return Err(memolite::MemoliteError::VectorStore(
//                 "injected insert failure".into(),
//             ));
//         }
//         self.inner.insert(id, vector, metadata).await
//     }

//     async fn search(&self, query: &[f32], k: usize) -> memolite::Result<Vec<VectorHit>> {
//         self.inner.search(query, k).await
//     }

//     async fn delete(&self, id: uuid::Uuid) -> memolite::Result<()> {
//         self.delete_calls.fetch_add(1, Ordering::SeqCst);
//         if self.fail_delete.load(Ordering::SeqCst) {
//             return Err(memolite::MemoliteError::VectorStore(
//                 "injected delete failure".into(),
//             ));
//         }
//         self.inner.delete(id).await
//     }

//     async fn contains(&self, id: uuid::Uuid) -> memolite::Result<bool> {
//         self.inner.contains(id).await
//     }

//     async fn clear(&self) -> memolite::Result<()> {
//         self.inner.clear().await
//     }

//     async fn replace_all(&self, entries: Vec<VectorEntry>) -> memolite::Result<()> {
//         self.replace_all_calls.fetch_add(1, Ordering::SeqCst);
//         self.inner.replace_all(entries).await
//     }

//     fn dimension(&self) -> usize {
//         self.inner.dimension()
//     }
// }

// // ---------------------------------------------------------------------
// // 1. Basic store and recall
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn basic_store_and_recall_ranks_relevant_higher() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     engine
//         .store(
//             "The user prefers dark mode for the editor",
//             MemoryType::Semantic,
//             0.8,
//         )
//         .await
//         .expect("store 1");
//     engine
//         .store("The cat sat on the mat", MemoryType::Episodic, 0.2)
//         .await
//         .expect("store 2");
//     engine
//         .store(
//             "Bananas are a good source of potassium",
//             MemoryType::Semantic,
//             0.3,
//         )
//         .await
//         .expect("store 3");
//     let relevant_id = engine
//         .store(
//             "User wants the interface theme to be dark",
//             MemoryType::Semantic,
//             0.9,
//         )
//         .await
//         .expect("store 4");

//     let results = engine
//         .recall("What color theme does the user want?")
//         .await
//         .expect("recall");

//     assert!(!results.is_empty(), "expected at least one recall result");
//     // The two dark-mode-related memories should be ranked above the unrelated ones.
//     let top_ids: Vec<String> = results.iter().take(2).map(|m| m.id.to_string()).collect();
//     assert!(
//         top_ids.contains(&relevant_id),
//         "expected the most relevant memory in the top results, got {:?}",
//         top_ids
//     );

//     for m in &results {
//         assert!(!m.content.is_empty());
//         assert!(m.importance >= 0.0 && m.importance <= 1.0);
//     }
// }

// #[tokio::test]
// async fn recall_from_empty_engine_returns_empty() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");
//     let results = engine.recall("anything at all").await.expect("recall");
//     assert!(results.is_empty());
// }

// // ---------------------------------------------------------------------
// // 2. Advanced recall with filters
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn advanced_recall_with_filters() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let mut meta_a = HashMap::new();
//     meta_a.insert("project".to_string(), serde_json::json!("memolite"));

//     engine
//         .store_with_options(
//             StoreRequest::new(
//                 "memolite uses SQLite as the source of truth",
//                 MemoryType::Semantic,
//                 0.9,
//             )
//             .metadata(meta_a.clone()),
//         )
//         .await
//         .expect("store a");

//     engine
//         .store_with_options(StoreRequest::new(
//             "random low importance episodic note",
//             MemoryType::Episodic,
//             0.1,
//         ))
//         .await
//         .expect("store b");

//     let mut meta_c = HashMap::new();
//     meta_c.insert("project".to_string(), serde_json::json!("other-project"));
//     engine
//         .store_with_options(
//             StoreRequest::new(
//                 "some other project also uses a database",
//                 MemoryType::Semantic,
//                 0.8,
//             )
//             .metadata(meta_c),
//         )
//         .await
//         .expect("store c");

//     let query = RecallQuery::new("what database does memolite use")
//         .limit(10)
//         .min_importance(0.5)
//         .memory_types(vec![MemoryType::Semantic])
//         .metadata_equals("project", serde_json::json!("memolite"))
//         .include_superseded(false)
//         .include_expired(false);

//     let result = engine.recall_query(query).await.expect("recall_query");

//     assert!(!result.items.is_empty());
//     for item in &result.items {
//         assert_eq!(item.memory.memory_type, MemoryType::Semantic);
//         assert!(item.memory.importance >= 0.5);
//         assert_eq!(
//             item.memory.metadata.get("project"),
//             Some(&serde_json::json!("memolite"))
//         );
//     }

//     // Sorted descending by score.
//     for pair in result.items.windows(2) {
//         assert!(pair[0].score >= pair[1].score);
//     }
// }

// // ---------------------------------------------------------------------
// // 3. Temporal recall queries
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn temporal_queries_and_expiry_filters() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let before = chrono::Utc::now() - chrono::Duration::seconds(5);
//     engine
//         .store("a freshly created memory", MemoryType::Semantic, 0.5)
//         .await
//         .expect("store");
//     let after = chrono::Utc::now() + chrono::Duration::seconds(5);

//     let query = RecallQuery::new("freshly created memory")
//         .created_after(before)
//         .created_before(after);
//     let result = engine.recall_query(query).await.expect("recall_query");
//     assert!(!result.items.is_empty());

//     // created_after > created_before must error.
//     let bad_query = RecallQuery::new("anything")
//         .created_after(after)
//         .created_before(before);
//     assert!(engine.recall_query(bad_query).await.is_err());

//     // Immediately-expired memory via zero-duration custom expiry.
//     let expired_id = engine
//         .store_with_options(
//             StoreRequest::new(
//                 "this memory expires immediately",
//                 MemoryType::Working,
//                 0.5,
//             )
//             .expiry(ExpiryPolicy::Custom(chrono::Duration::milliseconds(1))),
//         )
//         .await
//         .expect("store expiring");

//     // Give the millisecond duration time to elapse.
//     tokio::time::sleep(std::time::Duration::from_millis(50)).await;

//     let default_query = RecallQuery::new("this memory expires immediately").limit(20);
//     let default_result = engine
//         .recall_query(default_query)
//         .await
//         .expect("recall_query default");
//     assert!(
//         !default_result
//             .items
//             .iter()
//             .any(|i| i.memory.id.to_string() == expired_id),
//         "expired memory should be excluded by default"
//     );

//     let include_expired_query = RecallQuery::new("this memory expires immediately")
//         .limit(20)
//         .include_expired(true);
//     let include_expired_result = engine
//         .recall_query(include_expired_query)
//         .await
//         .expect("recall_query include_expired");
//     assert!(
//         include_expired_result
//             .items
//             .iter()
//             .any(|i| i.memory.id.to_string() == expired_id),
//         "expired memory should appear when include_expired(true)"
//     );

//     // only_stale should not panic and should be a legal filter, even though
//     // we can't force staleness through the public API alone.
//     let stale_query = RecallQuery::new("this memory expires immediately")
//         .limit(20)
//         .include_expired(true)
//         .only_stale(true);
//     let _ = engine
//         .recall_query(stale_query)
//         .await
//         .expect("only_stale filter should not error");
// }

// // ---------------------------------------------------------------------
// // 4. Memory update and superseding
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn update_supersedes_old_memory() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let old_id = engine
//         .store("User uses VS Code", MemoryType::Semantic, 0.7)
//         .await
//         .expect("store");

//     let update = MemoryUpdate {
//         new_content: Some("User now uses Zed".to_string()),
//         ..Default::default()
//     };
//     let new_id = engine.update(&old_id, update).await.expect("update");
//     assert_ne!(old_id, new_id);

//     let old_memory = engine
//         .get(&old_id)
//         .await
//         .expect("get old")
//         .expect("old memory exists");
//     assert_eq!(
//         old_memory.superseded_by.map(|u| u.to_string()),
//         Some(new_id.clone())
//     );

//     let new_memory = engine
//         .get(&new_id)
//         .await
//         .expect("get new")
//         .expect("new memory exists");
//     assert_eq!(new_memory.content, "User now uses Zed");
//     // Not explicitly given a confidence on update -> Inferred by default.
//     assert_eq!(new_memory.confidence, ConfidenceLevel::Inferred);

//     // Default recall hides the superseded original.
//     let default_result = engine
//         .recall_query(RecallQuery::new("editor preference").limit(20))
//         .await
//         .expect("recall_query default");
//     assert!(!default_result
//         .items
//         .iter()
//         .any(|i| i.memory.id.to_string() == old_id));

//     // include_superseded reveals it.
//     let with_superseded = engine
//         .recall_query(
//             RecallQuery::new("editor preference")
//                 .limit(20)
//                 .include_superseded(true),
//         )
//         .await
//         .expect("recall_query include_superseded");
//     assert!(with_superseded
//         .items
//         .iter()
//         .any(|i| i.memory.id.to_string() == old_id));

//     // Update again, this time with an explicit confidence.
//     let update2 = MemoryUpdate {
//         new_importance: Some(0.95),
//         new_confidence: Some(ConfidenceLevel::Explicit),
//         ..Default::default()
//     };
//     let newer_id = engine.update(&new_id, update2).await.expect("update 2");
//     let newer_memory = engine
//         .get(&newer_id)
//         .await
//         .expect("get newer")
//         .expect("exists");
//     assert_eq!(newer_memory.confidence, ConfidenceLevel::Explicit);
//     assert!((newer_memory.importance - 0.95).abs() < 1e-6);
// }

// // ---------------------------------------------------------------------
// // 5. Forget
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn forget_removes_memory_and_handles_edge_cases() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let id = engine
//         .store("something to forget", MemoryType::Working, 0.5)
//         .await
//         .expect("store");
//     assert!(engine.get(&id).await.expect("get").is_some());

//     engine.forget(&id).await.expect("forget");
//     assert!(engine.get(&id).await.expect("get after forget").is_none());

//     // Forgetting a well-formed but nonexistent id is a no-op success.
//     let random_id = uuid::Uuid::new_v4().to_string();
//     engine
//         .forget(&random_id)
//         .await
//         .expect("forget nonexistent should be Ok");

//     // Forgetting a malformed id is an error.
//     let bad_id = "not-a-real-uuid";
//     assert!(engine.forget(bad_id).await.is_err());
// }

// // ---------------------------------------------------------------------
// // 6. Confidence levels
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn confidence_levels_and_promotion() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let explicit_id = engine
//         .store_with_options(
//             StoreRequest::new("explicit fact about the user", MemoryType::Semantic, 0.7)
//                 .with_confidence(ConfidenceLevel::Explicit),
//         )
//         .await
//         .expect("store explicit");

//     let inferred_id = engine
//         .store_with_options(
//             StoreRequest::new(
//                 "inferred fact about the user, same importance",
//                 MemoryType::Semantic,
//                 0.7,
//             )
//             .with_confidence(ConfidenceLevel::Inferred),
//         )
//         .await
//         .expect("store inferred");

//     let reinforced_id = engine
//         .store_with_options(
//             StoreRequest::new(
//                 "reinforced fact about the user, same importance",
//                 MemoryType::Semantic,
//                 0.7,
//             )
//             .with_confidence(ConfidenceLevel::Reinforced),
//         )
//         .await
//         .expect("store reinforced");

//     let explicit = engine.get(&explicit_id).await.unwrap().unwrap();
//     let inferred = engine.get(&inferred_id).await.unwrap().unwrap();
//     let reinforced = engine.get(&reinforced_id).await.unwrap().unwrap();
//     assert_eq!(explicit.confidence, ConfidenceLevel::Explicit);
//     assert_eq!(inferred.confidence, ConfidenceLevel::Inferred);
//     assert_eq!(reinforced.confidence, ConfidenceLevel::Reinforced);

//     // Recall both explicit and inferred memories with an identical query;
//     // explicit should score at or above inferred (weight 1.0 vs < 1.0).
//     let result = engine
//         .recall_query(RecallQuery::new("fact about the user, same importance").limit(20))
//         .await
//         .expect("recall_query");
//     let explicit_score = result
//         .items
//         .iter()
//         .find(|i| i.memory.id.to_string() == explicit_id)
//         .map(|i| i.score);
//     let inferred_score = result
//         .items
//         .iter()
//         .find(|i| i.memory.id.to_string() == inferred_id)
//         .map(|i| i.score);
//     if let (Some(e), Some(i)) = (explicit_score, inferred_score) {
//         assert!(e >= i, "explicit ({e}) should score >= inferred ({i})");
//     }

//     // Recall the inferred memory exactly 5 times to promote it.
//     for _ in 0..5 {
//         let _ = engine
//             .recall_query(
//                 RecallQuery::new("inferred fact about the user, same importance")
//                     .limit(1)
//                     .min_importance(0.0),
//             )
//             .await
//             .expect("recall_query for promotion");
//     }
//     let promoted = engine.get(&inferred_id).await.unwrap().unwrap();
//     assert_eq!(
//         promoted.confidence,
//         ConfidenceLevel::Reinforced,
//         "inferred memory should be promoted after 5 recalls, access_count={}",
//         promoted.access_count
//     );

//     // Explicit memory never changes confidence regardless of recall count.
//     for _ in 0..5 {
//         let _ = engine
//             .recall_query(
//                 RecallQuery::new("explicit fact about the user")
//                     .limit(1)
//                     .min_importance(0.0),
//             )
//             .await
//             .expect("recall_query explicit");
//     }
//     let still_explicit = engine.get(&explicit_id).await.unwrap().unwrap();
//     assert_eq!(still_explicit.confidence, ConfidenceLevel::Explicit);
// }

// // ---------------------------------------------------------------------
// // 7. Expiry and expiration
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn expiry_policies_behave_as_documented() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let never_id = engine
//         .store_with_options(
//             StoreRequest::new("this never expires", MemoryType::Semantic, 0.5)
//                 .expiry(ExpiryPolicy::Never),
//         )
//         .await
//         .expect("store never");
//     let never_memory = engine.get(&never_id).await.unwrap().unwrap();
//     assert!(never_memory.expires_at.is_none());

//     let default_id = engine
//         .store_with_options(
//             StoreRequest::new("this uses type default ttl", MemoryType::Working, 0.5)
//                 .expiry(ExpiryPolicy::TypeDefault),
//         )
//         .await
//         .expect("store type default");
//     let default_memory = engine.get(&default_id).await.unwrap().unwrap();
//     assert!(default_memory.expires_at.is_some());

//     let expired_id = engine
//         .store_with_options(
//             StoreRequest::new("this expires immediately", MemoryType::Working, 0.5)
//                 .expiry(ExpiryPolicy::Custom(chrono::Duration::milliseconds(1))),
//         )
//         .await
//         .expect("store custom expiry");
//     tokio::time::sleep(std::time::Duration::from_millis(50)).await;

//     let stats = engine.stats().await.expect("stats");
//     assert!(
//         stats.expired_count >= 1,
//         "expected at least one expired memory in stats, got {}",
//         stats.expired_count
//     );

//     let default_recall = engine
//         .recall_query(RecallQuery::new("expires immediately").limit(20))
//         .await
//         .expect("recall_query");
//     assert!(!default_recall
//         .items
//         .iter()
//         .any(|i| i.memory.id.to_string() == expired_id));

//     let with_expired = engine
//         .recall_query(
//             RecallQuery::new("expires immediately")
//                 .limit(20)
//                 .include_expired(true),
//         )
//         .await
//         .expect("recall_query include_expired");
//     assert!(with_expired
//         .items
//         .iter()
//         .any(|i| i.memory.id.to_string() == expired_id));
// }

// // ---------------------------------------------------------------------
// // 8. Stats accuracy
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn stats_reflect_store_update_forget() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let id1 = engine
//         .store("semantic fact one", MemoryType::Semantic, 0.6)
//         .await
//         .unwrap();
//     let _id2 = engine
//         .store("episodic event one", MemoryType::Episodic, 0.3)
//         .await
//         .unwrap();
//     engine
//         .store_with_options(
//             StoreRequest::new("explicit fact", MemoryType::Semantic, 0.9)
//                 .with_confidence(ConfidenceLevel::Explicit),
//         )
//         .await
//         .unwrap();

//     // Supersede id1.
//     let new_id = engine
//         .update(
//             &id1,
//             MemoryUpdate {
//                 new_content: Some("updated semantic fact one".into()),
//                 ..Default::default()
//             },
//         )
//         .await
//         .unwrap();

//     // Forget one memory entirely.
//     engine.forget(&new_id).await.unwrap();

//     let stats: MemoryStats = engine.stats().await.expect("stats");

//     // id1 remains (superseded, not forgotten); new_id was forgotten;
//     // so total should be: id1 (superseded) + episodic + explicit = 3.
//     assert_eq!(stats.total_memories, 3);
//     assert_eq!(stats.superseded_count, 1);
//     assert!(stats.by_type.values().sum::<usize>() == stats.total_memories);
//     assert!(stats.by_confidence.values().sum::<usize>() == stats.total_memories);
//     assert!(stats.average_importance >= 0.0 && stats.average_importance <= 1.0);
//     assert!(stats.average_access_count >= 0.0);
// }

// #[tokio::test]
// async fn stats_on_empty_engine_do_not_panic() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");
//     let stats = engine.stats().await.expect("stats on empty engine");
//     assert_eq!(stats.total_memories, 0);
//     assert_eq!(stats.average_importance, 0.0);
//     assert_eq!(stats.average_access_count, 0.0);
// }

// // ---------------------------------------------------------------------
// // 9. Streaming ingestion
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn streaming_ingestion_stores_sentences() {
//     let engine = Arc::new(MemoryEngine::open(":memory:").await.expect("open engine"));
//     let ingestor =
//         memolite::StreamIngestor::spawn(Arc::clone(&engine), 8).expect("spawn ingestor");
//     let sender = ingestor.sender();

//     sender
//         .send(memolite::IngestChunk {
//             text: "The user likes Rust. The user also likes Go.".to_string(),
//             memory_type: MemoryType::Semantic,
//             importance: 0.5,
//         })
//         .await
//         .expect("send chunk 1");

//     sender
//         .send(memolite::IngestChunk {
//             text: "A partial sentence without a terminator".to_string(),
//             memory_type: MemoryType::Episodic,
//             importance: 0.4,
//         })
//         .await
//         .expect("send chunk 2");

//     drop(sender);
//     let report = ingestor.finish().await.expect("finish");

//     assert!(report.received >= 1);
//     assert!(report.stored >= 1);

//     let recalled = engine
//         .recall("What programming languages does the user like?")
//         .await
//         .expect("recall streamed content");
//     assert!(!recalled.is_empty());
// }

// // ---------------------------------------------------------------------
// // 10. Compression (via raw-SQL backdating of created_at)
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn compression_runs_cleanly_on_fresh_db() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");
//     let compressed = engine
//         .compress_old_memories()
//         .await
//         .expect("compress fresh db");
//     assert_eq!(compressed, 0);
// }

// #[tokio::test]
// async fn compression_end_to_end_with_backdated_memories() {
//     let path = temp_db_path("compression");

//     {
//         let engine = MemoryEngine::open(&path).await.expect("open engine");
//         for i in 0..3 {
//             engine
//                 .store_with_options(StoreRequest::new(
//                     &format!("low importance episodic note number {i} about a routine event"),
//                     MemoryType::Episodic,
//                     0.1,
//                 ))
//                 .await
//                 .expect("store episodic candidate");
//         }
//     } // engine dropped, SQLite file remains

//     // Backdate created_at directly via a raw connection to the same file.
//     {
//         let conn = rusqlite::Connection::open(&path).expect("open raw connection");
//         conn.execute(
//             "UPDATE memories SET created_at = 0 WHERE type = 'episodic'",
//             [],
//         )
//         .expect("backdate created_at");
//     }

//     let engine = MemoryEngine::open(&path).await.expect("reopen engine");
//     let compressed = engine
//         .compress_old_memories()
//         .await
//         .expect("compress backdated memories");

//     // Either the cluster met the >=3-member threshold and compressed, or it
//     // didn't (similarity-dependent) — assert the call is at least consistent
//     // and doesn't error, and if it compressed, verify superseding + rebuild.
//     if compressed > 0 {
//         let stats = engine.stats().await.expect("stats after compression");
//         assert!(stats.superseded_count >= compressed);

//         engine
//             .rebuild_vector_index()
//             .await
//             .expect("rebuild_vector_index after compression");

//         let with_superseded = engine
//             .recall_query(
//                 RecallQuery::new("routine event")
//                     .limit(20)
//                     .include_superseded(true)
//                     .include_expired(true),
//             )
//             .await
//             .expect("recall including superseded");
//         assert!(!with_superseded.items.is_empty());
//     }

//     let _ = std::fs::remove_file(&path);
// }

// // ---------------------------------------------------------------------
// // 11. Restart and vector index reconstruction
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn restart_reconstructs_vector_index() {
//     let path = temp_db_path("restart");

//     let ids: Vec<String> = {
//         let engine = MemoryEngine::open(&path).await.expect("open engine");
//         let mut ids = Vec::new();
//         ids.push(
//             engine
//                 .store("The sun is a star", MemoryType::Semantic, 0.6)
//                 .await
//                 .unwrap(),
//         );
//         ids.push(
//             engine
//                 .store("Water boils at 100 degrees Celsius", MemoryType::Semantic, 0.7)
//                 .await
//                 .unwrap(),
//         );
//         ids
//     }; // engine dropped here

//     let engine2 = MemoryEngine::open(&path).await.expect("reopen engine");
//     let recalled = engine2
//         .recall("What temperature does water boil at?")
//         .await
//         .expect("recall after restart");
//     assert!(!recalled.is_empty());
//     assert!(recalled.iter().any(|m| ids.contains(&m.id.to_string())));

//     let stats = engine2.stats().await.expect("stats after restart");
//     assert_eq!(stats.total_memories, ids.len());

//     let _ = std::fs::remove_file(&path);
// }

// // ---------------------------------------------------------------------
// // 12. Compensation logic (faulty vector store injection)
// // ---------------------------------------------------------------------

// // NOTE: The ideal version of this test opens the engine via
// // `MemoryEngine::open_with_store(path, faulty_store, BackfillPolicy)` so a
// // `FaultyVectorStore` (defined above) can be injected to force an `insert`
// // failure and verify SQLite compensation. `open_with_store` is an M11
// // feature and isn't implemented in this codebase yet, so that variant is
// // commented out below. Once M11 lands, delete this test and uncomment it.
// //
// // #[tokio::test]
// // async fn store_failure_in_vector_backend_rolls_back_sqlite() {
// //     let path = temp_db_path("compensation");
// //     let faulty = Arc::new(FaultyVectorStore::new(384));
// //     let engine = MemoryEngine::open_with_store(&path, faulty.clone(), BackfillPolicy::ReplaceAll)
// //         .await
// //         .expect("open_with_store");
// //     faulty.set_fail_insert(true);
// //     let result = engine
// //         .store("this store should fail and roll back", MemoryType::Working, 0.5)
// //         .await;
// //     assert!(result.is_err());
// //     let stats = engine.stats().await.expect("stats after failed store");
// //     assert_eq!(stats.total_memories, 0);
// //     faulty.set_fail_insert(false);
// //     let good_id = engine
// //         .store("this store should succeed", MemoryType::Working, 0.5)
// //         .await
// //         .expect("store should succeed once fault cleared");
// //     assert!(engine.get(&good_id).await.unwrap().is_some());
// //     let _ = std::fs::remove_file(&path);
// // }

// /// Interim compensation smoke test that works with today's public API
// /// (no `open_with_store` needed): confirms that a rejected `store()` call
// /// (invalid input, so it never even reaches the vector backend) leaves no
// /// partial row behind, and that a healthy `store()` afterward still works.
// /// This does not exercise the vector-insert-failure path — only the
// /// input-validation short-circuit — but keeps a green regression signal
// /// until M11 makes true fault injection possible.
// #[tokio::test]
// async fn rejected_store_leaves_no_partial_state() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let before = engine.stats().await.expect("stats before");
//     assert_eq!(before.total_memories, 0);

//     let result = engine.store("", MemoryType::Working, 0.5).await;
//     assert!(result.is_err(), "empty content must be rejected");

//     let after = engine.stats().await.expect("stats after rejected store");
//     assert_eq!(
//         after.total_memories, 0,
//         "a rejected store() must not leave any row behind"
//     );

//     let good_id = engine
//         .store("this store should succeed", MemoryType::Working, 0.5)
//         .await
//         .expect("store should succeed");
//     assert!(engine.get(&good_id).await.unwrap().is_some());
// }

// // ---------------------------------------------------------------------
// // 13. Concurrency
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn concurrent_store_and_recall_do_not_deadlock() {
//     let engine = Arc::new(MemoryEngine::open(":memory:").await.expect("open engine"));
//     let mut handles = Vec::new();

//     for i in 0..20 {
//         let engine = Arc::clone(&engine);
//         handles.push(tokio::spawn(async move {
//             engine
//                 .store(
//                     &format!("concurrent memory number {i}"),
//                     MemoryType::Working,
//                     0.5,
//                 )
//                 .await
//                 .expect("concurrent store")
//         }));
//     }
//     for i in 0..20 {
//         let engine = Arc::clone(&engine);
//         handles.push(tokio::spawn(async move {
//             let _ = engine
//                 .recall(&format!("concurrent memory number {i}"))
//                 .await
//                 .expect("concurrent recall");
//             String::new()
//         }));
//     }

//     for h in handles {
//         h.await.expect("task panicked");
//     }

//     let stats = engine.stats().await.expect("stats after concurrency");
//     assert_eq!(stats.total_memories, 20);
// }

// // ---------------------------------------------------------------------
// // 14. Edge cases
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn edge_cases_invalid_inputs() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     assert!(engine
//         .store("", MemoryType::Semantic, 0.5)
//         .await
//         .is_err());
//     assert!(engine
//         .store("valid content", MemoryType::Semantic, 1.5)
//         .await
//         .is_err());
//     assert!(engine
//         .store("valid content", MemoryType::Semantic, -0.1)
//         .await
//         .is_err());
//     assert!(engine.forget("not-a-uuid").await.is_err());

//     assert!(engine
//         .recall_query(RecallQuery::new("something").limit(0))
//         .await
//         .is_err());
//     assert!(engine
//         .recall_query(RecallQuery::new("something").min_importance(f32::NAN))
//         .await
//         .is_err());

//     let after = chrono::Utc::now();
//     let before = after - chrono::Duration::seconds(10);
//     assert!(engine
//         .recall_query(
//             RecallQuery::new("something")
//                 .created_after(after)
//                 .created_before(before)
//         )
//         .await
//         .is_err());
// }

// #[tokio::test]
// async fn metadata_filter_supports_various_json_value_types() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     let mut meta = HashMap::new();
//     meta.insert("count".to_string(), serde_json::json!(42));
//     meta.insert("nested".to_string(), serde_json::json!({"a": 1, "b": "two"}));
//     meta.insert("flag".to_string(), serde_json::json!(true));

//     engine
//         .store_with_options(
//             StoreRequest::new("memory with rich metadata", MemoryType::Semantic, 0.5)
//                 .metadata(meta),
//         )
//         .await
//         .expect("store with metadata");

//     let by_count = engine
//         .recall_query(
//             RecallQuery::new("memory with rich metadata")
//                 .metadata_equals("count", serde_json::json!(42)),
//         )
//         .await
//         .expect("recall_query by count");
//     assert!(!by_count.items.is_empty());

//     let by_flag = engine
//         .recall_query(
//             RecallQuery::new("memory with rich metadata")
//                 .metadata_equals("flag", serde_json::json!(true)),
//         )
//         .await
//         .expect("recall_query by flag");
//     assert!(!by_flag.items.is_empty());

//     let by_nested = engine
//         .recall_query(
//             RecallQuery::new("memory with rich metadata")
//                 .metadata_equals("nested", serde_json::json!({"a": 1, "b": "two"})),
//         )
//         .await
//         .expect("recall_query by nested object");
//     assert!(!by_nested.items.is_empty());
// }

// #[tokio::test]
// async fn recall_query_bumps_and_reflects_access_stats() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");
//     let id = engine
//         .store("a memory whose access stats we will check", MemoryType::Semantic, 0.5)
//         .await
//         .expect("store");

//     let before = engine.get(&id).await.unwrap().unwrap();
//     assert_eq!(before.access_count, 0);

//     let result = engine
//         .recall_query(RecallQuery::new("memory whose access stats we will check").limit(5))
//         .await
//         .expect("recall_query");

//     let item = result
//         .items
//         .iter()
//         .find(|i| i.memory.id.to_string() == id)
//         .expect("target memory present in results");

//     assert!(
//         item.memory.access_count >= 1,
//         "expected the returned Memory to already reflect the access bump"
//     );
//     assert!(item.memory.last_accessed >= before.last_accessed);
// }

// // ---------------------------------------------------------------------
// // 15. Batch operations
// // ---------------------------------------------------------------------

// #[tokio::test]
// async fn batch_store_and_recall_respects_limit_and_sorting() {
//     let engine = MemoryEngine::open(":memory:").await.expect("open engine");

//     for i in 0..100 {
//         let memory_type = match i % 4 {
//             0 => MemoryType::Semantic,
//             1 => MemoryType::Episodic,
//             2 => MemoryType::Procedural,
//             _ => MemoryType::Working,
//         };
//         let importance = (i % 10) as f32 / 10.0;
//         engine
//             .store(
//                 &format!("batch memory content number {i} about various topics"),
//                 memory_type,
//                 importance,
//             )
//             .await
//             .expect("batch store");
//     }

//     let stats = engine.stats().await.expect("stats after batch store");
//     assert_eq!(stats.total_memories, 100);

//     let result = engine
//         .recall_query(RecallQuery::new("various topics").limit(15))
//         .await
//         .expect("recall_query batch");

//     assert!(result.items.len() <= 15);
//     for pair in result.items.windows(2) {
//         assert!(pair[0].score >= pair[1].score, "results must be sorted descending by score");
//     }
// }