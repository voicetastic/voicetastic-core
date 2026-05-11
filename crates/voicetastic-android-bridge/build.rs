// Build script: compile the UniFFI scaffolding from `src/voicetastic.udl`.
//
// This emits `voicetastic.uniffi.rs` into `OUT_DIR`, which `lib.rs`
// pulls in via `uniffi::include_scaffolding!`.
fn main() {
    uniffi::generate_scaffolding("src/voicetastic.udl")
        .expect("failed to generate UniFFI scaffolding for src/voicetastic.udl");
}
