use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::memory::{Memory, MemoryType};

pub const MAX_CANDIDATES: usize = 500;

pub const DEFAULT_RECALL_LIMIT: usize = 10;

pub fn candidate_pool_size(limit: usize) -> usize {
    limit.saturating_mul(5).clamp(50, MAX_CANDIDATES)
}

#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub query_text: String,
    pub limit: usize,
    pub min_importance: f32,
    pub memory_types: Option<Vec<MemoryType>>,
    pub include_superseded: bool,
    pub include_expired: bool,
    pub metadata_equals: HashMap<String, Value>,

    // M7 — temporal querying.
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub only_stale: bool,
}

impl RecallQuery {
    pub fn new(query_text: &str) -> Self {
        Self {
            query_text: query_text.to_string(),
            limit: DEFAULT_RECALL_LIMIT,
            min_importance: 0.0,
            memory_types: None,
            include_superseded: false,
            include_expired: false,
            metadata_equals: HashMap::new(),
            created_after: None,
            created_before: None,
            only_stale: false,
        }
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = n;
        self
    }

    pub fn min_importance(mut self, v: f32) -> Self {
        self.min_importance = v;
        self
    }

    pub fn memory_types(mut self, t: Vec<MemoryType>) -> Self {
        self.memory_types = Some(t);
        self
    }

    pub fn include_superseded(mut self, b: bool) -> Self {
        self.include_superseded = b;
        self
    }

    pub fn include_expired(mut self, b: bool) -> Self {
        self.include_expired = b;
        self
    }

    pub fn metadata_equals(mut self, k: &str, v: Value) -> Self {
        self.metadata_equals.insert(k.to_string(), v);
        self
    }

    /// Only include memories created at or after `t`. Combined with
    /// `.created_before(...)`, forms an inclusive `[after, before]`
    /// window. An inverted window (`after > before`) is rejected by
    /// `MemoryEngine::recall_query` with `InvalidArgument`, not silently
    /// treated as "no results".
    pub fn created_after(mut self, t: DateTime<Utc>) -> Self {
        self.created_after = Some(t);
        self
    }

    /// Only include memories created at or before `t`. See
    /// `.created_after(...)`.
    pub fn created_before(mut self, t: DateTime<Utc>) -> Self {
        self.created_before = Some(t);
        self
    }

    /// When `true`, only include memories that haven't been accessed in
    /// at least twice their memory type's decay half-life — the same
    /// definition of "stale" used by `MemoryEngine::find_stale_memories`.
    /// Defaults to `false` (no staleness filtering).
    pub fn only_stale(mut self, b: bool) -> Self {
        self.only_stale = b;
        self
    }
}

#[derive(Debug, Clone)]
pub struct RecallItem {
    pub memory: Memory,
    pub similarity: f32,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct RecallResult {
    pub items: Vec<RecallItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_size_is_bounded_and_monotonic() {
        assert_eq!(candidate_pool_size(0), 50);
        assert_eq!(candidate_pool_size(1), 50);
        assert_eq!(candidate_pool_size(10), 50);
        assert_eq!(candidate_pool_size(20), 100);
        assert_eq!(candidate_pool_size(100), 500);
        assert_eq!(candidate_pool_size(1000), MAX_CANDIDATES);

        assert!(candidate_pool_size(20) >= candidate_pool_size(10));
        assert!(candidate_pool_size(1000) >= candidate_pool_size(100));
    }

    #[test]
    fn recall_query_defaults_match_the_documented_contract() {
        let q = RecallQuery::new("hello");

        assert_eq!(q.query_text, "hello");
        assert_eq!(q.limit, DEFAULT_RECALL_LIMIT);
        assert_eq!(q.min_importance, 0.0);
        assert!(q.memory_types.is_none());
        assert!(!q.include_superseded);
        assert!(!q.include_expired);
        assert!(q.metadata_equals.is_empty());
        assert!(q.created_after.is_none());
        assert!(q.created_before.is_none());
        assert!(!q.only_stale);
    }

    #[test]
    fn builder_methods_set_the_expected_fields() {
        let now = Utc::now();
        let earlier = now - chrono::Duration::days(1);

        let q = RecallQuery::new("hello")
            .limit(5)
            .min_importance(0.3)
            .memory_types(vec![
                MemoryType::Semantic,
                MemoryType::Episodic,
            ])
            .include_superseded(true)
            .include_expired(true)
            .metadata_equals("project", Value::String("memolite".into()))
            .created_after(earlier)
            .created_before(now)
            .only_stale(true);

        assert_eq!(q.limit, 5);
        assert_eq!(q.min_importance, 0.3);
        assert_eq!(
            q.memory_types,
            Some(vec![
                MemoryType::Semantic,
                MemoryType::Episodic,
            ])
        );
        assert!(q.include_superseded);
        assert!(q.include_expired);

        assert_eq!(
            q.metadata_equals.get("project"),
            Some(&Value::String("memolite".into()))
        );

        assert_eq!(q.created_after, Some(earlier));
        assert_eq!(q.created_before, Some(now));
        assert!(q.only_stale);
    }

    #[test]
    fn created_after_and_created_before_are_independently_settable() {
        let now = Utc::now();

        let only_after = RecallQuery::new("x").created_after(now);
        assert_eq!(only_after.created_after, Some(now));
        assert!(only_after.created_before.is_none());

        let only_before = RecallQuery::new("x").created_before(now);
        assert!(only_before.created_after.is_none());
        assert_eq!(only_before.created_before, Some(now));
    }

    #[test]
    fn only_stale_defaults_to_false_and_is_settable_both_ways() {
        assert!(!RecallQuery::new("x").only_stale);
        assert!(RecallQuery::new("x").only_stale(true).only_stale);
        assert!(!RecallQuery::new("x")
            .only_stale(true)
            .only_stale(false)
            .only_stale);
    }
}