//! Demonstrates StreamIngestor end to end: spawn an ingestor against a
//! real MemoryEngine, feed it a few facts through SentenceBuffer, and
//! inspect the resulting IngestReport.

use std::sync::Arc;

use memolite::{IngestChunk, MemoryEngine, MemoryType, StreamIngestor};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let engine = Arc::new(MemoryEngine::open("streaming_demo.db").await?);
    let ingestor = StreamIngestor::spawn(engine.clone(), 32)?;
    let sender = ingestor.sender();

    // Simulate text arriving in fragments (e.g. LLM tokens / a live
    // transcript) and use SentenceBuffer to turn it into whole sentences
    // before handing each one to the ingestor.
    let mut buffer = memolite::streaming::SentenceBuffer::new();
    let fragments = [
        "I prefer ",
        "Zed for Rust proj",
        "ects. ",
        "I always run Clippy before comm",
        "itting!",
    ];

    for fragment in fragments {
        for sentence in buffer.feed(fragment) {
            sender
                .send(IngestChunk {
                    text: sentence,
                    memory_type: MemoryType::Semantic,
                    importance: 0.7,
                })
                .await?;
        }
    }
    // Flush whatever partial sentence is left at "stream end".
    if let Some(rest) = buffer.finish() {
        sender
            .send(IngestChunk {
                text: rest,
                memory_type: MemoryType::Semantic,
                importance: 0.7,
            })
            .await?;
    }

    // Drop our extra sender clone, keep only the ingestor's own — then
    // finish() will drain everything queued and wait for completion.
    drop(sender);
    let report = ingestor.finish().await?;

    println!(
        "received={} stored={} failed={}",
        report.received, report.stored, report.failed
    );
    for failure in &report.errors {
        println!("  failed chunk: {:?} -> {}", failure.preview, failure.error);
    }

    // Prove it actually landed.
    let recalled = engine.recall("What editor does the user prefer?").await?;
    for m in recalled {
        println!("recalled: {} (importance {})", m.content, m.importance);
    }

    Ok(())
}