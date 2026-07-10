//! Crate-wide error type.
//!
//! Every fallible operation in `memolite` returns this error type instead of
//! a generic `anyhow::Error`. This makes it possible for callers to `match`
//! on *why* something failed (e.g. "was this a real DB problem, or just
//! corrupted data in one row?") instead of only getting a string message.

use thiserror::Error;

/// Convenience alias so the rest of the crate can write `Result<T>` instead
/// of `Result<T, MemoliteError>` everywhere.
pub type Result<T> = std::result::Result<T, MemoliteError>;

/// The single error type returned by every public `MemoryEngine` method.
#[derive(Debug, Error)]
pub enum MemoliteError {
    /// Something went wrong at the SQLite layer itself (connection, SQL
    /// syntax, constraint violation, etc). `#[from]` means any `?` on a
    /// `rusqlite::Result` automatically converts into this variant.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// A row's `type` column contained a string outside
    /// `('semantic','episodic','procedural','working')`. Should only happen
    /// if the schema and the `MemoryType` enum ever drift apart.
    #[error("invalid memory type in database: {0}")]
    InvalidMemoryType(String),

    /// A row's `metadata` column wasn't valid JSON.
    #[error("invalid metadata JSON: {0}")]
    InvalidMetadata(#[from] serde_json::Error),

    /// A row's `id` or `superseded_by` column wasn't a parseable UUID.
    #[error("invalid uuid: {0}")]
    InvalidUuid(#[from] uuid::Error),

    /// A row's timestamp column held a value that doesn't correspond to a
    /// real, representable `DateTime<Utc>`.
    #[error("invalid timestamp in database: {0}")]
    InvalidTimestamp(i64),

    /// Requested memory could not be found.
    #[error("memory not found: {0}")]
    NotFound(String),

    /// Failed to initialize the embedding model.
    #[error("failed to initialize embedding model: {0}")]
    EmbeddingInit(String),

    /// Failed while generating an embedding.
    #[error("failed to generate embedding: {0}")]
    EmbeddingFailed(String),

    /// Attempted to embed an empty string.
    #[error("cannot embed empty text")]
    EmptyEmbeddingInput,

    /// Failed to serialize an embedding for storage.
    #[error("failed to encode embedding for storage: {0}")]
    EmbeddingEncode(String),

    /// Failed to deserialize an embedding from storage.
    #[error("failed to decode stored embedding: {0}")]
    EmbeddingDecode(String),

    /// Catch-all error for miscellaneous failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}