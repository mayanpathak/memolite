use serde::{Deserialize, Serialize};

use crate::error::{MemoliteError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceLevel {
    Explicit,
    Inferred,
    Reinforced,
}

impl ConfidenceLevel {
    pub const PROMOTION_THRESHOLD: u32 = 5;

    pub fn as_str(&self) -> &'static str {
        match self {
            ConfidenceLevel::Explicit => "explicit",
            ConfidenceLevel::Inferred => "inferred",
            ConfidenceLevel::Reinforced => "reinforced",
        }
    }

    pub fn parse_str(s: &str) -> Result<Self> {
        match s {
            "explicit" => Ok(ConfidenceLevel::Explicit),
            "inferred" => Ok(ConfidenceLevel::Inferred),
            "reinforced" => Ok(ConfidenceLevel::Reinforced),
            other => Err(MemoliteError::InvalidConfidence(other.to_string())),
        }
    }

    pub fn weight(&self) -> f32 {
        match self {
            ConfidenceLevel::Explicit | ConfidenceLevel::Reinforced => 1.0,
            ConfidenceLevel::Inferred => 0.7,
        }
    }

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