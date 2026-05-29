// Loads the vendored libopus standalone wasm (built by the `libopus`
// crate via emscripten) and wraps its raw C ABI (opus_encoder_*,
// opus_decoder_*, opus_packet_get_*) into higher-level clip-level
// encode/decode functions that the Rust web crate calls via wasm-bindgen.
//
// The libopus wasm lives in its own linear memory, separate from the Rust
// wasm — so this shim does the byte/sample shuffling across the boundary.
// Built FIXED_POINT + DISABLE_FLOAT_API, so all PCM crosses as i16.

const SAMPLE_RATE = 48000;
const CHANNELS = 1;
const FRAME_SAMPLES = 960;          // 20 ms @ 48 kHz mono — matches the wire
                                     // format documented in codec/mod.rs.
const APPLICATION_VOIP = 2048;       // OPUS_APPLICATION_VOIP
const OPUS_SIGNAL_VOICE = 3001;      // OPUS_SIGNAL_VOICE
const MAX_FRAME_SAMPLES = 5760;     // 120 ms @ 48 kHz — Opus's largest legal
                                     // frame; sizes the decode scratch buffer.
const MAX_PACKET_BYTES = 1275;       // RFC 6716 §3.4 upper bound for one packet.

let opus = null;
let readyResolve;
let readyReject;
const ready = new Promise((res, rej) => { readyResolve = res; readyReject = rej; });

export async function opusProvideBytes(bytes) {
  try {
    const { instance } = await WebAssembly.instantiate(bytes, {
      env: { emscripten_notify_memory_growth: () => {} },
    });
    instance.exports._initialize();
    opus = instance;
    readyResolve();
  } catch (e) {
    readyReject(e);
    throw e;
  }
}

export async function opusEncodeClip(pcm_i16, bitrate_bps) {
  await ready;
  const ex = opus.exports;
  // Create encoder + set bitrate.
  const errPtr = ex.malloc(4);
  const encPtr = ex.opus_encoder_create(SAMPLE_RATE, CHANNELS, APPLICATION_VOIP, errPtr);
  // Refresh views once after the create call (which may grow memory).
  let mem32 = new Int32Array(ex.memory.buffer);
  const err = mem32[errPtr >> 2];
  if (err !== 0 || encPtr === 0) {
    ex.free(errPtr);
    throw new Error(`opus_encoder_create error ${err}`);
  }
  // opus_encoder_ctl is C-variadic, which emscripten STANDALONE_WASM can't
  // dispatch from JS (the trailing value silently drops). The helpers we
  // ship alongside libopus expose fixed-signature setters instead.
  ex.opus_helpers_encoder_set_bitrate(encPtr, bitrate_bps);
  ex.opus_helpers_encoder_set_signal(encPtr, OPUS_SIGNAL_VOICE);

  const pcmPtr = ex.malloc(FRAME_SAMPLES * 2);
  const outPtr = ex.malloc(MAX_PACKET_BYTES);
  // The hot loop only stack-allocates inside opus_encode (VAR_ARRAYS), so
  // the underlying ArrayBuffer doesn't grow — safe to keep these views.
  let mem8 = new Uint8Array(ex.memory.buffer);
  let mem16 = new Int16Array(ex.memory.buffer);

  const totalFrames = Math.floor(pcm_i16.length / FRAME_SAMPLES);
  const chunks = [];
  for (let i = 0; i < totalFrames; i++) {
    mem16.set(
      pcm_i16.subarray(i * FRAME_SAMPLES, (i + 1) * FRAME_SAMPLES),
      pcmPtr >> 1,
    );
    const n = ex.opus_encode(encPtr, pcmPtr, FRAME_SAMPLES, outPtr, MAX_PACKET_BYTES);
    if (n <= 0) {
      // n == 0: DTX (no packet emitted, treat as silence frame)
      // n  < 0: hard error — bail
      if (n < 0) console.warn(`opus_encode error ${n}`);
      continue;
    }
    // Wire format: [u16 BE length][opus packet bytes] (matches codec/mod.rs).
    const framed = new Uint8Array(2 + n);
    framed[0] = (n >> 8) & 0xff;
    framed[1] = n & 0xff;
    framed.set(mem8.subarray(outPtr, outPtr + n), 2);
    chunks.push(framed);
  }
  ex.opus_encoder_destroy(encPtr);
  ex.free(errPtr);
  ex.free(pcmPtr);
  ex.free(outPtr);

  const total = chunks.reduce((s, a) => s + a.length, 0);
  const out = new Uint8Array(total);
  let o = 0;
  for (const c of chunks) { out.set(c, o); o += c.length; }
  return out;
}

export async function opusDecodeClip(payload_u8) {
  await ready;
  const ex = opus.exports;
  const errPtr = ex.malloc(4);
  const decPtr = ex.opus_decoder_create(SAMPLE_RATE, CHANNELS, errPtr);
  let mem32 = new Int32Array(ex.memory.buffer);
  const err = mem32[errPtr >> 2];
  if (err !== 0 || decPtr === 0) {
    ex.free(errPtr);
    throw new Error(`opus_decoder_create error ${err}`);
  }

  const packetPtr = ex.malloc(MAX_PACKET_BYTES);
  const speechPtr = ex.malloc(MAX_FRAME_SAMPLES * 2);
  let mem8 = new Uint8Array(ex.memory.buffer);
  let mem16 = new Int16Array(ex.memory.buffer);

  const blocks = [];
  let i = 0;
  while (i + 2 <= payload_u8.length) {
    const len = (payload_u8[i] << 8) | payload_u8[i + 1];
    i += 2;
    if (len === 0 || i + len > payload_u8.length || len > MAX_PACKET_BYTES) {
      // Bad length header or truncated — bail rather than march into garbage.
      break;
    }
    mem8.set(payload_u8.subarray(i, i + len), packetPtr);
    const decoded = ex.opus_decode(decPtr, packetPtr, len, speechPtr, MAX_FRAME_SAMPLES, 0);
    if (decoded > 0) {
      blocks.push(mem16.slice(speechPtr >> 1, (speechPtr >> 1) + decoded));
    } else if (decoded < 0) {
      console.warn(`opus_decode error ${decoded}`);
    }
    i += len;
  }
  ex.opus_decoder_destroy(decPtr);
  ex.free(errPtr);
  ex.free(packetPtr);
  ex.free(speechPtr);

  const total = blocks.reduce((s, b) => s + b.length, 0);
  const out = new Int16Array(total);
  let o = 0;
  for (const b of blocks) { out.set(b, o); o += b.length; }
  return out;
}
