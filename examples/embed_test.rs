use anyhow::Result;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());

    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    dot / (norm_a.sqrt() * norm_b.sqrt())
}

fn main() -> Result<()> {
    // Downloads the model once (first run only).
    // Afterwards it loads from the local cache.
    let mut model = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::AllMiniLML6V2))?;

    let texts = vec![
        "I love Rust",
        "Rust is my favorite programming language",
        "The weather is sunny today",
        "I enjoy eating pizza",
    ];

    let embeddings = model.embed(texts.clone(), None)?;

    // ---------------------------------------------------------
    // Print embedding for "I love Rust"
    // ---------------------------------------------------------
    println!("Text: {}", texts[0]);
    println!("Embedding length: {}", embeddings[0].len());
    println!("Embedding:\n{:?}", embeddings[0]);

    println!("\n----------------------------------------");

    // Cosine similarities
    let rust_pair = cosine_similarity(&embeddings[0], &embeddings[1]);

    let rust_weather = cosine_similarity(&embeddings[0], &embeddings[2]);

    let rust_pizza = cosine_similarity(&embeddings[0], &embeddings[3]);

    let weather_pizza = cosine_similarity(&embeddings[2], &embeddings[3]);

    println!("Cosine Similarities");
    println!("===================");
    println!("Rust vs Rust-related : {:.4}", rust_pair);
    println!("Rust vs Weather      : {:.4}", rust_weather);
    println!("Rust vs Pizza        : {:.4}", rust_pizza);
    println!("Weather vs Pizza     : {:.4}", weather_pizza);

    println!("\nExpected:");
    println!("  Rust vs Rust-related should be the highest.");
    println!("  The unrelated pairs should be noticeably lower.");

    Ok(())
}
