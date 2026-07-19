//! Confidence scoring for memories (M6).
//!
//! Every memory carries a `ConfidenceLevel` describing how it came to be
//! trusted:
//!
//! - `Explicit`: directly stated by the user/caller. Full ranking weight.
//! - `Inferred`: concluded indirectly (e.g. by an `update()` call that
//!   didn't specify a confidence level). Reduced ranking weight until
//!   reinforced.
//! - `Reinforced`: an `Inferred` memory that has been recalled enough times
//!   (`access_count >= PROMOTION_THRESHOLD`) to be promoted back to full
//!   ranking weight.
//!
//! This module is intentionally pure data + pure functions -- no I/O, no
//! locking -- matching the rest of the crate's separation between data
//! shapes (`memory.rs`, `requests.rs`, this file) and the engine code
//! (`engine.rs`) that persists and scores them.

use serde::{Deserialize, Serialize};

use crate::error::{MemoliteError, Result};

/// How a memory came to be trusted, and how much weight it gets in ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceLevel {
    /// Directly stated by a user or trusted caller.
    Explicit,
    /// Concluded indirectly; ranking weight is reduced until reinforced.
    Inferred,
    /// An `Inferred` memory that has been recalled enough times to earn
    /// full ranking weight back.
    Reinforced,
}

impl ConfidenceLevel {
    /// The `access_count` an `Inferred` memory must reach to promote to
    /// `Reinforced`.
    ///
    /// Kept as one named constant so this module's pure-Rust
    /// [`ConfidenceLevel::maybe_promote`] and `engine.rs`'s SQL `CASE`
    /// expression (used when bumping access stats after a recall) can
    /// never silently drift apart -- both read from this single value
    /// instead of each hardcoding the number `5` independently.
    pub const PROMOTION_THRESHOLD: u32 = 5;

    /// Converts to the string form stored in SQLite's `confidence` column
    /// (see the `CHECK(confidence IN (...))` constraint added by
    /// migration 2 in `migrations.rs`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfidenceLevel::Explicit => "explicit",
            ConfidenceLevel::Inferred => "inferred",
            ConfidenceLevel::Reinforced => "reinforced",
        }
    }

    /// Parses a `ConfidenceLevel` back out of the string stored in SQLite.
    ///
    /// Inverse of [`ConfidenceLevel::as_str`]. Returns
    /// `MemoliteError::InvalidConfidence` instead of panicking if a row
    /// somehow contains a value outside the `CHECK` constraint (e.g. the
    /// on-disk file was hand-edited or corrupted).
    ///
    /// Named `parse_str` rather than `from_str`, matching
    /// `MemoryType::parse_str` elsewhere in the crate -- this avoids
    /// Clippy's `should_implement_trait` lint, since this isn't meant to
    /// back a `.parse::<ConfidenceLevel>()` call via `std::str::FromStr`.
    pub fn parse_str(s: &str) -> Result<Self> {
        match s {
            "explicit" => Ok(ConfidenceLevel::Explicit),
            "inferred" => Ok(ConfidenceLevel::Inferred),
            "reinforced" => Ok(ConfidenceLevel::Reinforced),
            other => Err(MemoliteError::InvalidConfidence(other.to_string())),
        }
    }

    /// The ranking weight this confidence level contributes to
    /// `ranking::final_score`. `Explicit` and `Reinforced` are both fully
    /// trusted (`1.0`); `Inferred` is discounted to `0.7` until it earns
    /// reinforcement through repeated recall.
    pub fn weight(&self) -> f32 {
        match self {
            ConfidenceLevel::Explicit | ConfidenceLevel::Reinforced => 1.0,
            ConfidenceLevel::Inferred => 0.7,
        }
    }

    /// Returns `Reinforced` if `self` is `Inferred` and `access_count` has
    /// reached [`ConfidenceLevel::PROMOTION_THRESHOLD`]; otherwise returns
    /// `self` unchanged. `Explicit` is never promoted (it's already at
    /// full weight) and `Reinforced` never demotes.
    ///
    /// This is the pure-Rust mirror of the SQL `CASE` expression
    /// `MemoryEngine::recall_query`'s access-stats bump runs directly in
    /// the database. It exists so the promotion rule is unit-testable
    /// without a database, and so any future in-Rust confidence
    /// recomputation (e.g. an offline maintenance pass) has one obvious,
    /// already-correct place to call instead of re-deriving the rule.
    pub fn maybe_promote(self, access_count: u32) -> Self {
        if self == ConfidenceLevel::Inferred && access_count >= Self::PROMOTION_THRESHOLD {
            ConfidenceLevel::Reinforced
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_round_trips_through_parse_str() {
        for level in [
            ConfidenceLevel::Explicit,
            ConfidenceLevel::Inferred,
            ConfidenceLevel::Reinforced,
        ] {
            assert_eq!(ConfidenceLevel::parse_str(level.as_str()).unwrap(), level);
        }
    }

    #[test]
    fn parse_str_rejects_unknown_values() {
        assert!(matches!(
            ConfidenceLevel::parse_str("maybe"),
            Err(MemoliteError::InvalidConfidence(_))
        ));
    }

    #[test]
    fn explicit_and_reinforced_have_full_weight() {
        assert_eq!(ConfidenceLevel::Explicit.weight(), 1.0);
        assert_eq!(ConfidenceLevel::Reinforced.weight(), 1.0);
    }

    #[test]
    fn inferred_has_reduced_weight() {
        assert_eq!(ConfidenceLevel::Inferred.weight(), 0.7);
    }

    #[test]
    fn inferred_promotes_at_threshold_not_before() {
        assert_eq!(
            ConfidenceLevel::Inferred.maybe_promote(ConfidenceLevel::PROMOTION_THRESHOLD - 1),
            ConfidenceLevel::Inferred
        );
        assert_eq!(
            ConfidenceLevel::Inferred.maybe_promote(ConfidenceLevel::PROMOTION_THRESHOLD),
            ConfidenceLevel::Reinforced
        );
        assert_eq!(
            ConfidenceLevel::Inferred.maybe_promote(1000),
            ConfidenceLevel::Reinforced
        );
    }

    #[test]
    fn explicit_is_never_promoted() {
        assert_eq!(
            ConfidenceLevel::Explicit.maybe_promote(1000),
            ConfidenceLevel::Explicit
        );
    }

    #[test]
    fn reinforced_stays_reinforced() {
        assert_eq!(
            ConfidenceLevel::Reinforced.maybe_promote(0),
            ConfidenceLevel::Reinforced
        );
    }
}