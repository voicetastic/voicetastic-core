//! C ABI bridge: voicetastic-proto -> ESP32 firmware (PlatformIO).
//!
//! Cross-compiled for `xtensa-esp32s3-none-elf` (`-Zbuild-std=core,alloc`) and
//! linked into the Arduino-ESP32 firmware as a static archive. On the
//! bare-metal target the firmware's C/C++ runtime owns neither a Rust
//! allocator nor a panic handler, so this crate provides both (allocator
//! backed by the firmware libc heap via `memalign`/`free`). On a std host
//! (so `cargo build`/`clippy --workspace` work) those are gated out and the
//! crate uses std's - the FFI surface compiles identically either way.
#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;

use core::ffi::{c_char, c_int};

// Bare-metal runtime: global allocator (firmware heap) + panic handler.
// Only on `target_os = "none"`; on a std host these come from std.
#[cfg(target_os = "none")]
mod embedded_rt {
    use core::alloc::{GlobalAlloc, Layout};
    use core::ffi::c_void;

    unsafe extern "C" {
        fn memalign(align: usize, size: usize) -> *mut c_void;
        fn free(ptr: *mut c_void);
    }

    struct FirmwareHeap;

    unsafe impl GlobalAlloc for FirmwareHeap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // memalign needs a power-of-two align >= sizeof(void*); Layout
            // guarantees power-of-two. Allocations here are small.
            let align = layout.align().max(core::mem::size_of::<usize>());
            unsafe { memalign(align, layout.size()) as *mut u8 }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            unsafe { free(ptr as *mut c_void) }
        }
    }

    #[global_allocator]
    static HEAP: FirmwareHeap = FirmwareHeap;

    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo) -> ! {
        loop {}
    }
}

/// Static NUL-terminated build identifier; never null.
#[unsafe(no_mangle)]
pub extern "C" fn vt_core_version() -> *const c_char {
    concat!("voicetastic-proto ", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// End-to-end self-test of the shared wire protocol on-device: chunk a small
/// buffer with FEC parity via `voicetastic_proto::build_message`, exercising
/// the chunker, header + MAC, Reed-Solomon encode, and the global allocator.
/// Returns the number of frames produced (> 0) on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn vt_proto_selftest() -> c_int {
    use voicetastic_proto::builder::{BuildConfig, build_message};
    use voicetastic_proto::types::VoiceCodec;

    let audio = [0u8; 64];
    let cfg = BuildConfig {
        message_id: 0xDEAD_BEEF, // host injects the real id; fixed here for the test
        stream_seq: 7,
        codec: VoiceCodec::Codec2,
        codec_param: 5,
        chunk_size: 32,
        parity_count: 2,
        last_in_stream: true,
    };
    match build_message(&audio, &cfg) {
        Ok(enc) => enc.frames.len() as c_int,
        Err(_) => -1,
    }
}
