# voicetastic-android-bridge
UniFFI scenario that exposes [`voicetastic-core`](../voicetastic-core)'s
voice protocol (`build_message`, `VoiceAssembler`, `build_nack`,
`random_message_id`, `detect_version`) to Kotlin/Android.
## Scope
Voice protocol layer only. The Android app keeps its existing BLE / USB
transport stack and Meshtastic state machine; this bridge replaces the
Kotlin `VoiceChunker` / `VoiceAssembler` (protocol v1) with calls into
`voicetastic-core::voice::*` (protocol v2: 12-byte header, AES-GCM
envelope, Reed-Solomon FEC, NACK-driven selective retransmit).
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
