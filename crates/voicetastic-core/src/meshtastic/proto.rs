//! prost-generated Meshtastic protobuf bindings.
//!
//! The `meshtastic.*` packages from the upstream protobuf snapshot in
//! `proto/meshtastic/`.

#![allow(clippy::all)]
#![allow(missing_docs)]

include!(concat!(env!("OUT_DIR"), "/meshtastic.rs"));

// Some upstream files declare additional packages; include them if generated.
// prost-build emits one file per top-level package.
