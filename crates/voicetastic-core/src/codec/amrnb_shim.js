// Loads the vendored opencore-amr-nb standalone wasm (built by the
// `opencore-amrnb` crate via emscripten) and wraps its raw C ABI
// (Encoder_Interface_* / Decoder_Interface_*) into higher-level clip-level
// encode/decode functions that the Rust web crate calls via wasm-bindgen.
//
// The opencore wasm lives in its own linear memory, separate from the Rust
// wasm — so this shim does the byte/sample shuffling across the boundary.

const FRAME_SAMPLES = 160; // 20 ms @ 8 kHz mono
// Index = mode 0..7; total bytes per AMR-NB IF1 frame INCLUDING the 1-byte
// ToC. Matches core's `AMRNB_BYTES_PER_FRAME_LOOKUP` in codec/mod.rs.
const BYTES_PER_FRAME = [13, 14, 16, 18, 20, 21, 27, 32];

let amrnb = null;
let readyResolve;
let readyReject;
const ready = new Promise((res, rej) => { readyResolve = res; readyReject = rej; });

export async function amrnbProvideBytes(bytes) {
  try {
    const { instance } = await WebAssembly.instantiate(bytes, {
      env: { emscripten_notify_memory_growth: () => {} },
    });
    instance.exports._initialize();
    amrnb = instance;
    readyResolve();
  } catch (e) {
    readyReject(e);
    throw e;
  }
}

export async function amrnbEncodeClip(speech_i16, mode) {
  await ready;
  const ex = amrnb.exports;
  const mem8 = new Uint8Array(ex.memory.buffer);
  const mem16 = new Int16Array(ex.memory.buffer);
  const state = ex.Encoder_Interface_init(0);
  const speechPtr = ex.malloc(FRAME_SAMPLES * 2);
  const outPtr = ex.malloc(64);
  const totalFrames = Math.floor(speech_i16.length / FRAME_SAMPLES);
  const chunks = [];
  for (let i = 0; i < totalFrames; i++) {
    mem16.set(
      speech_i16.subarray(i * FRAME_SAMPLES, (i + 1) * FRAME_SAMPLES),
      speechPtr >> 1,
    );
    const n = ex.Encoder_Interface_Encode(state, mode, speechPtr, outPtr, 0);
    if (n <= 0) break;
    chunks.push(mem8.slice(outPtr, outPtr + n));
  }
  ex.Encoder_Interface_exit(state);
  ex.free(speechPtr);
  ex.free(outPtr);
  const total = chunks.reduce((s, a) => s + a.length, 0);
  const out = new Uint8Array(total);
  let o = 0;
  for (const c of chunks) { out.set(c, o); o += c.length; }
  return out;
}

export async function amrnbDecodeClip(payload_u8) {
  await ready;
  const ex = amrnb.exports;
  const mem8 = new Uint8Array(ex.memory.buffer);
  const mem16 = new Int16Array(ex.memory.buffer);
  const state = ex.Decoder_Interface_init();
  const payloadPtr = ex.malloc(64);
  const speechPtr = ex.malloc(FRAME_SAMPLES * 2);
  const blocks = [];
  let i = 0;
  while (i < payload_u8.length) {
    const toc = payload_u8[i];
    const mode = (toc >> 3) & 0x0F;
    const size = BYTES_PER_FRAME[mode];
    if (size === undefined || i + size > payload_u8.length) break;
    mem8.set(payload_u8.subarray(i, i + size), payloadPtr);
    ex.Decoder_Interface_Decode(state, payloadPtr, speechPtr, 0);
    // copy 160 samples out before the next iteration overwrites the buffer
    blocks.push(mem16.slice(speechPtr >> 1, (speechPtr >> 1) + FRAME_SAMPLES));
    i += size;
  }
  ex.Decoder_Interface_exit(state);
  ex.free(payloadPtr);
  ex.free(speechPtr);
  const out = new Int16Array(blocks.length * FRAME_SAMPLES);
  let o = 0;
  for (const b of blocks) { out.set(b, o); o += b.length; }
  return out;
}
