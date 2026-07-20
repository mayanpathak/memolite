use std::time::Instant;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::error::{MemoliteError, Result};

pub struct Embedder {
    model: TextEmbedding,
    dimension: usize,
}

impl Embedder {
    pub fn new() -> Result<Self> {
        eprintln!(
            "[memolite] loading embedding model (first run may download ~100MB, please wait)..."
        );

        let start = Instant::now();

        let model = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::AllMiniLML6V2))
            .map_err(|e| MemoliteError::EmbeddingInit(e.to_string()))?;

        eprintln!("[memolite] embedding model ready in {:?}", start.elapsed());

        let dimension = 384;

        Ok(Self { model, dimension })
    }

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

    pub fn dimension(&self) -> usize {
        self.dimension
    }
}