pub mod embedder;
pub mod engine;
pub mod error;
pub mod memory;
pub mod recall;
pub mod vector_store;

mod migrations;

pub use engine::{BackfillPolicy, MemoryEngine};
pub use error::{MemoliteError, Result};
pub use memory::{Memory, MemoryType};
pub use vector_store::{InMemoryVectorStore, VectorEntry, VectorHit, VectorStore};

// ranking, requests, confidence, streaming, compression, maintenance, and
// stats are registered starting at the milestone that gives each one real
// content (M4, M5, M6, M8, M9, M10, M9.5 respectively) -- not here.