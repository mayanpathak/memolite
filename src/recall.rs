

//! Recall-time constants and helpers that don't depend on anything defined
//! later than Step 0.
//!
//! `RecallQuery`/`RecallItem`/`RecallResult` and `recall_query()` itself
//! are introduced in M4 and will live in this same module.

/// Hard ceiling on how many nearest-neighbor hits `recall_query()` ever
/// pulls from a `VectorStore` in one call.
pub const MAX_CANDIDATES: usize = 500;

/// Calculates the candidate pool requested from the vector store before
/// later filtering narrows the result set.
pub fn candidate_pool_size(limit: usize) -> usize {
    limit.saturating_mul(5).clamp(50, MAX_CANDIDATES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_size_is_bounded_and_monotonic() {
        assert_eq!(candidate_pool_size(0), 50);
        assert_eq!(candidate_pool_size(10), 50);
        assert_eq!(candidate_pool_size(100), 500);
        assert_eq!(candidate_pool_size(1000), MAX_CANDIDATES);
    }
}
