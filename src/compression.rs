//! M9 — episodic memory compression + vector-index rebuild.
//!
//! Compression consolidates old, low-importance episodic memories into a
//! single semantic summary, reducing active clutter while preserving the
//! gist. Originals are never deleted — they are marked `superseded_by`
//! the new summary, exactly like `MemoryEngine::update()` does, so full
//! history remains recoverable via `include_superseded(true)`.
//!
//! Everything in this module is a pure, engine-independent helper:
//! eligibility, clustering, and summarization. The engine-side
//! orchestration (`compress_old_memories`, `rebuild_vector_index`,
//! `mark_all_superseded`) lives in `engine.rs` because it needs the
//! database connection, the embedder, and the vector store.

use uuid::Uuid;

use crate::error::{MemoliteError, Result};
use crate::memory::{Memory, MemoryType};

/// Bumped whenever the summarization algorithm changes in a way that
/// would make old summaries' `compression.algorithm_version` metadata
/// stop matching what a fresh run would produce. Not currently read by
/// any code path — it's recorded on every summary purely as a forward
/// compatibility breadcrumb.
pub const COMPRESSION_ALGORITHM_VERSION: u32 = 1;

/// Maximum length, in **characters** (not bytes), of a generated
/// extractive summary. Truncation always happens on a `char` boundary
/// (see `summarize_cluster`), so this is safe for any UTF-8 content.
pub const MAX_SUMMARY_CHARS: usize = 2000;

/// One group of memories whose embeddings were judged similar enough
/// (by `greedy_cluster`) to be summarized together.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub member_ids: Vec<Uuid>,
}

/// The result of summarizing one `Cluster`'s member `Memory` rows.
#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub summary_content: String,
    pub original_ids: Vec<Uuid>,
}

/// Returns `true` if `mem` is eligible for compression:
/// - `memory_type == Episodic`
/// - `created_at` is more than 14 days ago
/// - `importance < 0.3`
/// - not already superseded
/// - not expired (`expires_at` is `None`, or `>=` now)
pub fn is_compression_eligible(mem: &Memory) -> bool {
    let now = chrono::Utc::now();
    let age_days = (now - mem.created_at).num_days();
    let not_expired = mem.expires_at.map(|e| e >= now).unwrap_or(true);

    mem.memory_type == MemoryType::Episodic
        && age_days > 14
        && mem.importance < 0.3
        && mem.superseded_by.is_none()
        && not_expired
}

/// Cosine similarity between two equal-length vectors. Returns `0.0` if
/// either vector has zero norm (rather than dividing by zero).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Greedy single-linkage clustering over `vectors`. For each
/// not-yet-assigned entry, starts a new cluster anchored on that entry
/// and pulls in every remaining unassigned entry whose cosine similarity
/// with the anchor is `>= threshold`.
///
/// Intentionally O(n^2) and intentionally simple: compression only ever
/// runs over a small, infrequent candidate set (episodic memories older
/// than 14 days), so a proper ANN/clustering algorithm would be
/// over-engineering for this milestone. This is documented, not a bug.
pub fn greedy_cluster(vectors: &[(Uuid, Vec<f32>)], threshold: f32) -> Vec<Cluster> {
    let mut assigned = vec![false; vectors.len()];
    let mut clusters = Vec::new();

    for i in 0..vectors.len() {
        if assigned[i] {
            continue;
        }
        assigned[i] = true;
        let mut member_ids = vec![vectors[i].0];

        for j in (i + 1)..vectors.len() {
            if assigned[j] {
                continue;
            }
            if cosine(&vectors[i].1, &vectors[j].1) >= threshold {
                assigned[j] = true;
                member_ids.push(vectors[j].0);
            }
        }

        clusters.push(Cluster { member_ids });
    }

    clusters
}

/// Builds an extractive summary from `members`: each member's content is
/// concatenated in order, separated by `" | "`, then truncated to at
/// most `MAX_SUMMARY_CHARS` **characters** (never bytes, so this can
/// never panic on a multi-byte UTF-8 boundary).
///
/// `threshold` is accepted but not currently used inside the function
/// body — it's kept in the signature so a future, smarter summarizer can
/// use it (e.g. to decide how much overlap to dedupe) without changing
/// every call site.
pub fn summarize_cluster(members: &[Memory], _threshold: f32) -> Result<CompressionResult> {
    if members.is_empty() {
        return Err(MemoliteError::InvalidArgument(
            "cannot summarize an empty cluster".into(),
        ));
    }

    let mut summary = String::new();
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            summary.push_str(" | ");
        }
        summary.push_str(&m.content);
    }

    // Character-aware truncation — `MAX_SUMMARY_CHARS` counts chars, and
    // `.chars().take(n).collect()` can never land mid-codepoint the way
    // `String::truncate(n)` (a byte index) could.
    let summary_content: String = summary.chars().take(MAX_SUMMARY_CHARS).collect();

    Ok(CompressionResult {
        summary_content,
        original_ids: members.iter().map(|m| m.id).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::ConfidenceLevel;
    use std::collections::HashMap;

    fn fixture_memory(
        memory_type: MemoryType,
        importance: f32,
        age_days: i64,
        superseded: bool,
        expired: bool,
    ) -> Memory {
        let now = chrono::Utc::now();
        Memory {
            id: Uuid::new_v4(),
            content: "some episodic content".to_string(),
            memory_type,
            importance,
            access_count: 0,
            created_at: now - chrono::Duration::days(age_days),
            last_accessed: now,
            expires_at: if expired {
                Some(now - chrono::Duration::days(1))
            } else {
                None
            },
            superseded_by: if superseded { Some(Uuid::new_v4()) } else { None },
            metadata: HashMap::new(),
            confidence: ConfidenceLevel::Explicit,
        }
    }

    #[test]
    fn eligible_memory_passes_all_checks() {
        let mem = fixture_memory(MemoryType::Episodic, 0.1, 20, false, false);
        assert!(is_compression_eligible(&mem));
    }

    #[test]
    fn wrong_type_is_not_eligible() {
        let mem = fixture_memory(MemoryType::Semantic, 0.1, 20, false, false);
        assert!(!is_compression_eligible(&mem));
    }

    #[test]
    fn too_important_is_not_eligible() {
        let mem = fixture_memory(MemoryType::Episodic, 0.5, 20, false, false);
        assert!(!is_compression_eligible(&mem));
    }

    #[test]
    fn too_young_is_not_eligible() {
        let mem = fixture_memory(MemoryType::Episodic, 0.1, 5, false, false);
        assert!(!is_compression_eligible(&mem));
    }

    #[test]
    fn already_superseded_is_not_eligible() {
        let mem = fixture_memory(MemoryType::Episodic, 0.1, 20, true, false);
        assert!(!is_compression_eligible(&mem));
    }

    #[test]
    fn expired_is_not_eligible() {
        let mem = fixture_memory(MemoryType::Episodic, 0.1, 20, false, true);
        assert!(!is_compression_eligible(&mem));
    }

    #[test]
    fn three_close_vectors_cluster_together_one_far_is_separate() {
        let a = (Uuid::new_v4(), vec![1.0, 0.0, 0.0]);
        let b = (Uuid::new_v4(), vec![0.99, 0.01, 0.0]);
        let c = (Uuid::new_v4(), vec![0.98, 0.02, 0.0]);
        let d = (Uuid::new_v4(), vec![0.0, 0.0, 1.0]);

        let clusters = greedy_cluster(&[a.clone(), b.clone(), c.clone(), d.clone()], 0.85);

        let big = clusters.iter().find(|c| c.member_ids.len() == 3);
        assert!(big.is_some(), "expected one cluster of size 3");
        let big = big.unwrap();
        assert!(big.member_ids.contains(&a.0));
        assert!(big.member_ids.contains(&b.0));
        assert!(big.member_ids.contains(&c.0));

        let small = clusters.iter().find(|c| c.member_ids.len() == 1).unwrap();
        assert_eq!(small.member_ids[0], d.0);
    }

    #[test]
    fn summarize_empty_cluster_is_an_error() {
        assert!(summarize_cluster(&[], 0.85).is_err());
    }

    #[test]
    fn summarize_joins_content_with_pipe_separator() {
        let m1 = fixture_memory(MemoryType::Episodic, 0.1, 20, false, false);
        let m2 = fixture_memory(MemoryType::Episodic, 0.1, 20, false, false);
        let result = summarize_cluster(&[m1.clone(), m2.clone()], 0.85).unwrap();
        assert!(result.summary_content.contains(" | "));
        assert_eq!(result.original_ids, vec![m1.id, m2.id]);
    }

    #[test]
    fn summarize_truncates_to_max_chars_on_a_char_boundary() {
        // Multi-byte content so a byte-index truncate would risk a panic
        // if this weren't char-aware.
        let mut mem = fixture_memory(MemoryType::Episodic, 0.1, 20, false, false);
        mem.content = "日".repeat(MAX_SUMMARY_CHARS + 500);
        let result = summarize_cluster(&[mem], 0.85).unwrap();
        assert_eq!(result.summary_content.chars().count(), MAX_SUMMARY_CHARS);
    }
}