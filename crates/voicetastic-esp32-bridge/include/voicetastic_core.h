/*
 * C ABI for voicetastic-esp32-bridge (no_std static archive over
 * voicetastic-proto). The firmware provides `malloc`/`free` (newlib/esp-idf).
 */
#ifndef VOICETASTIC_CORE_H
#define VOICETASTIC_CORE_H

#ifdef __cplusplus
extern "C" {
#endif

/* Static NUL-terminated build identifier; never NULL. */
const char *vt_core_version(void);

/* Staged self-test (log between each to localize faults on the USB console). */
int vt_alloc_smoke(void);    /* global allocator: Vec alloc+write -> len (1), or -1 */
int vt_header_smoke(void);   /* header + MAC round-trip -> 0 ok, -1 fail */
int vt_chunk_smoke(void);    /* chunker, no FEC -> frame count (2), or -1 */
int vt_proto_selftest(void); /* chunk + Reed-Solomon -> frame count (>0), or -1 */

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* VOICETASTIC_CORE_H */
