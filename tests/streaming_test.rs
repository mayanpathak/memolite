//! Black-box integration tests for M8 streaming ingestion. Exercises the
//! public API only, against a real MemoryEngine (real SQLite, real
//! embedder) — no mocking of the engine internals.

use std::sync::Arc;

use memolite::{
    IngestChunk, MemoryEngine, MemoryType, RecallQuery, StreamIngestor,
};
use memolite::streaming::SentenceBuffer;

async fn open_test_engine() -> Arc<MemoryEngine> {
    Arc::new(
        MemoryEngine::open(":memory:")
            .await
            .expect("engine should open"),
    )
}

fn chunk(text: &str) -> IngestChunk {
    IngestChunk {
        text: text.to_string(),
        memory_type: MemoryType::Semantic,
        importance: 0.6,
    }
}

// ---------------------------------------------------------------------
// SentenceBuffer — unit-level
// ---------------------------------------------------------------------

#[test]
fn sentence_buffer_splits_on_terminal_punctuation() {
    let mut buf = SentenceBuffer::new();
    let out = buf.feed("Hello world. How are you? I am fine!");
    assert_eq!(
        out,
        vec!["Hello world.", "How are you?", "I am fine!"]
    );
}

#[test]
fn sentence_buffer_holds_partial_sentence_across_feeds() {
    let mut buf = SentenceBuffer::new();
    assert!(buf.feed("The user prefers ").is_empty());
    let out = buf.feed("dark mode.");
    assert_eq!(out, vec!["The user prefers dark mode."]);
}

#[test]
fn sentence_buffer_is_unicode_safe_across_a_boundary() {
    let mut buf = SentenceBuffer::new();
    // Multi-byte chars immediately surrounding a sentence boundary must
    // not panic or corrupt the split.
    let out = buf.feed("café is nice. 日本語のテスト。more text.");
    assert!(!out.is_empty());
    assert!(out[0].contains("café"));
}

#[test]
fn sentence_buffer_treats_abbreviations_as_boundaries_known_limitation() {
    let mut buf = SentenceBuffer::new();
    // Documented simplification, asserted rather than "fixed".
    let out = buf.feed("Dr. Smith arrived.");
    assert_eq!(out, vec!["Dr.", "Smith arrived."]);
}

#[test]
fn sentence_buffer_finish_flushes_trailing_partial() {
    let mut buf = SentenceBuffer::new();
    buf.feed("no terminator here");
    assert_eq!(
        buf.finish(),
        Some("no terminator here".to_string())
    );
}

#[test]
fn sentence_buffer_finish_on_empty_buffer_is_none() {
    let buf = SentenceBuffer::new();
    assert_eq!(buf.finish(), None);

    let mut buf2 = SentenceBuffer::new();
    buf2.feed("   ");
    assert_eq!(buf2.finish(), None);
}

// ---------------------------------------------------------------------
// StreamIngestor — integration
// ---------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_stream_and_recall() {
    let engine = open_test_engine().await;
    let ingestor = StreamIngestor::spawn(engine.clone(), 8).unwrap();
    let sender = ingestor.sender();

    sender.send(chunk("The user prefers Zed for Rust projects.")).await.unwrap();
    sender.send(chunk("The user always runs Clippy before committing.")).await.unwrap();
    sender.send(chunk("The user's favorite coffee is a flat white.")).await.unwrap();
    drop(sender);

    let report = ingestor.finish().await.unwrap();
    assert_eq!(report.received, 3);
    assert_eq!(report.stored, 3);
    assert_eq!(report.failed, 0);
    assert!(report.errors.is_empty());

    let result = engine
        .recall_query(RecallQuery::new("What should I do before committing?").limit(5))
        .await
        .unwrap();
    assert!(
        result.items.iter().any(|i| i.memory.content.contains("Clippy")),
        "expected the Clippy memory to be recallable, got: {:?}",
        result.items.iter().map(|i| &i.memory.content).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn ingest_report_captures_per_chunk_failures() {
    let engine = open_test_engine().await;
    let ingestor = StreamIngestor::spawn(engine.clone(), 8).unwrap();
    let sender = ingestor.sender();

    sender.send(chunk("A valid fact about the user.")).await.unwrap();
    // Empty content fails StoreRequest validation inside store_with_options.
    sender.send(chunk("")).await.unwrap();
    sender.send(chunk("Another valid fact.")).await.unwrap();
    drop(sender);

    let report = ingestor.finish().await.unwrap();
    assert_eq!(report.received, 3);
    assert_eq!(report.stored, 2);
    assert_eq!(report.failed, 1);
    assert_eq!(report.errors.len(), report.failed);
    assert!(report.errors[0].error.to_lowercase().contains("empty")
        || report.errors[0].error.to_lowercase().contains("content"));
}

#[tokio::test]
async fn finish_drains_full_backlog_across_cloned_senders() {
    let engine = open_test_engine().await;
    let ingestor = StreamIngestor::spawn(engine.clone(), 2).unwrap();

    let sender_a = ingestor.sender();
    let sender_b = ingestor.sender();

    for i in 0..3 {
        sender_a.send(chunk(&format!("fact a{i}"))).await.unwrap();
    }
    for i in 0..2 {
        sender_b.send(chunk(&format!("fact b{i}"))).await.unwrap();
    }

    // finish() only completes once every clone (including the
    // ingestor's own internal sender) is dropped.
    drop(sender_a);
    drop(sender_b);

    let report = ingestor.finish().await.unwrap();
    assert_eq!(report.received, 5);
    assert_eq!(report.stored, 5);
}

#[tokio::test]
async fn shutdown_now_does_not_process_more_than_was_sent() {
    let engine = open_test_engine().await;
    let ingestor = StreamIngestor::spawn(engine.clone(), 32).unwrap();
    let sender = ingestor.sender();

    for i in 0..20 {
        sender.send(chunk(&format!("fact {i}"))).await.unwrap();
    }

    // Cancel essentially immediately, without giving the consumer task
    // a chance to make much (or any) progress.
    let report = ingestor.shutdown_now().await.unwrap();

    assert!(
        report.received <= 20,
        "received {} should never exceed the number sent",
        report.received
    );
    assert_eq!(report.stored + report.failed, report.received);
}

#[tokio::test]
async fn backpressure_with_small_buffer_still_delivers_everything() {
    let engine = open_test_engine().await;
    let ingestor = StreamIngestor::spawn(engine.clone(), 1).unwrap();
    let sender = ingestor.sender();

    let producer = tokio::spawn(async move {
        for i in 0..5 {
            sender.send(chunk(&format!("backpressure fact {i}"))).await.unwrap();
        }
    });
    producer.await.unwrap();

    let report = ingestor.finish().await.unwrap();
    assert_eq!(report.received, 5);
    assert_eq!(report.stored, 5);
}

#[tokio::test]
async fn spawn_rejects_zero_buffer_size() {
    let engine = open_test_engine().await;
    let result = StreamIngestor::spawn(engine, 0);
    assert!(result.is_err());
}

#[tokio::test]
async fn multiple_producer_tasks_are_fully_drained() {
    let engine = open_test_engine().await;
    let ingestor = StreamIngestor::spawn(engine.clone(), 4).unwrap();

    let mut handles = Vec::new();
    for producer_id in 0..3 {
        let sender = ingestor.sender();
        handles.push(tokio::spawn(async move {
            for i in 0..4 {
                sender
                    .send(chunk(&format!("producer {producer_id} item {i}")))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let report = ingestor.finish().await.unwrap();
    assert_eq!(report.received, 12);
    assert_eq!(report.stored, 12);
}

#[tokio::test]
async fn streamed_memories_participate_in_full_ranking_and_filters() {
    let engine = open_test_engine().await;
    let ingestor = StreamIngestor::spawn(engine.clone(), 8).unwrap();
    let sender = ingestor.sender();

    sender.send(IngestChunk {
        text: "The deploy command is `make release`.".to_string(),
        memory_type: MemoryType::Procedural,
        importance: 0.9,
    }).await.unwrap();
    drop(sender);
    ingestor.finish().await.unwrap();

    // Confirms streamed content went through the same transactional
    // store_with_options path M4-M7 rely on: filterable by type,
    // rankable, not just fetchable by raw recall().
    let result = engine
        .recall_query(
            RecallQuery::new("How do I deploy?")
                .memory_types(vec![MemoryType::Procedural])
                .min_importance(0.5),
        )
        .await
        .unwrap();

    assert!(result.items.iter().any(|i| i.memory.content.contains("make release")));
}