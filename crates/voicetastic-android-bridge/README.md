# voicetastic-android-bridge
UniFFI scenario that exposes [`voicetastic-core`](../voicetastic-core)'s
voice protocol (`build_message`, `VoiceAssembler`, `build_nack`,
`random_message_id`, `detect_version`) to Kotlin/Android.

## Architecture: codec responsibility

Audio **encode/decode is handled by Android** (`android.media.MediaCodec`), not
by Rust. The bridge only passes codec info as opaque identifying metadata:

```
Android mic → MediaCodec (opus/amr-nb) → encoded bytes
  → build_message(audio=bytes, BuildConfig { codec, codec_param, … })
  → wire frames → mesh
```

The voice protocol engine (`voicetastic-core::voice`) is **codec-free**: it
never touches PCM samples. This keeps the bridge narrow (no PCM marshalling
across FFI), avoids cross-compiling C codec libraries for Android NDK, and
lets Android use its **HW-accelerated** media codecs for better power and
CPU efficiency.

Supported codecs on Android:
| Codec  | MediaCodec support | Notes                              |
|--------|--------------------|------------------------------------|
| AMR-NB | Encode + decode    | Required codec, available API 1+   |
| Opus   | Decode API 21+, encode API 29+ | Full support on API 29+ |
| Codec2 | ❌ Not supported    | Niche amateur-radio codec, no platform decoder |

## Scope
Two surfaces:

1. **Voice protocol** — `build_message`, `VoiceAssembler`, `build_nack`,
   `random_message_id`, `detect_version`. Replaces the Kotlin
   `VoiceChunker` / `VoiceAssembler` (protocol v1) with calls into
   `voicetastic-core::voice::*` (protocol v2: 16-byte header with
   4-byte trailing MAC, AES-GCM envelope, Reed-Solomon FEC, NACK-driven
   selective retransmit).
2. **Settings** — `SettingsApi`, the centralised client-side preference
   facade (last device, voice max duration, reassembly timeout,
   outgoing codec, Codec2 mode, AMR-NB mode). Same TOML schema as the desktop GUI /
   CLI. Persistence path is **host-injected**: pass the app's private
   data directory (typically `Context.filesDir.path`) as the
   constructor argument, or `null` for an in-memory store. Use the
   typed accessors (`set_voice_codec(...)` etc.) for known keys, and
   the generic `list()` / `get_str()` / `set_str()` if you want to
   render a descriptor-driven settings screen.

Out of scope here: the `MeshService` façade and the `Transport` foreign
trait. A follow-up bridge can add those when the Android side is ready
to also retire its Kotlin Meshtastic state machine.
## Building
### Host (sanity check):
```bash
cargo test -p voicetastic-android-bridge
```
### Android (per ABI):
The Gradle layer in the Android app drives this — see the corresponding
PR there. Manual invocation for a single ABI:
```bash
export ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/<version>
TC=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64
CC_aarch64_linux_android=$TC/bin/aarch64-linux-android24-clang \
AR_aarch64_linux_android=$TC/bin/llvm-ar \
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$TC/bin/aarch64-linux-android24-clang \
cargo build -p voicetastic-android-bridge \
    --target aarch64-linux-android --release
```
The output `target/aarch64-linux-android/release/libvoicetastic.so`
gets packaged into the Android app under
`app/src/main/jniLibs/arm64-v8a/libvoicetastic.so`.
## Kotlin bindings
The `.udl` file is the single source of truth for the foreign API. To
generate the Kotlin wrapper:
```bash
cargo run --bin uniffi-bindgen -- generate \
    src/voicetastic.udl --language kotlin --out-dir target/kotlin
```
In the Android Gradle build this is automated by a task that emits the
generated wrapper into `app/build/generated/source/uniffi/`.
## Wire format
See [`VOICE_PROTOCOL.md`](../../VOICE_PROTOCOL.md) at the workspace root.
