//! Sanity check that `src/lib.rs` only registers modules that are real,
//! populated code as of the current milestone, and does not yet declare
//! any later-milestone module before it has content.
//!
//! Updated from the M4 baseline: `requests` was M5's own new module
//! (Steps 5.1-5.3 -- `StoreRequest`/`MemoryUpdate`/`ExpiryPolicy`), so as
//! of M5 it moves from the "not yet" list into the "must be registered
//! and have a real file" list, alongside everything M4 already
//! established (`ranking` made the same move at the M3->M4 boundary).
//! Every milestone after M5 should bump this same boundary forward the
//! same way, rather than deleting the guard.

#[test]
fn lib_rs_registers_only_the_modules_that_exist_in_m5() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let lib_src = std::fs::read_to_string(format!("{manifest_dir}/src/lib.rs"))
        .expect("src/lib.rs should be readable");

    for expected in [
        "error",
        "embedder",
        "memory",
        "engine",
        "vector_store",
        "recall",
        "ranking",
        "requests",
    ] {
        assert!(
            lib_src.contains(&format!("pub mod {expected};")),
            "lib.rs must register `{expected}` as a public module"
        );

        let single_file = format!("{manifest_dir}/src/{expected}.rs");
        let dir_module = format!("{manifest_dir}/src/{expected}/mod.rs");
        assert!(
            std::path::Path::new(&single_file).exists() || std::path::Path::new(&dir_module).exists(),
            "module `{expected}` is registered in lib.rs but has no file on disk"
        );
    }

    assert!(
        lib_src.contains("mod migrations;"),
        "migrations must be registered (privately, not `pub mod`)"
    );

    // M4 introduces RecallQuery/RecallItem/RecallResult and re-exports them
    // at the crate root, same as every other public-facing type.
    for reexported in ["RecallQuery", "RecallItem", "RecallResult"] {
        assert!(
            lib_src.contains(reexported),
            "`{reexported}` should be re-exported from lib.rs once M4 introduces it"
        );
    }

    // M5 introduces StoreRequest/MemoryUpdate/ExpiryPolicy and re-exports
    // them at the crate root, same treatment as M4's recall types above.
    for reexported in ["StoreRequest", "MemoryUpdate", "ExpiryPolicy"] {
        assert!(
            lib_src.contains(reexported),
            "`{reexported}` should be re-exported from lib.rs once M5 introduces it"
        );
    }

    for not_yet in [
        "confidence",
        "streaming",
        "compression",
        "maintenance",
        "stats",
    ] {
        assert!(
            !lib_src.contains(&format!("pub mod {not_yet};")),
            "`{not_yet}` must not be registered until its own milestone gives it real content"
        );
    }
}