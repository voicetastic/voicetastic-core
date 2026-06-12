//! C ABI bridge: voicetastic-proto -> ESP32 firmware (PlatformIO).
//!
//! no_std on `xtensa-esp32s3-none-elf` (`-Zbuild-std=core,alloc`); provides a
//! global allocator (firmware libc heap) + panic handler there, gated out on a
//! std host. Self-test is split into FFI stages the firmware logs between, so
//! the last visible line localizes any fault. Panics route to the firmware's
//! `vt_host_log` (visible USB-CDC console), not `esp_rom_printf` (UART0).
#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;

use core::ffi::{c_char, c_int};

#[cfg(target_os = "none")]
mod embedded_rt {
    use core::alloc::{GlobalAlloc, Layout};
    use core::ffi::{c_char, c_void};

    unsafe extern "C" {
        fn malloc(size: usize) -> *mut c_void;
        fn free(ptr: *mut c_void);
        fn abort() -> !;
        // Provided by the firmware; routes to the visible LOG console.
        fn vt_host_log(msg: *const c_char);
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
            vt_host_log(c"RUST PANIC in voicetastic-proto".as_ptr());
            abort()
        }
    }
}

/// Static NUL-terminated build identifier; never null.
#[unsafe(no_mangle)]
pub extern "C" fn vt_core_version() -> *const c_char {
    concat!("voicetastic-proto ", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Stage 1: global allocator alone. Returns 1 on success, -1 otherwise.
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

/// Stage 2: header + MAC round-trip (sha2). Returns 0 on success, -1 on fail.
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

// Shared chunker config; only `parity_count` differs between the two stages.
fn build_cfg(parity_count: u8) -> voicetastic_proto::builder::BuildConfig {
    use voicetastic_proto::types::VoiceCodec;
    voicetastic_proto::builder::BuildConfig {
        message_id: 0xDEAD_BEEF,
        stream_seq: 7,
        codec: VoiceCodec::Codec2,
        codec_param: 5,
        chunk_size: 32,
        parity_count,
        last_in_stream: true,
    }
}

/// Stage 3a: chunker + framing, NO Reed-Solomon (parity_count = 0).
/// Returns the frame count (expect 2 data frames), -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn vt_chunk_smoke() -> c_int {
    let audio = [0u8; 64];
    match voicetastic_proto::builder::build_message(&audio, &build_cfg(0)) {
        Ok(enc) => enc.frames.len() as c_int,
        Err(_) => -1,
    }
}

/// Stage 3b: chunker + Reed-Solomon (parity_count = 2). Returns frame count
/// (expect 4 = 2 data + 2 parity), -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn vt_proto_selftest() -> c_int {
    let audio = [0u8; 64];
    match voicetastic_proto::builder::build_message(&audio, &build_cfg(2)) {
        Ok(enc) => enc.frames.len() as c_int,
        Err(_) => -1,
    }
}
