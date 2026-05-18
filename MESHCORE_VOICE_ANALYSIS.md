# Meshcore Voice Layer Migration Analysis

**Date**: 2026-05-18  
**Question**: Can voicetastic's voice codec pipeline be adapted to work with Meshcore?  
**Executive Answer**: Technically feasible (150-160 bytes of payload available), but architecturally misaligned and not recommended.

---

## Part 1: Technical Feasibility

### Frame Size Analysis

**Meshcore constraints:**
- Maximum frame: 256 bytes (total)
- Binary header: 5-9 bytes (protocol version, route type, payloads type, path info)
- **Available payload**: ~175-179 bytes

**Voicetastic voice frame requirements (20ms @ 20fps):**

| Codec | Bitrate | Frame Size | % of Payload |
|-------|---------|------------|--------------|
| Opus | 12 kbps | 30 bytes | 17% ✅ |
| AMR-NB | 12.2 kbps | 32 bytes | 18% ✅ |
| Codec2 | 1200 bps | 6 bytes | 3% ✅ |

**FEC (Reed-Solomon) overhead** (if 20% parity):
- Base frame: 32 bytes
- Parity data: ~8 bytes (per fragment)
- **Total per fragment**: 40 bytes ✅

**Verdict**: ✅ Voice frames fit comfortably. All three codecs + FEC headers occupy only 18-25% of available payload, leaving room for fragmentation metadata.

---

### Bandwidth vs. Real-Time Feasibility

**LoRa effective throughput by preset:**

| Preset | Effective Kbps | Voice Feasibility | Range Impact |
|--------|----------------|------------------|--------------|
| LongFast (default) | ~1 kbps | ❌ 12× too slow | 40+ km |
| LongModerate | ~2 kbps | ❌ 6× too slow | 40+ km |
| MediumFast | ~3-4 kbps | ❌ 3-4× too slow | 10-20 km |
| ShortFast | ~8-10 kbps | ⚠️ Marginal (1.3-1.2× too slow) | 1-5 km |
| ShortTurbo | ~15+ kbps | ✅ Feasible | <1 km |

**Reality check**: Voice at 12 kbps requires **ShortTurbo preset minimum**, which sacrifices range (from 40 km to <1 km). Users would only get voice in short-range, line-of-sight scenarios.

**Verdict**: ⚠️ Technically possible but practically limited to local mesh (<1 km radius).

---

## Part 2: Protocol Design Impact

### Changes Required to Meshcore

Meshcore's **binary payload type field** (4 bits) currently supports 16 types. Current allocation:

```
0x0 = TEXT
0x1 = ACK
0x2 = ADVERTISEMENT
0x3 = CONTROL
```

To add voice:

```
0x4 = VOICE_DATA
0x5 = VOICE_PARITY
0x6 = VOICE_NACK
0x7 = RESERVED (future)
```

**Voice extension header** (new, 6-8 bytes):
```
[codec: 1 byte]          // 0=Opus, 1=AMR-NB, 2=Codec2
[message_id: 2 bytes]    // Message sequencing
[chunk_index: 1 byte]    // Fragment index (0-31)
[total_chunks: 1 byte]   // Total fragments
[fec_level: 1 byte]      // Parity count
[reserved: 1 byte]       // Future use
```

**New payload layout:**
```
Meshcore binary header (5-9 bytes)
+ Voice extension header (8 bytes)
+ Voice frame data (30 bytes)
+ FEC parity (if present, ~8 bytes)
─────────────────────────────────
Total: ~51-55 bytes << 256 byte limit ✅
```

**Protocol changes**: ~200-300 lines in Meshcore core (payload type parsing, header definitions).

---

### Repeater Behavior: Critical Issue

Meshcore's **repeater model** is fundamentally incompatible with voice.

**Current repeater function**:
```
1. Receive packet
2. Parse header
3. Forward unchanged if not for me
4. Decrypt + parse only for messages destined to me
```

**Repeaters don't maintain state** — they forward opaque bytes. This works for text:
- Full message arrives in one packet
- Repeater can't corrupt it by forwarding; if corrupted, retransmit

**Voice introduces state challenges**:

| Operation | Text Model | Voice Model |
|-----------|------------|-------------|
| **Message assembly** | Single packet | Multi-packet over 400ms |
| **FEC context** | N/A | Repeater must forward all fragments for FEC to work |
| **NACK handling** | Implicit (ask for text again) | Explicit (request missing chunks) |
| **Buffering** | None needed | Repeater must buffer incomplete frames |

**Three options for repeaters:**

1. **Transparent forwarding** (no voice awareness)
   - Repeater forwards all 30-byte fragments blindly
   - ✅ No changes needed
   - ❌ FEC is useless (repeater doesn't coordinate parity data)
   - ❌ Voice becomes 50% more lossy than with FEC

2. **Smart repeaters** (voice-aware)
   - Repeater buffers voice fragments, applies FEC, reconstructs
   - ✅ Voice becomes reliable
   - ❌ Breaks Meshcore's "lightweight repeater" design
   - ❌ Repeater firmware becomes complex; battery drain increases
   - ❌ Requires firmware update to all repeaters

3. **Application-layer workaround** (voicetastic handles FEC locally)
   - Don't change Meshcore core; voicetastic requests retransmission
   - ✅ Repeaters unchanged
   - ❌ NACK storms; voice quality degrades (you've already solved this for Meshtastic!)
   - ❌ Reinvents the wheel

**Verdict**: ❌ Repeater incompatibility is a show-stopper. Either break Meshcore's architecture (option 2) or accept poor quality (option 3).

---

## Part 3: Architectural Mismatch

### Core Design Philosophy Conflict

**Meshtastic** (voicetastic's current platform):
- **Philosophy**: "Full-featured mesh; every node is equal"
- **Assumption**: Nodes process messages intelligently (recognize chunks, apply FEC, send NACKs)
- **Trade-off**: Higher CPU, higher power, more memory
- **Result**: Voice works well; voice optimization is a first-class concern

**Meshcore** (proposed platform):
- **Philosophy**: "Lightweight mesh; repeaters are simple forwarders"
- **Assumption**: Repeaters never interpret payloads; clients handle all state
- **Trade-off**: Lower CPU, lower power, minimal memory (fit on tiny MCUs)
- **Result**: Text-only design; voice would add per-repeater complexity

**Adding voice to Meshcore violates its core contract** — it forces repeaters to become "intelligent" and stateful, which defeats the purpose of the protocol.

---

## Part 4: Implementation Cost

If you decided to go forward anyway, here's the effort:

### Minimal Path (voicetastic-only, transparent repeaters)

1. **Meshcore protocol extension** (300 lines)
   - Payload type enum extensions
   - Header parsing for voice frames
   - ~2-3 weeks

2. **MeshcoreService voice adapter** (800 lines)
   - VoiceSender::enqueue_voice_frame_with_id() → Meshcore
   - subscribe_voice_data() from Meshcore frames
   - Reassembly buffer
   - ~3-4 weeks

3. **NACK handling** (500 lines)
   - Subscribe to voice NACKs
   - Retransmit logic (you've already solved this!)
   - ~1-2 weeks

4. **Testing** (1200 lines)
   - Loopback tests for fragmentation
   - FEC verification
   - NACK storm handling (again!)
   - ~3-4 weeks

**Total**: 10-13 weeks for a voice-over-Meshcore implementation that:
- ✅ Fits frame sizes
- ✅ Meets Meshcore protocol requirements
- ❌ Still requires fast (ShortTurbo) preset for viability
- ❌ Provides worse voice quality than Meshtastic (repeaters can't help with FEC)
- ❌ Requires reinventing NACK resilience you've already built

### Full Path (modify Meshcore core)

If you wanted "smart repeaters" to actually use FEC:
- Protocol redesign: 2-3 weeks
- Repeater firmware: 6-8 weeks (per radio model)
- Testing + certification: 4-6 weeks
- **Total**: 4-5 months for a **fork** of Meshcore that's incompatible with existing repeaters

---

## Part 5: Recommendation

### Why NOT to Migrate Voice to Meshcore

| Factor | Impact |
|--------|--------|
| **Range loss** | ShortTurbo preset required → <1 km range (40 km with text) |
| **Repeater conflict** | Transparent forwarding breaks FEC; smart repeaters break Meshcore philosophy |
| **Effort** | 10-13 weeks + ongoing dual-protocol maintenance |
| **Quality** | Voice without smart FEC repeaters = 50% more lossy than Meshtastic path |
| **User value** | "Mesh voice calling over 1 km" vs. "40 km text messaging" |
| **Maintenance burden** | Two voice implementations; NACK logic duplicated |

### What You Should Do Instead

**Timeline:**

**Now (✅ Done)**:
- Ship voicetastic-meshtastic v1.0 with optimized voice
- RadioService abstraction is clean and ready

**2027 (if Meshcore adoption grows)**:
- Evaluate real-world Meshcore deployments
- Ship voicetastic-meshcore v2.0 as **text-only companion**
- Feature flag (`--features meshcore`) disables voice module
- Single codebase, two builds, zero voice duplication

**If voice-over-Meshcore becomes a user need** (unlikely):
- Benchmark actual deployments
- Propose protocol extension to Meshcore maintainers
- Contribute upstream rather than fork
- Accept 10-13 week investment **after** validating user demand

---

## Conclusion

**Is voice technically possible on Meshcore?** Yes, but barely:
- Payload fits (✅ 30-40 bytes available vs. 32-byte frame)
- Bandwidth doesn't (❌ requires 12× faster preset = 40× range loss)
- Repeaters don't cooperate (⚠️ FEC becomes useless without smart forwarding)

**Is it worth doing?** No, for four reasons:
1. Architectural mismatch (violates Meshcore's lightweight design)
2. Range trade-off (users lose 40 km to gain 1 km voice)
3. Engineering cost (10-13 weeks for inferior quality)
4. Maintenance burden (duplicate NACK logic, dual-protocol support)

**Bottom line**: Meshcore is a text-only protocol by design, not accident. Voice retrofitting would break that design without delivering compelling benefits. Keep voice on Meshtastic. Keep Meshcore text-only in separate build. Ship v1.0 now.

---

## See Also

- `MESHCORE_INTEGRATION_EVALUATION.md` — Protocol comparison & integration roadmap
- voicetastic-core `voice/sender.rs` — Current Meshtastic voice implementation (don't rewrite this for Meshcore)
