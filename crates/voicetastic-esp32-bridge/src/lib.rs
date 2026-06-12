//! C ABI bridge: voicetastic-proto -> ESP32 firmware (PlatformIO).
//!
//! Cross-compiled for `xtensa-esp32s3-none-elf` (`-Zbuild-std=core,alloc`) and
//! linked into the Arduino-ESP32 firmware as a static archive. On the
//! bare-metal target this crate provides the global allocator (firmware libc
//! heap) and a panic handler; on a std host they gate out so
//! `cargo build`/`clippy --workspace` work. The FFI surface is identical.
//!
//! The self-test is split into three calls (alloc / header+MAC / chunk+FEC) so
//! the firmware can `LOG_INFO` between them on the visible (USB-CDC) console -
//! the last log before a crash localizes any fault, without relying on
//! `esp_rom_printf` (which targets UART0, not the USB console).
#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;

use core::ffi::{c_char, c_int};

#[cfg(target_os = "none")]
mod embedded_rt {
    use core::alloc::{GlobalAlloc, Layout};
    use core::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        fn malloc(size: usize) -> *mut c_void;
        fn free(ptr: *mut c_void);
        fn esp_rom_printf(fmt: *const c_char, ...) -> c_int;
        fn abort() -> !;
    }

    struct FirmwareHeap;

    unsafe impl GlobalAlloc for FirmwareHeap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if layout.align() > 8 {
                return core::ptr::null_mut();
            }
            unsafe { malloc(layout.size()) as *mut u8 }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            unsafe { free(ptr as *mut c_void) }
        }
    }

    #[global_allocator]
    static HEAP: FirmwareHeap = FirmwareHeap;

    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo) -> ! {
        unsafe {
            esp_rom_printf(c"\n[vt-core] RUST PANIC in voicetastic-proto\n".as_ptr());
            abort()
        }
    }
}

/// Static NUL-terminated build identifier; never null.
#[unsafe(no_mangle)]
pub extern "C" fn vt_core_version() -> *const c_char {
    concat!("voicetastic-proto ", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Stage 1: the global allocator alone. Allocates + writes a small `Vec`,
/// returns its length (1) on success, -1 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn vt_alloc_smoke() -> c_int {
    let mut v = alloc::vec::Vec::<u8>::with_capacity(32);
    v.push(0xAB);
    if v.first() == Some(&0xAB) {
        v.len() as c_int
    } else {
        -1
    }
}

/// Stage 2: header + MAC round-trip (sha2; ~no heap, no Reed-Solomon).
/// Returns 0 on success, -1 on mismatch/parse failure.
#[unsafe(no_mangle)]
pub extern "C" fn vt_header_smoke() -> c_int {
    use voicetastic_proto::header::ChunkHeader;
    use voicetastic_proto::types::{PacketType, VoiceCodec};

    let h = ChunkHeader {
        packet_type: PacketType::Data,
        last_in_stream: false,
        message_id: 0xDEAD_BEEF,
        codec: VoiceCodec::Codec2,
        codec_param: 5,
        stream_seq: 7,
        chunk_index: 3,
        total_data: 22,
        parity_count: 5,
    };
    let bytes = h.serialize();
    match ChunkHeader::parse(&bytes) {
        Ok((r, _)) if r.message_id == h.message_id => 0,
        _ => -1,
    }
}

/// Stage 3: chunk + Reed-Solomon encode via the shared protocol. Returns the
/// frame count (> 0) on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn vt_proto_selftest() -> c_int {
    use voicetastic_proto::builder::{BuildConfig, build_message};
    use voicetastic_proto::types::VoiceCodec;

    let audio = [0u8; 64];
    let cfg = BuildConfig {
        message_id: 0xDEAD_BEEF,
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
