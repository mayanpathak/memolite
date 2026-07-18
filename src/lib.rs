pub mod embedder;
pub mod engine;
pub mod error;
pub mod memory;
pub mod recall;
pub mod vector_store;
pub mod ranking;   // M4

mod migrations;

pub use engine::{BackfillPolicy, MemoryEngine};
pub use error::{MemoliteError, Result};
pub use memory::{Memory, MemoryType};
pub use recall::{RecallItem, RecallQuery, RecallResult};   // M4
pub use vector_store::{InMemoryVectorStore, VectorEntry, VectorHit, VectorStore};

// requests, confidence, streaming, compression, maintenance, and stats are
// registered starting at the milestone that gives each one real content
// (M5, M6, M8, M9, M10, M9.5 respectively) -- not here.