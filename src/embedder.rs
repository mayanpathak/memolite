use std::time::Instant;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::error::{MemoliteError, Result};

/// Turns text into a fixed-length embedding vector.
///
/// This wraps a local ONNX model (via `fastembed`) so that generating an
/// embedding never requires an API key or a network call after the model
/// has been downloaded once. Model loading is relatively expensive (it has
/// to read weights off disk and initialize an ONNX runtime session), so an
/// `Embedder` is meant to be constructed **once** — in `MemoryEngine::open()`
/// — and then reused for every `embed()` call, never recreated per call.
pub struct Embedder {
    model: TextEmbedding,
    dimension: usize,
}

impl Embedder {
    /// Loads the local embedding model.
    ///
    /// On the very first run, `fastembed` needs to download the model
    /// weights (~100MB) before this can complete. That download can be slow
    /// on a bad connection, so this logs a message rather than silently
    /// hanging, and returns a proper error instead of panicking if
    /// initialization fails outright.
    pub fn new() -> Result<Self> {
        eprintln!(
            "[memolite] loading embedding model (first run may download ~100MB, please wait)..."
        );

        let start = Instant::now();

        let model = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::AllMiniLML6V2))
            .map_err(|e| MemoliteError::EmbeddingInit(e.to_string()))?;

        eprintln!("[memolite] embedding model ready in {:?}", start.elapsed());

        // AllMiniLML6V2 is a fixed 384-dimensional model. If the model
        // choice ever becomes configurable, this should be derived from the
        // model's own metadata instead of hardcoded.
        let dimension = 384;

        Ok(Self { model, dimension })
    }

    /// Embeds a single piece of text into a `Vec<f32>` of length
    /// [`Embedder::dimension`].
    ///
    /// Returns [`MemoliteError::EmptyEmbeddingInput`] for empty/whitespace-only
    /// text rather than sending it to the model, since an empty embedding is
    /// never meaningful and some backends behave oddly on empty input.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            return Err(MemoliteError::EmptyEmbeddingInput);
        }

        let start = Instant::now();

        let mut embeddings = self
            .model
            .embed(vec![text], None)
            .map_err(|e| MemoliteError::EmbeddingFailed(e.to_string()))?;

        #[cfg(debug_assertions)]
        eprintln!("[memolite] embed() took {:?}", start.elapsed());

        #[cfg(not(debug_assertions))]
        let _ = start;

        embeddings
            .pop()
            .ok_or_else(|| MemoliteError::EmbeddingFailed("model returned no vectors".into()))
    }

    /// The length of every vector this embedder produces.
    pub fn dimension(&self) -> usize {
        self.dimension
    }
}
