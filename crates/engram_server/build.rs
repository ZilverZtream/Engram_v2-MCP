// Build script: hash gate logic and its evidence dependencies so the
// gate_source_hash field in ConfigSnapshot reflects the actual gate logic bytes,
// not just a manually-maintained version string. Any edit to these source files
// automatically changes the hash, making stale-replay detection reliable.
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    // Include the selected-edge scorer and its shared semantic-kind predicate:
    // changing a dependency must also invalidate an old gate replay identity.
    let mut hasher = blake3::Hasher::new();
    for relative in ["src/services/autonomous_decision_service.rs", "src/services/evidence_orchestration.rs",
        "src/services/extraction_evidence.rs", "src/services/blast_radius_service.rs"] {
        println!("cargo:rerun-if-changed={relative}");
        let source_bytes = fs::read(Path::new(&manifest_dir).join(relative))
            .unwrap_or_else(|e| panic!("build.rs: failed to read {relative}: {e}"));
        hasher.update(relative.as_bytes()); hasher.update(&[0]);
        hasher.update(&(source_bytes.len() as u64).to_le_bytes()); hasher.update(&source_bytes);
    }
    let hash_hex = hasher.finalize().to_hex().to_string();

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let out_path = Path::new(&out_dir).join("gate_source_hash.txt");
    fs::write(&out_path, &hash_hex)
        .unwrap_or_else(|e| panic!("build.rs: failed to write hash file: {e}"));
}
