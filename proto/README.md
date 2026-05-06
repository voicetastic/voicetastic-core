# Vendored protobuf snapshot

The `meshtastic/` directory contains a snapshot of the upstream
[Meshtastic protobuf definitions](https://github.com/meshtastic/protobufs)
fetched on 2026-05-06 from the `master` branch.

`nanopb.proto` is a local minimal stub (option-only, no runtime use) so prost
can compile the upstream files which annotate fields with nanopb extensions.

When refreshing:

1. Re-fetch every `meshtastic/*.proto` from the upstream `master` branch.
2. Diff `nanopb.proto` against upstream nanopb if upstream adds new options.
3. Run `cargo build -p voicetastic-core` to regenerate Rust bindings.
