//! Request/update types for `MemoryEngine`'s storage API.
//!
//! M3 exposed only `store(content, type, importance)`. M5 replaces that as
//! the *only* write path with a richer, extensible request model instead:
//! `StoreRequest` describes everything about a new memory (including
//! optional expiry and metadata), and `MemoryUpdate` describes a partial
//! change to an existing one. Both are pure data -- no I/O, no logic --
//! matching the rest of the crate's pattern of keeping request shapes
//! separate from the engine code that executes them.

use std::collections::HashMap;

use chrono::Duration;
use serde_json::Value;

use crate::memory::MemoryType;

/// Controls how long a newly stored memory lives before
/// `MemoryEngine::purge_expired()` is allowed to delete it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpiryPolicy {
    /// Use `MemoryType::default_ttl()` for whatever `memory_type` is on
    /// the request. This is the exact behavior `store()` always had
    /// before M5 -- `StoreRequest::new(...)` defaults to this.
    TypeDefault,
    /// Expire exactly `Duration` after the memory is created. MUST be a
    /// positive duration -- `MemoryEngine::store_with_options` (and
    /// therefore `store_with_options_id`) rejects `Custom(d)` where
    /// `d <= Duration::zero()` before doing any work.
    Custom(Duration),
    /// The memory never expires (`expires_at` is stored as `NULL`).
    Never,
}

/// A complete description of a memory to store. `StoreRequest::new(...)`
/// gives the same defaults `store()` always used (`ExpiryPolicy::TypeDefault`,
/// empty metadata); everything else is opt-in via the builder methods.
#[derive(Debug, Clone)]
pub struct StoreRequest {
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
    pub expiry: ExpiryPolicy,
    pub metadata: HashMap<String, Value>,
}

impl StoreRequest {
    pub fn new(content: &str, memory_type: MemoryType, importance: f32) -> Self {
        Self {
            content: content.to_string(),
            memory_type,
            importance,
            expiry: ExpiryPolicy::TypeDefault,
            metadata: HashMap::new(),
        }
    }

    pub fn expiry(mut self, expiry: ExpiryPolicy) -> Self {
        self.expiry = expiry;
        self
    }

    pub fn metadata(mut self, metadata: HashMap<String, Value>) -> Self {
        self.metadata = metadata;
        self
    }
}

/// A partial update to an existing memory. Every field is `Option<T>`:
/// `None` means "leave this unchanged." There is no `id` field here --
/// the id being updated is always passed separately as the first argument
/// to `MemoryEngine::update(id, update)`.
///
/// Updating a memory never mutates its row in place: `update()` creates a
/// brand-new memory with the merged fields via `store_with_options_id`,
/// then marks the old row's `superseded_by` to point at the new one. The
/// old row is never deleted or overwritten, so history is preserved.
#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
    pub new_content: Option<String>,
    pub new_importance: Option<f32>,
    pub new_metadata: Option<HashMap<String, Value>>,
    pub new_memory_type: Option<MemoryType>,
    pub new_expiry: Option<ExpiryPolicy>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_request_new_uses_type_default_expiry_and_empty_metadata() {
        let r = StoreRequest::new("hello", MemoryType::Semantic, 0.5);
        assert_eq!(r.expiry, ExpiryPolicy::TypeDefault);
        assert!(r.metadata.is_empty());
    }

    #[test]
    fn store_request_builders_override_defaults() {
        let mut meta = HashMap::new();
        meta.insert("k".to_string(), serde_json::json!("v"));

        let r = StoreRequest::new("hello", MemoryType::Working, 0.9)
            .expiry(ExpiryPolicy::Never)
            .metadata(meta.clone());

        assert_eq!(r.expiry, ExpiryPolicy::Never);
        assert_eq!(r.metadata, meta);
    }

    #[test]
    fn memory_update_default_is_all_none() {
        let u = MemoryUpdate::default();
        assert!(u.new_content.is_none());
        assert!(u.new_importance.is_none());
        assert!(u.new_metadata.is_none());
        assert!(u.new_memory_type.is_none());
        assert!(u.new_expiry.is_none());
    }
}