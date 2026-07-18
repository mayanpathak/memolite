//! Scoring math for `recall_query()`: recency decay, reinforcement from
//! repeated access, and the final weighted score. Pure functions, no I/O,
//! so they're trivial to unit test in isolation from the engine.

use crate::memory::MemoryType;

/// Half-life, in days, used by [`recency_factor`]'s exponential decay.
/// Shorter for ephemeral types (`Working`), much longer for durable ones
/// (`Procedural`).
pub fn decay_half_life_days(t: MemoryType) -> f64 {
    match t {
        MemoryType::Episodic => 14.0,
        MemoryType::Semantic => 693.0,
        MemoryType::Procedural => 1386.0,
        MemoryType::Working => 0.17,
    }
}

/// Exponential decay factor in `(0.0, 1.0]` based on days since last
/// access. `days_since_access` is clamped to `>= 0.0` so a clock skew or a
/// just-recalled memory never produces a factor `> 1.0`.
pub fn recency_factor(days_since_access: f64, memory_type: MemoryType) -> f32 {
    let days = days_since_access.max(0.0);
    let half_life = decay_half_life_days(memory_type);
    let decay_rate = std::f64::consts::LN_2 / half_life;
    (-decay_rate * days).exp() as f32
}

/// Mild boost for memories accessed more often. Logarithmic so repeated
/// access has diminishing returns rather than dominating the score.
pub fn reinforcement_factor(access_count: u32) -> f32 {
    1.0 + ((1.0 + access_count as f32).ln()) * 0.1
}

/// Combines similarity, importance, recency, reinforcement, and confidence
/// into the single number recall results are ranked by.
pub fn final_score(
    similarity: f32,
    importance: f32,
    recency: f32,
    reinforcement: f32,
    confidence_weight: f32,
) -> f32 {
    similarity * importance * recency * reinforcement * confidence_weight
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recency_factor_is_one_at_zero_days() {
        assert!((recency_factor(0.0, MemoryType::Semantic) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn recency_factor_decays_toward_zero() {
        let near = recency_factor(1.0, MemoryType::Working);
        let far = recency_factor(30.0, MemoryType::Working);
        assert!(far < near);
        assert!(far >= 0.0);
    }

    #[test]
    fn recency_factor_never_exceeds_one_for_negative_days() {
        assert!(recency_factor(-5.0, MemoryType::Episodic) <= 1.0);
    }

    #[test]
    fn slower_decaying_types_retain_more_score_at_equal_age() {
        let episodic = recency_factor(30.0, MemoryType::Episodic);
        let procedural = recency_factor(30.0, MemoryType::Procedural);
        assert!(procedural > episodic);
    }

    #[test]
    fn reinforcement_factor_increases_with_access_count() {
        assert!(reinforcement_factor(10) > reinforcement_factor(0));
    }

    #[test]
    fn reinforcement_factor_is_one_at_zero_accesses() {
        assert!((reinforcement_factor(0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn final_score_is_product_of_factors() {
        let s = final_score(0.5, 0.8, 0.9, 1.1, 1.0);
        assert!((s - (0.5 * 0.8 * 0.9 * 1.1)).abs() < 1e-6);
    }
}