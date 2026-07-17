//! Sanity check that `src/lib.rs` only registers modules that are real,
//! populated code as of M3, and does not yet declare any milestone-4+
//! module before it has content (Phase 1, Steps 2 and 27).

#[test]
fn lib_rs_registers_only_the_modules_that_exist_in_m3() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let lib_src = std::fs::read_to_string(format!("{manifest_dir}/src/lib.rs"))
        .expect("src/lib.rs should be readable");

    for expected in ["error", "embedder", "memory", "engine", "vector_store", "recall"] {
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

    for not_yet in [
        "ranking",
        "requests",
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