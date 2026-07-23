//! M8 — bounded streaming ingestion.
//!
//! Lets a caller feed text incrementally (LLM tokens, a chat transcript
//! arriving piece-by-piece, a log tailer, etc.) instead of only ever
//! calling `MemoryEngine::store()`/`store_with_options()` once per
//! complete fact. A background task consumes `IngestChunk`s from a
//! bounded channel and persists each one through the engine's existing,
//! unmodified public `store_with_options` path — every guarantee that
//! path already has (transactional memory+embedding write, vector-store
//! compensation on failure, confidence defaults) applies to streamed
//! content for free.
//!
//! `SentenceBuffer` is a separate, engine-independent helper: it turns
//! raw incoming text into sentence-sized units so a caller (or a future
//! wiring on top of this module) doesn't have to store one memory per
//! token. It is intentionally simple — see its own docs for the exact
//! boundary rule and its known limitations.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::engine::MemoryEngine;
use crate::error::{MemoliteError, Result};
use crate::memory::MemoryType;
use crate::requests::StoreRequest;

/// One unit of text handed to a [`StreamIngestor`], to be stored as a
/// single memory.
#[derive(Debug, Clone)]
pub struct IngestChunk {
    pub text: String,
    pub memory_type: MemoryType,
    pub importance: f32,
}

/// Detail for one chunk that failed to store. Kept separate from the
/// bare `failed` count in [`IngestReport`] so a caller can actually act
/// on a failure instead of just knowing something, somewhere, broke.
#[derive(Debug, Clone)]
pub struct IngestFailure {
    /// First 80 characters of the offending chunk's text (bounded so a
    /// pathologically large chunk can't bloat the report).
    pub preview: String,
    /// `to_string()` of the `MemoliteError` returned by `store_with_options`.
    pub error: String,
}

/// Summary of one `StreamIngestor` run, returned by both
/// [`StreamIngestor::shutdown_now`] and [`StreamIngestor::finish`].
#[derive(Debug, Default, Clone)]
pub struct IngestReport {
    /// Total chunks pulled off the channel, whether or not storing them
    /// succeeded.
    pub received: usize,
    /// Chunks successfully persisted via `store_with_options`.
    pub stored: usize,
    /// Chunks that failed to persist. Always equal to `errors.len()`.
    pub failed: usize,
    /// One entry per failed chunk, in the order the failures occurred.
    pub errors: Vec<IngestFailure>,
}

/// Splits incoming text into complete sentences on `.`, `!`, or `?`
/// followed by whitespace or end-of-input. Operates on `char`s (not
/// bytes), so it never splits inside a multi-byte codepoint.
///
/// Known, accepted simplification: abbreviations like "e.g." or "Dr."
/// are treated as sentence boundaries. This is a documented limitation,
/// not a bug — a fully correct abbreviation-aware splitter is out of
/// scope for this milestone.
#[derive(Default)]
pub struct SentenceBuffer {
    pending: String,
}

impl SentenceBuffer {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
        }
    }

    /// Feed more text in; returns every complete sentence found so far.
    /// Any trailing partial sentence is retained internally until either
    /// a later `feed()` call completes it, or `finish()` flushes it.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        self.pending.push_str(text);
        let mut out = Vec::new();

        loop {
            let Some(boundary) = self.pending.char_indices().find_map(|(i, c)| {
                if matches!(c, '.' | '!' | '?') {
                    let next = self.pending[i + c.len_utf8()..].chars().next();
                    if next.is_none() || next.map(char::is_whitespace).unwrap_or(false) {
                        return Some(i + c.len_utf8());
                    }
                }
                None
            }) else {
                break;
            };

            let sentence = self.pending[..boundary].trim().to_string();
            self.pending = self.pending[boundary..].trim_start().to_string();
            if !sentence.is_empty() {
                out.push(sentence);
            }
        }

        out
    }

    /// Flushes whatever partial sentence remains (e.g. at stream end).
    /// Consumes `self` since there is nothing meaningful left to feed
    /// into afterward.
    pub fn finish(mut self) -> Option<String> {
        let rest = self.pending.trim().to_string();
        self.pending.clear();
        if rest.is_empty() { None } else { Some(rest) }
    }
}

/// A cloneable handle for sending [`IngestChunk`]s into a running
/// [`StreamIngestor`]. Cloning is cheap (it's just an `mpsc::Sender`
/// clone) — hand out clones to multiple producers freely.
pub struct IngestorSender {
    tx: mpsc::Sender<IngestChunk>,
}

impl IngestorSender {
    /// Sends one chunk. Awaits if the bounded channel is full
    /// (backpressure). Fails only if the ingestor's receiving task has
    /// already stopped (e.g. after `shutdown_now()`/`finish()` completed).
    pub async fn send(&self, chunk: IngestChunk) -> Result<()> {
        self.tx
            .send(chunk)
            .await
            .map_err(|_| MemoliteError::Internal("ingest channel closed".into()))
    }

    pub fn clone_handle(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

/// Owns a background task that drains a bounded channel of
/// [`IngestChunk`]s into a [`MemoryEngine`] via its existing
/// `store_with_options` path. Construct with [`StreamIngestor::spawn`],
/// get sender handles via [`StreamIngestor::sender`], and end the run
/// with either [`StreamIngestor::shutdown_now`] (prompt, drops backlog)
/// or [`StreamIngestor::finish`] (drains everything first).
pub struct StreamIngestor {
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<IngestReport>,
    sender: IngestorSender,
}

impl StreamIngestor {
    /// Spawns the background consumer task. `buffer_size` is the bounded
    /// channel's capacity and must be greater than zero — a
    /// zero-capacity channel can never be fed without an immediate
    /// receiver ready, which is not how this is used here.
    pub fn spawn(engine: Arc<MemoryEngine>, buffer_size: usize) -> Result<Self> {
        if buffer_size == 0 {
            return Err(MemoliteError::InvalidArgument(
                "buffer_size must be > 0".into(),
            ));
        }

        let (tx, mut rx) = mpsc::channel::<IngestChunk>(buffer_size);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let join = tokio::spawn(async move {
            let mut report = IngestReport::default();

            loop {
                tokio::select! {
                    // `biased` makes cancellation win whenever both arms
                    // are ready, so `shutdown_now()` returns promptly
                    // instead of an unbiased select potentially favoring
                    // `rx.recv()` under a continuously-full channel.
                    // `finish()` never cancels the token, so this arm is
                    // simply never ready during a normal drain — the
                    // bias only matters for shutdown_now().
                    biased;
                    _ = cancel_clone.cancelled() => break,
                    maybe_chunk = rx.recv() => {
                        let Some(chunk) = maybe_chunk else {
                            // Channel closed (every IngestorSender dropped) — drain complete.
                            break;
                        };
                        report.received += 1;
                        let request = StoreRequest::new(&chunk.text, chunk.memory_type, chunk.importance);
                        match engine.store_with_options(request).await {
                            Ok(_) => report.stored += 1,
                            Err(e) => {
                                report.failed += 1;
                                let preview: String = chunk.text.chars().take(80).collect();
                                report.errors.push(IngestFailure { preview, error: e.to_string() });
                            }
                        }
                    }
                }
            }

            report
        });

        Ok(Self {
            cancel,
            join,
            sender: IngestorSender { tx },
        })
    }

    /// Returns a new cloned sender handle for feeding this ingestor.
    pub fn sender(&self) -> IngestorSender {
        self.sender.clone_handle()
    }

    /// Cancels immediately. Any chunks still queued in the channel are
    /// never processed. Returns promptly; does not wait for a drain.
    pub async fn shutdown_now(self) -> Result<IngestReport> {
        self.cancel.cancel();
        self.join
            .await
            .map_err(|e| MemoliteError::Internal(e.to_string()))
    }

    /// Drops this ingestor's own sender handle and waits for the channel
    /// to close naturally — i.e. once every cloned `IngestorSender` the
    /// caller is holding is also dropped, standard mpsc close semantics.
    /// Drains the full backlog before the task exits.
    pub async fn finish(self) -> Result<IngestReport> {
        drop(self.sender);
        self.join
            .await
            .map_err(|e| MemoliteError::Internal(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SentenceBuffer: pure logic, no engine needed ---

    #[test]
    fn feed_returns_nothing_without_a_terminal_boundary() {
        let mut buf = SentenceBuffer::new();
        assert!(buf.feed("this has no ending yet").is_empty());
    }

    #[test]
    fn feed_splits_on_period_followed_by_whitespace() {
        let mut buf = SentenceBuffer::new();
        let sentences = buf.feed("First sentence. Second sentence. Third");
        assert_eq!(sentences, vec!["First sentence.", "Second sentence."]);
    }

    #[test]
    fn feed_across_multiple_calls_completes_a_sentence() {
        let mut buf = SentenceBuffer::new();
        assert!(buf.feed("The user prefers ").is_empty());
        let sentences = buf.feed("dark mode.");
        assert_eq!(sentences, vec!["The user prefers dark mode."]);
    }

    #[test]
    fn finish_flushes_trailing_partial_sentence() {
        let mut buf = SentenceBuffer::new();
        buf.feed("Complete one. trailing fragment without punctuation");
        // pull the completed sentence out via a second feed of nothing
        let _ = buf.feed("");
        let rest = buf.finish();
        assert_eq!(rest, Some("trailing fragment without punctuation".to_string()));
    }

    #[test]
    fn finish_on_empty_buffer_returns_none() {
        let buf = SentenceBuffer::new();
        assert_eq!(buf.finish(), None);
    }

   #[test]
fn feed_is_unicode_safe_and_does_not_panic_on_multibyte_chars() {
    let mut buf = SentenceBuffer::new();
    // Multi-byte chars (emoji, accented text, CJK) around an ASCII boundary
    // must not panic or split mid-codepoint. CJK text here uses the ASCII
    // '!' rather than the fullwidth '！' (U+FF01) — fullwidth punctuation
    // is intentionally out of scope for this milestone's boundary rule
    // (only ASCII '.', '!', '?' are recognized terminators).
    let sentences = buf.feed("café résumé naïve 🎉. 日本語のテスト! remainder");
    assert_eq!(
        sentences,
        vec!["café résumé naïve 🎉.", "日本語のテスト!"]
    );
}

    #[test]
    fn question_and_exclamation_marks_are_also_boundaries() {
        let mut buf = SentenceBuffer::new();
        let sentences = buf.feed("Is this a question? Yes it is! Ok then");
        assert_eq!(sentences, vec!["Is this a question?", "Yes it is!"]);
    }

    #[test]
    fn punctuation_not_followed_by_whitespace_is_not_a_boundary() {
        // e.g. "3.14" — a period mid-token should not split.
        let mut buf = SentenceBuffer::new();
        let sentences = buf.feed("Pi is roughly 3.14 and that's it.");
        assert_eq!(sentences, vec!["Pi is roughly 3.14 and that's it."]);
    }
}