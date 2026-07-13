// // //! Recall-time constants and helpers that don't depend on anything defined
// // //! later than Step 0.
// // //!
// // //! `RecallQuery`/`RecallItem`/`RecallResult` and `recall_query()` itself
// // //! are introduced in M4 and will live in this same module -- but nothing
// // //! here references them, so this file is safe to write now.

// // /// Hard ceiling on how many nearest-neighbor hits `recall_query()` ever
// // /// pulls from a `VectorStore` in one call, regardless of how large the
// // /// requested `limit` is.
// // pub const MAX_CANDIDATES: usize = 500;

// // /// How many candidates to request from `VectorStore::search` for a given
// // /// `limit`, before post-search filtering (importance/type/metadata/etc.)
// // /// narrows it down. Requesting more than `limit` up front means filtering
// // /// out a few low-quality matches still leaves enough to fill `limit`.
// // pub fn candidate_pool_size(limit: usize) -> usize {
// //     limit.saturating_mul(5).max(50).min(MAX_CANDIDATES)
// // }

// // #[cfg(test)]
// // mod tests {
// //     use super::*;

// //     #[test]
// //     fn pool_size_is_bounded_and_monotonic() {
// //         assert_eq!(candidate_pool_size(0), 50);
// //         assert_eq!(candidate_pool_size(10), 50);
// //         assert_eq!(candidate_pool_size(100), 500);
// //         assert_eq!(candidate_pool_size(1000), MAX_CANDIDATES);
// //     }
// // }

// //! Recall-time constants and helpers that don't depend on anything defined
// //! later than Step 0.
// //!
// //! `RecallQuery`/`RecallItem`/`RecallResult` and `recall_query()` itself
// //! are introduced in M4 and will live in this same module -- but nothing
// //! here references them, so this file is safe to write now.

// /// Hard ceiling on how many nearest-neighbor hits `recall_query()` ever
// /// pulls from a `VectorStore` in one call, regardless of how large the
// /// requested `limit` is.
// pub const MAX_CANDIDATES: usize = 500;

// /// How many candidates to request from `VectorStore::search` for a given
// /// `limit`, before post-search filtering (importance/type/metadata/etc.)
// /// narrows it down. Requesting more than `limit` up front means filtering
// /// out a few low-quality matches still leaves enough to fill `limit`.
// pub fn candidate_pool_size(limit: usize) -> usize {
//     limit.saturating_mul(5).clamp(50, MAX_CANDIDATES)
// }

// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[test]
//     fn pool_size_is_bounded_and_monotonic() {
//         assert_eq!(candidate_pool_size(0), 50);
//         assert_eq!(candidate_pool_size(10), 50);
//         assert_eq!(candidate_pool_size(100), 500);
//         assert_eq!(candidate_pool_size(1000), MAX_CANDIDATES);
//     }
// }

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
