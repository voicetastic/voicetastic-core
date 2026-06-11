/*
 * C ABI for voicetastic-esp32-bridge (a static archive built from
 * voicetastic-core). Include from the firmware C++ and link the
 * libvoicetastic_esp32_bridge.a produced by:
 *
 *   cargo build --release -p voicetastic-esp32-bridge \
 *       --target xtensa-esp32s3-espidf
 *
 * Slice 1 surface only (toolchain proof). Grows to the sans-IO protocol once
 * the link is verified on hardware. Hand-maintained for now; switch to a
 * cbindgen-generated header when the surface expands.
 */
#ifndef VOICETASTIC_CORE_H
#define VOICETASTIC_CORE_H

#ifdef __cplusplus
extern "C" {
#endif

/* Static NUL-terminated build identifier; never NULL. */
const char *vt_core_version(void);

/*
 * Encode one 40 ms frame of 8 kHz silence at `codec_param` via core's Codec2.
 * Returns the encoded byte count (> 0) on success, -1 on error. Smoke test
 * that the codec2 path linked.
 */
int vt_codec2_smoke(unsigned char codec_param);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* VOICETASTIC_CORE_H */
