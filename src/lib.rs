//! `memolite` -- a SQLite-backed semantic memory engine with pluggable
//! vector search.
//!
//! Module registration follows one rule throughout this crate's build:
//! a module is only ever added to this file in the milestone that gives
//! it real content. As of M6, that's `embedder`, `engine`, `error`,
//! `memory`, `recall`, `vector_store` (Step 0), `ranking` (M4),
//! `requests` (M5), and `confidence` (M6). `streaming`, `compression`,
//! `maintenance`, and `stats` are registered starting at M8/M9/M10/M9.5
//! respectively -- not here, since none of them have content yet.

pub mod confidence; // M6
pub mod embedder;
pub mod engine;
pub mod error;
pub mod memory;
pub mod ranking; // M4
pub mod recall;
pub mod requests; // M5
pub mod vector_store;

mod migrations;

pub use confidence::ConfidenceLevel; // M6
pub use engine::{BackfillPolicy, MemoryEngine};
pub use error::{MemoliteError, Result};
pub use memory::{Memory, MemoryType};
pub use recall::{RecallItem, RecallQuery, RecallResult}; // M4
pub use requests::{ExpiryPolicy, MemoryUpdate, StoreRequest}; // M5
pub use vector_store::{InMemoryVectorStore, VectorEntry, VectorHit, VectorStore};

// streaming, compression, maintenance, and stats are registered starting
// at the milestone that gives each one real content (M8, M9, M10, M9.5
// respectively) -- not here.