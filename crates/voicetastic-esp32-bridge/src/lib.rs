//! C ABI bridge: voicetastic-proto -> ESP32 firmware (PlatformIO).
//!
//! Cross-compiled for `xtensa-esp32s3-none-elf` (`-Zbuild-std=core,alloc`) and
//! linked into the Arduino-ESP32 firmware as a static archive. On the
//! bare-metal target this crate provides the global allocator (firmware libc
//! heap) and panic handler; on a std host they gate out so
//! `cargo build`/`clippy --workspace` work. The FFI surface is identical.
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
        pub fn esp_rom_printf(fmt: *const c_char, ...) -> c_int;
        fn abort() -> !;
    }

    /// Synchronous ROM-UART print (no heap, safe in any context incl. just
    /// before a crash). Used for on-device self-test checkpoints.
    pub(crate) fn rom_print(msg: &core::ffi::CStr) {
        unsafe {
            esp_rom_printf(msg.as_ptr());
        }
    }

    struct FirmwareHeap;

    unsafe impl GlobalAlloc for FirmwareHeap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // ESP-IDF overrides malloc/free to use its heap (8-byte aligned).
            // proto only needs <=8 alignment.
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
        rom_print(c"\n[vt-core] RUST PANIC in voicetastic-proto\n");
        unsafe { abort() }
    }
}

/// Checkpoint trace - prints on-device, no-op on a std host.
#[inline]
fn trace(_msg: &core::ffi::CStr) {
    #[cfg(target_os = "none")]
    embedded_rt::rom_print(_msg);
}

/// Static NUL-terminated build identifier; never null.
#[unsafe(no_mangle)]
pub extern "C" fn vt_core_version() -> *const c_char {
    concat!("voicetastic-proto ", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Staged on-device self-test of the shared wire protocol. Prints a checkpoint
/// before each stage (so the last line seen pinpoints any crash), then returns
/// the frame count (> 0) on success, negative on a handled error.
#[unsafe(no_mangle)]
pub extern "C" fn vt_proto_selftest() -> c_int {
    use voicetastic_proto::builder::{BuildConfig, build_message};
    use voicetastic_proto::types::VoiceCodec;

    trace(c"[vt] selftest: enter\n");

    // Stage 1: the global allocator on its own.
    let mut probe = alloc::vec::Vec::<u8>::with_capacity(32);
    probe.push(0xAB);
    if probe.first() != Some(&0xAB) {
        return -10;
    }
    trace(c"[vt] selftest: alloc ok\n");

    // Stage 2: header + MAC round-trip (sha2, ~no heap, no Reed-Solomon).
    {
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
        if ChunkHeader::parse(&bytes).is_err() {
            return -11;
        }
    }
    trace(c"[vt] selftest: header ok\n");

    // Stage 3: chunk + Reed-Solomon via the shared protocol.
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
    let n = match build_message(&audio, &cfg) {
        Ok(enc) => enc.frames.len() as c_int,
        Err(_) => -1,
    };
    trace(c"[vt] selftest: build_message done\n");
    n
}
