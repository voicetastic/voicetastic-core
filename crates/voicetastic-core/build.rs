use std::path::PathBuf;

fn main() {
    // Compile from inside the proto root so protoc resolves both the file path
    // on the command line and any `import "meshtastic/foo.proto"` directives
    // against the same canonical name. Otherwise protoc treats the same file as
    // two distinct units (once via the relative cli path, once via the include
    // path) and reports duplicate symbols.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto_root = manifest_dir
        .join("../../proto")
        .canonicalize()
        .expect("proto root");
    let meshtastic_dir = proto_root.join("meshtastic");

    let mut protos: Vec<PathBuf> = std::fs::read_dir(&meshtastic_dir)
        .expect("read proto/meshtastic")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("proto"))
        .collect();
    protos.sort();

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
                .expect("under proto root")
                .to_path_buf()
        })
        .collect();

    let prev_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&proto_root).expect("chdir proto root");

    let mut config = prost_build::Config::new();
    config.protoc_arg("--experimental_allow_proto3_optional");
    let result = config.compile_protos(&rel_protos, &[PathBuf::from(".")]);

    std::env::set_current_dir(&prev_cwd).expect("restore cwd");
    result.expect("compile meshtastic protos");
}
