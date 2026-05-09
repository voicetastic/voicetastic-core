use std::path::PathBuf;
use std::process::Command;

type BuildResult<T> = Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(e) = build() {
        eprintln!("\nerror: voicetastic-core build script failed: {e}");
        eprintln!(
            "hint: ensure `protoc` is installed and the proto submodule is initialised\n      \
             (`git submodule update --init --recursive`)\n"
        );
        std::process::exit(1);
    }
}

fn build() -> BuildResult<()> {
    // Verify protoc is available before doing any work so the user gets a
    // friendly diagnostic instead of a panic from prost-build.
    if Command::new("protoc").arg("--version").output().is_err() {
        return Err("`protoc` was not found in PATH (install Protocol Buffers compiler)".into());
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
