#![deny(clippy::await_holding_lock)]

pub mod confidence; // M6
pub mod embedder;
pub mod engine;
pub mod error;
pub mod memory;
pub mod ranking; // M4
pub mod recall;
pub mod requests; // M5
pub mod vector_store;
pub mod streaming; // M8


mod migrations;

pub use confidence::ConfidenceLevel; // M6
pub use engine::{BackfillPolicy, MemoryEngine};
pub use error::{MemoliteError, Result};
pub use memory::{Memory, MemoryType};
pub use recall::{RecallItem, RecallQuery, RecallResult}; // M4, extended with temporal fields in M7
pub use requests::{ExpiryPolicy, MemoryUpdate, StoreRequest}; // M5
pub use vector_store::{InMemoryVectorStore, VectorEntry, VectorHit, VectorStore};
pub use streaming::{IngestChunk, IngestFailure, IngestReport, IngestorSender, StreamIngestor, SentenceBuffer}; // M8
