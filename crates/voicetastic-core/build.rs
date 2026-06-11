use std::path::PathBuf;
use std::process::Command;

type BuildResult<T> = Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(e) = build() {
        eprintln!("\nerror: voicetastic-core build script failed\n\n{e}\n");
        std::process::exit(1);
    }
}

fn build() -> BuildResult<()> {
    // Verify protoc is available before doing any work so the user gets a
    // friendly diagnostic instead of a panic from prost-build.
    if Command::new("protoc").arg("--version").output().is_err() {
        return Err(
            "`protoc` was not found in PATH.\n\
             hint: install the Protocol Buffers compiler and initialise the proto submodule:\n\
             \n\
             Debian/Ubuntu:  sudo apt install protobuf-compiler\n\
             Homebrew:       brew install protobuf\n\
             \n\
             Then run: git submodule update --init --recursive"
                .into(),
        );
    }

    // When the `codecs` feature is enabled, the native AMR-NB encoder/decoder
    // in src/codec/imp.rs links against libopencore-amrnb. Probe for it here so
    // the user gets a friendly diagnostic instead of an inscrutable linker error.
    if std::env::var_os("CARGO_FEATURE_CODECS").is_some() {
        probe_opencore_amrnb()?;
    }

    // Compile from inside the proto root so protoc resolves both the file path
    // on the command line and any `import "meshtastic/foo.proto"` directives
    // against the same canonical name. Otherwise protoc treats the same file as
    // two distinct units (once via the relative cli path, once via the include
    // path) and reports duplicate symbols.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto_root = manifest_dir
        .join("../../proto")
        .canonicalize()
        .map_err(|e| format!("proto submodule not found at ../../proto: {e}"))?;
    let meshtastic_dir = proto_root.join("meshtastic");

    let mut protos: Vec<PathBuf> = std::fs::read_dir(&meshtastic_dir)
        .map_err(|e| format!("failed to read {}: {e}", meshtastic_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("proto"))
        .collect();
    protos.sort();

    if protos.is_empty() {
        return Err(format!(
            "no .proto files found under {} (submodule probably not initialised)",
            meshtastic_dir.display()
        )
        .into());
    }

    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        proto_root.join("nanopb.proto").display()
    );

    let rel_protos: Vec<PathBuf> = protos
        .iter()
        .map(|p| {
            p.strip_prefix(&proto_root)
                .map(|rel| rel.to_path_buf())
                .map_err(|e| format!("path not under proto root: {e}"))
        })
        .collect::<Result<_, _>>()?;

    // Pass paths to protoc relative to `proto_root`, with `proto_root` as the
    // sole include directory. protoc canonicalises both the cli paths and the
    // `import "meshtastic/foo.proto"` directives against the same prefix, so
    // it doesn't see each file twice and report duplicate symbols. Using
    // absolute paths plus an absolute include avoids touching the process
    // working directory.
    let mut config = prost_build::Config::new();
    config.protoc_arg("--experimental_allow_proto3_optional");
    // Pre-pend the include path on the command line so protoc resolves names
    // before falling back to its default behaviour.
    config.protoc_arg("-I");
    config.protoc_arg(proto_root.to_string_lossy().into_owned());
    config
        .compile_protos(&rel_protos, std::slice::from_ref(&proto_root))
        .map_err(|e| format!("failed to compile meshtastic protos: {e}"))?;
    Ok(())
}

// Resolve libopencore-amrnb for the `codecs` feature.
//
// Priority:
//   1. OPENCORE_AMRNB_LIB_DIR - emit a native search path and let the
//      #[link(name = "opencore-amrnb")] attribute in codec/imp.rs do the rest.
//      Useful for cross-compilation or non-standard install prefixes.
//   2. pkg-config - probe "opencore-amrnb" and emit link flags automatically.
fn probe_opencore_amrnb() -> BuildResult<()> {
    println!("cargo:rerun-if-env-changed=OPENCORE_AMRNB_LIB_DIR");

    if let Some(dir) = std::env::var_os("OPENCORE_AMRNB_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
        return Ok(());
    }

    pkg_config::probe_library("opencore-amrnb").map_err(|e| {
        format!(
            "libopencore-amrnb not found via pkg-config: {e}\n\
             hint: install the development package:\n\
             \n\
             Debian/Ubuntu:  sudo apt install libopencore-amrnb-dev\n\
             Fedora/RHEL:    sudo dnf install opencore-amr-devel\n\
             Homebrew:       brew install opencore-amr\n\
             \n\
             Or set OPENCORE_AMRNB_LIB_DIR to the directory containing the library\n\
             if it is installed in a non-standard prefix."
        )
    })?;
    Ok(())
}
