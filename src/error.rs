//! Crate-wide error type.
//!
//! Every fallible operation in `memolite` returns this error type instead of
//! a generic `anyhow::Error`. This makes it possible for callers to `match`
//! on *why* something failed (e.g. "was this a real DB problem, or just
//! corrupted data in one row?") instead of only getting a string message.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, MemoliteError>;

#[derive(Debug, Error)]
pub enum MemoliteError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("invalid memory type in database: {0}")]
    InvalidMemoryType(String),

    #[error("invalid metadata JSON: {0}")]
    InvalidMetadata(#[from] serde_json::Error),

    #[error("invalid uuid: {0}")]
    InvalidUuid(#[from] uuid::Error),

    #[error("invalid timestamp in database: {0}")]
    InvalidTimestamp(i64),

    #[error("memory not found: {0}")]
    NotFound(String),

    #[error("failed to initialize embedding model: {0}")]
    EmbeddingInit(String),

    #[error("failed to generate embedding: {0}")]
    EmbeddingFailed(String),

    #[error("cannot embed empty text")]
    EmptyEmbeddingInput,

    #[error("failed to encode embedding for storage: {0}")]
    EmbeddingEncode(String),

    #[error("failed to decode stored embedding: {0}")]
    EmbeddingDecode(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("vector store error: {0}")]
    VectorStore(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("data invariant violated: {0}")]
    Corruption(String),

    #[error("operation failed: {operation}; compensation also failed: {compensation}")]
    CompensationFailed {
        operation: String,
        compensation: String,
    },

    #[error("invalid confidence value: {0}")]
    InvalidConfidence(String),
}