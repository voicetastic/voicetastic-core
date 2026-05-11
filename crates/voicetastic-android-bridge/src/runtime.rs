// SPDX-License-Identifier: MIT
//
//! Shared Tokio runtime for the Android bridge.
//!
//! The voice-protocol primitives exposed in PR-MVP (`build_message`,
//! `VoiceAssembler`) are fully synchronous and do not need a runtime.
//! Starting with PR 1 the bridge will also expose
//! `voicetastic_core::service::MeshService`, which internally relies on
//! `tokio::spawn` and `tokio::sync::{mpsc, watch, broadcast}`. Those
//! primitives require a running multi-thread reactor.
//!
//! On Android the only thread guaranteed to outlive any single UniFFI call
//! is one we own, so we lazily create a process-wide runtime the first time
//! the bridge needs it. The runtime keeps running until the host process
//! exits (i.e. until `System.exit` or the OS kills the app), which matches
//! the lifetime model JNI gives us anyway.
//!
//! ## Sizing
//!
//! Two worker threads is a deliberate compromise:
//! - One thread comfortably handles the inbound BLE notification stream,
//!   the outbound serializer, and the voice-tx pacing loop, because none
//!   of them are CPU-bound (they're all I/O + small protobuf decodes).
//! - The second thread absorbs the occasional Reed-Solomon decode or
//!   AES-GCM seal/open without stalling the inbound path.
//!
//! On low-end Android devices (the floor we care about) the runtime adds
//! ~2 threads + ~1 MiB of stacks; that's negligible next to the JVM.

use once_cell::sync::Lazy;
use tokio::runtime::{Builder, Runtime};

/// Process-wide multi-thread Tokio runtime owned by the bridge.
///
/// Accessed through [`runtime()`]; do not call this directly from UniFFI
/// glue — go through the accessor so the panic message points at the right
/// place if initialization ever fails.
static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .enable_time()
        .thread_name("voicetastic-bridge")
        .build()
        .expect("failed to start voicetastic-bridge tokio runtime")
});

/// Returns the bridge-wide Tokio runtime, starting it on first call.
///
/// Safe to call from any thread (including a JNI call-in). The first call
/// pays the runtime-startup cost (~1 ms on a Pixel-class device); every
/// subsequent call is a single `OnceCell` load.
#[allow(dead_code)] // wired in PR 1 when MeshService lands.
pub(crate) fn runtime() -> &'static Runtime {
    &RUNTIME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_runs_a_task() {
        let answer = runtime().block_on(async { 1 + 2 });
        assert_eq!(answer, 3);
    }

    #[test]
    fn runtime_is_reused_across_calls() {
        let a = runtime() as *const Runtime;
        let b = runtime() as *const Runtime;
        assert_eq!(a, b, "runtime() must return the same global instance");
    }
}
