// Build script: compute BLAKE3 hash of autonomous_decision_service.rs source so the
// gate_source_hash field in ConfigSnapshot reflects the actual gate logic bytes,
// not just a manually-maintained version string. Any edit to the gate logic file
// automatically changes the hash, making stale-replay detection reliable.
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let service_path = Path::new(&manifest_dir)
        .join("src")
        .join("services")
        .join("autonomous_decision_service.rs");

    // Re-run this script whenever the gate logic file changes.
    println!(
        "cargo:rerun-if-changed=src/services/autonomous_decision_service.rs"
    );

    let source_bytes = fs::read(&service_path)
        .unwrap_or_else(|e| panic!("build.rs: failed to read {}: {e}", service_path.display()));

    let hash = blake3::hash(&source_bytes);
    let hash_hex = hash.to_hex().to_string();

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let out_path = Path::new(&out_dir).join("gate_source_hash.txt");
    fs::write(&out_path, &hash_hex)
        .unwrap_or_else(|e| panic!("build.rs: failed to write hash file: {e}"));
}
