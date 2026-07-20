use std::collections::HashMap;

use chrono::Duration;
use serde_json::Value;

use crate::confidence::ConfidenceLevel;
use crate::memory::MemoryType;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpiryPolicy {
TypeDefault,
Custom(Duration),
Never,
}

#[derive(Debug, Clone)]
pub struct StoreRequest {
pub content: String,
pub memory_type: MemoryType,
pub importance: f32,
pub expiry: ExpiryPolicy,
pub metadata: HashMap<String, Value>,
pub confidence: ConfidenceLevel,
}

impl StoreRequest {
pub fn new(content: &str, memory_type: MemoryType, importance: f32) -> Self {
Self {
content: content.to_string(),
memory_type,
importance,
expiry: ExpiryPolicy::TypeDefault,
metadata: HashMap::new(),
confidence: ConfidenceLevel::Explicit,
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

pub fn with_confidence(mut self, confidence: ConfidenceLevel) -> Self {
    self.confidence = confidence;
    self
}

}

#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
pub new_content: Option<String>,
pub new_importance: Option<f32>,
pub new_metadata: Option<HashMap<String, Value>>,
pub new_memory_type: Option<MemoryType>,
pub new_expiry: Option<ExpiryPolicy>,
pub new_confidence: Option<ConfidenceLevel>,
}

#[cfg(test)]
mod tests {
use super::*;

#[test]
fn store_request_new_uses_type_default_expiry_empty_metadata_and_explicit_confidence() {
    let r = StoreRequest::new("hello", MemoryType::Semantic, 0.5);
    assert_eq!(r.expiry, ExpiryPolicy::TypeDefault);
    assert!(r.metadata.is_empty());
    assert_eq!(r.confidence, ConfidenceLevel::Explicit);
}

#[test]
fn store_request_builders_override_defaults() {
    let mut meta = HashMap::new();
    meta.insert("k".to_string(), serde_json::json!("v"));

    let r = StoreRequest::new("hello", MemoryType::Working, 0.9)
        .expiry(ExpiryPolicy::Never)
        .metadata(meta.clone())
        .with_confidence(ConfidenceLevel::Inferred);

    assert_eq!(r.expiry, ExpiryPolicy::Never);
    assert_eq!(r.metadata, meta);
    assert_eq!(r.confidence, ConfidenceLevel::Inferred);
}

#[test]
fn memory_update_default_is_all_none() {
    let u = MemoryUpdate::default();
    assert!(u.new_content.is_none());
    assert!(u.new_importance.is_none());
    assert!(u.new_metadata.is_none());
    assert!(u.new_memory_type.is_none());
    assert!(u.new_expiry.is_none());
    assert!(u.new_confidence.is_none());
}

}