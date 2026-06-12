/*
 * C ABI for voicetastic-esp32-bridge (a no_std static archive built from
 * voicetastic-proto). Link the libvoicetastic_esp32_bridge.a produced by:
 *
 *   cargo build --release -p voicetastic-esp32-bridge \
 *       -Z build-std=core,alloc --target xtensa-esp32s3-none-elf
 *
 * The firmware must provide `memalign` and `free` (newlib/esp-idf do).
 */
#ifndef VOICETASTIC_CORE_H
#define VOICETASTIC_CORE_H

#ifdef __cplusplus
extern "C" {
#endif

/* Static NUL-terminated build identifier; never NULL. */
const char *vt_core_version(void);

/*
 * Self-test of the shared wire protocol: chunk + FEC-encode a small buffer via
 * voicetastic-proto. Returns the frame count (> 0) on success, -1 on error.
 */
int vt_proto_selftest(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* VOICETASTIC_CORE_H */
