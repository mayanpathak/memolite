use std::collections::HashMap;

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
        assert_eq!(candidate_pool_size(10), 50);
        assert_eq!(candidate_pool_size(100), 500);
        assert_eq!(candidate_pool_size(1000), MAX_CANDIDATES);
    }

    #[test]
    fn recall_query_defaults_match_the_documented_contract() {
        let q = RecallQuery::new("hello");
        assert_eq!(q.limit, DEFAULT_RECALL_LIMIT);
        assert_eq!(q.min_importance, 0.0);
        assert!(q.memory_types.is_none());
        assert!(!q.include_superseded);
        assert!(!q.include_expired);
        assert!(q.metadata_equals.is_empty());
    }

    #[test]
    fn builder_methods_set_the_expected_fields() {
        let q = RecallQuery::new("hello")
            .limit(3)
            .min_importance(0.4)
            .memory_types(vec![MemoryType::Semantic])
            .include_superseded(true)
            .include_expired(true)
            .metadata_equals("project", serde_json::json!("memolite"));

        assert_eq!(q.limit, 3);
        assert_eq!(q.min_importance, 0.4);
        assert_eq!(q.memory_types, Some(vec![MemoryType::Semantic]));
        assert!(q.include_superseded);
        assert!(q.include_expired);
        assert_eq!(
            q.metadata_equals.get("project"),
            Some(&serde_json::json!("memolite"))
        );
    }
}