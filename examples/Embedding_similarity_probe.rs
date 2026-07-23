//! Quick, standalone probe for calibrating `compress_old_memories`'s
//! clustering threshold (currently hard-coded at `0.85` in
//! `MemoryEngine::compress_old_memories`) against your *actual* embedder.
//!
//! Run it with:
//!
//!     cargo run --example embedding_similarity_probe
//!
//! It embeds a handful of representative sentence-pair categories with
//! the real `Embedder` (the same one `MemoryEngine` uses) and prints the
//! cosine similarity for each pair, grouped by category, plus a min/max
//! summary per category at the end.
//!
//! Read the output like this:
//! - "Near-templated" pairs (same structure, one word swapped) are the
//!   shape of memory that reliably clusters today. If these come out
//!   noticeably below 0.85, something about the embedder or its config
//!   has changed and the whole threshold needs revisiting.
//! - "Genuine paraphrase" pairs (same fact, different wording/structure)
//!   are the shape of memory `compress_old_memories` is *supposed* to
//!   catch in practice — this is the category that actually determines
//!   whether 0.85 is too strict.
//! - "Unrelated" pairs are a sanity floor — if these score anywhere near
//!   0.85, the threshold is too loose and compression risks merging
//!   memories that aren't actually redundant.
//!
//! A reasonable threshold sits above the top of "Unrelated" and below
//! the bottom of "Genuine paraphrase" (or as close to that as the two
//! ranges allow — they may overlap, in which case there's an inherent
//! trade-off to make explicitly rather than leave implicit in a
//! hard-coded `0.85`).

use memolite::error::Result;

// `Embedder` isn't re-exported at the crate root (only `MemoryEngine`
// uses it internally), but `embedder` is a `pub mod` and `Embedder` is a
// `pub struct` with `pub fn new`/`embed`/`dimension`, so the full path
// below works without any crate changes.
use memolite::embedder::Embedder;

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

struct PairSet {
    category: &'static str,
    pairs: &'static [(&'static str, &'static str)],
}

const PAIR_SETS: &[PairSet] = &[
    PairSet {
        category: "Near-templated (single word swapped)",
        pairs: &[
            (
                "The user debugged a login timeout issue in the auth service",
                "The user debugged a login timeout problem in the auth service",
            ),
            (
                "The user prefers dark mode across all their applications",
                "The user prefers dark mode across all their apps",
            ),
            (
                "The deployment failed because of a missing environment variable",
                "The deployment failed because of a missing config variable",
            ),
        ],
    },
    PairSet {
        category: "Genuine paraphrase (same fact, different wording)",
        pairs: &[
            (
                "Debugged the login timeout in auth today",
                "Spent an hour tracking down why users were getting logged out",
            ),
            (
                "The user prefers dark mode across all their applications",
                "The user really likes dark themes for all their software",
            ),
            (
                "Finally found the auth timeout bug -- it was the session TTL",
                "The root cause of the login timeouts was the session's TTL setting",
            ),
        ],
    },
    PairSet {
        category: "Unrelated",
        pairs: &[
            (
                "The user prefers dark mode across all their applications",
                "The production database credentials rotated successfully",
            ),
            (
                "Debugged the login timeout in auth today",
                "The quarterly report is due next Friday",
            ),
            (
                "The user's favorite programming language is Rust",
                "It rained heavily in Seattle yesterday",
            ),
        ],
    },
];

fn main() -> Result<()> {
    let mut embedder = Embedder::new()?;

    println!("Compression clustering threshold currently in use: 0.85\n");

    for set in PAIR_SETS {
        println!("== {} ==", set.category);
        let mut scores = Vec::with_capacity(set.pairs.len());

        for (a, b) in set.pairs {
            let vec_a = embedder.embed(a)?;
            let vec_b = embedder.embed(b)?;
            let score = cosine(&vec_a, &vec_b);
            scores.push(score);
            println!("  {score:.4}  |  \"{a}\"  vs  \"{b}\"");
        }

        let min = scores.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = scores.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let avg = scores.iter().sum::<f32>() / scores.len() as f32;
        println!("  -> min {min:.4}  max {max:.4}  avg {avg:.4}\n");
    }

    println!(
        "A workable threshold sits above the top of 'Unrelated' and below \
         the bottom of 'Genuine paraphrase'. If those two ranges overlap, \
         there's a real precision/recall trade-off to choose explicitly \
         rather than leave baked into an arbitrary 0.85."
    );

    Ok(())
}