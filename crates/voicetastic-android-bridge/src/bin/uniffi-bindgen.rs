//! Re-export of UniFFI's standalone bindgen so Gradle can invoke it via
//! `cargo run --bin uniffi-bindgen -- generate ...` without forcing the
//! user to `cargo install uniffi-bindgen-cli` out-of-band.
fn main() {
    uniffi::uniffi_bindgen_main()
}
