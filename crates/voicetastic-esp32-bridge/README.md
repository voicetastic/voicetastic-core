# voicetastic-esp32-bridge

C ABI static archive exposing `voicetastic-core` to the ESP32 firmware
(PlatformIO / Arduino-ESP32), so the firmware can build on the one shared core
instead of carrying its own C++ reimplementation of the voice protocol
(`VtAssembler`, `VtChunker`, `VtProtocol`, `rs/`). This is the firmware analog
of `voicetastic-android-bridge` (Android) and the wasm path (web).

## Status: slice 1 (toolchain proof)

The surface is intentionally tiny - just enough to prove the cross-compile +
static link + FFI call end to end before moving protocol logic across:

- `vt_core_version()` - static build-id string.
- `vt_codec2_smoke(codec_param)` - encodes one 8 kHz silence frame via core's
  pure-Rust Codec2; returns the byte count. Forces the `codec2` feature to
  compile + link.

**Verified on host** (x86-64): `cargo build -p voicetastic-esp32-bridge`
produces `libvoicetastic_esp32_bridge.a`; a C harness links it and prints the
version + `codec2 smoke (mode 5): 6 bytes` (correct: 1200 bps = 6 bytes/frame).
So the FFI/ABI/staticlib mechanics and codec2 linkage are proven.

**Not yet verified**: the Xtensa cross-compile and on-device link (needs espup
+ hardware). See the open decision below.

## Open decision: which ESP32-S3 target / std model

The ESP32-S3 is Xtensa, so this needs the esp-rs Rust fork (`espup`), not
mainline. Two targets, and core is `std` today:

- **`xtensa-esp32s3-espidf` (std)** - easiest source-wise (core stays std, like
  the wasm build), but `esp-idf-sys` wants to *own* the ESP-IDF build, which
  collides with PlatformIO/Arduino-ESP32 already owning ESP-IDF. Linking the
  resulting `.a` into the PlatformIO build is the integration risk to resolve
  first.
- **`xtensa-esp32s3-none-elf` (no_std + alloc)** - links cleanly into the
  existing firmware libc/runtime (no second ESP-IDF), but core is **not**
  `no_std`; the sans-IO subset would need a `no_std`+`alloc` cfg (replace
  `std::time`, `HashMap` hasher, `parking_lot`, etc.). Bigger core change,
  cleaner embedding.

Recommendation: try the `espidf` (std) path first to validate the link with the
least source churn; fall back to a `no_std` subset if the esp-idf-sys ownership
conflict can't be reconciled inside the PlatformIO build.

## Building for the device

```sh
# one-time: install the Xtensa Rust toolchain
cargo install espup && espup install
# then, from the voicetastic-core workspace root:
cargo build --release -p voicetastic-esp32-bridge --target xtensa-esp32s3-espidf
# -> target/xtensa-esp32s3-espidf/release/libvoicetastic_esp32_bridge.a
```

## Wiring into the firmware (PlatformIO)

`platformio-link.py` in this directory is a starting-point pre-build script:
it runs the cargo build above and adds the `.a` + `include/` to the link. Copy
it into the firmware's `extra_scripts` and point `VT_CORE_DIR` at a sibling
checkout of `voicetastic-core` (mirroring how the firmware clones `device-ui`
as a sibling). It is **untested on hardware** - it encodes the plan, not a
verified build.

Firmware call site (slice 1 smoke), e.g. at boot:

```cpp
#include "voicetastic_core.h"
LOG_INFO("voicetastic-core linked: %s", vt_core_version());
```

If that line prints over serial on a flashed t-deck-tft, the toolchain is
proven and the surface can grow to the sans-IO protocol (`decode_inbound`,
`VoiceAssembler`, `OutgoingVoiceRegistry`, chunker/FEC, codec, denoiser).
