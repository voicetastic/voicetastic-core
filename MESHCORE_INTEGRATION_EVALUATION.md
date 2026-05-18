# Meshcore Integration Evaluation

**Date**: 2026-05-18  
**Context**: Evaluating feasibility of adding Meshcore protocol support alongside Meshtastic in voicetastic-desktop using the new RadioService abstraction layer.

## Executive Summary

Meshcore integration is **partially compatible** with the abstraction layer. The protocol is fundamentally text-only (no voice), uses variable-length node IDs instead of fixed u32, and has different routing/queue semantics. Integrating Meshcore would require:

1. **NodeId changes** — support variable-length public key hashes, not just u32
2. **GUI refactoring** — remove voice pipeline entirely for Meshcore builds
3. **Transport differences** — Companion Radio Protocol instead of direct ToRadio/FromRadio
4. **Configuration model** — Clients/Repeaters/RoomServers vs. homogeneous Meshtastic nodes
5. **Message queue** — different offline queue semantics

**Effort estimate**: 6-8 weeks for a production-ready Meshcore implementation.

---

## Protocol Comparison

| Aspect | Meshtastic | Meshcore |
|--------|------------|----------|
| **Routing** | Flood-based broadcast | Explicit unicast + repeaters |
| **Node IDs** | Fixed u32 (0xAABBCCDD) | Variable-length hashes (1-3 bytes) |
| **Message Types** | Text, voice, admin, waypoints | Text, ACKs, advertisements only |
| **Voice Support** | Yes (Opus, AMR-NB, Codec2) | **No** |
| **Transport** | Direct ToRadio/FromRadio | Companion Radio Protocol |
| **Framing** | Protobuf + variable length | Binary compact header + length |
| **Max Frame** | 255 bytes (body) | 256 bytes (total) |
| **Encryption** | AES-256-GCM | AES-128-CBC |
| **Identity** | Admin name + node number | Ed25519-signed advertisements |
| **Node Model** | Homogeneous | Clients/Repeaters/RoomServers |
| **Queue** | Immediate delivery attempt | Offline queue sync on reconnect |

---

## Abstraction Layer Compatibility

### ✅ Compatible

1. **RadioService trait interface** — Meshcore can implement:
   - `connect_with_transport()` / `disconnect()`
   - `watch_state()` / `watch_nodes()`
   - `subscribe_text()`
   - `send_text()` with channel + recipient
   
2. **Transport trait** — Meshcore can use the existing BLE/serial transport abstraction
   
3. **Connection state machine** — Meshcore has similar states (Disconnected → Connecting → Connected → Configuring → Ready)

### ⚠️ Partially Compatible

1. **NodeId type**
   - Current: `NodeId(u32)` with Display format `!{:08x}`
   - **Change needed**: Support variable-length node IDs (1-3 bytes for Meshcore, u32 for Meshtastic)
   - **Solution**: Extend NodeId to `enum NodeId { Meshtastic(u32), Meshcore([u8; 3]) }` or use a wrapper with a tag byte
   - **Impact**: Minimal; Display/FromStr implementations handle both formats

2. **Node model abstraction**
   - Meshtastic: all nodes are equal; config applies to "this device"
   - Meshcore: explicit roles (Client sends, Repeater forwards, RoomServer aggregates)
   - **Change needed**: Extend `NodeSummary` to include optional `role: Option<MeshcoreRole>`
   - **Impact**: Minor; add fields, not breaking changes

3. **Message queue semantics**
   - Meshtastic: sender fires immediately, receiver NACKs if missing
   - Meshcore: offline queue synced on reconnect (fire-and-forget semantics differ)
   - **Change needed**: `QueueEvent` already abstracts this; Meshcore just signals differently
   - **Impact**: Low; trait is generic enough

### ❌ Incompatible

1. **Voice pipeline**
   - Meshcore has **no voice support** whatsoever
   - **Change required**: Either:
     - **(A) Separate builds**: `voicetastic-meshtastic` (with voice) and `voicetastic-meshcore` (text-only)
     - **(B) Conditional compilation**: `#[cfg(feature = "voice")]` throughout the voice module
     - **(C) Runtime opt-out**: VoiceFrameSink returns `Err(...)` for "not supported" if protocol is Meshcore
   - **Recommendation**: Option A (separate builds) — cleaner, avoids dead code in Meshcore version
   - **Impact**: **High** — voice module is ~30% of the codebase

2. **Meshtastic-specific features**
   - Admin messages (reboot, factory reset, channel config)
   - Waypoints, position sharing
   - LoRa modem presets, channel encryption
   - Device name/status customization
   - **Change required**: Remove from Meshcore builds or make optional
   - **Impact**: Medium; Settings tab becomes protocol-specific

---

## Implementation Path

### Phase 1: Core Abstraction Updates (2-3 weeks)

**Prerequisite for all Meshcore work:**

1. **Extend NodeId**
   ```rust
   pub enum NodeId {
       Meshtastic(u32),
       Meshcore([u8; 3]),
   }
   ```
   - Update Display/FromStr for both variants
   - Update NodeSummary to support both

2. **Conditional voice feature**
   ```rust
   #[cfg(feature = "voice")]
   pub mod voice { ... }
   
   #[cfg(feature = "voice")]
   impl VoiceFrameSink for MeshtasticService { ... }
   ```

3. **Protocol enum in RadioService**
   ```rust
   pub enum RadioProtocol {
       Meshtastic,
       Meshcore,
   }
   
   pub trait RadioService {
       fn protocol(&self) -> RadioProtocol;
       // ... rest unchanged
   }
   ```

### Phase 2: MeshcoreService Implementation (3-4 weeks)

1. **Transport adapter**
   - Wrap existing Transport trait
   - Implement Companion Radio Protocol framing (BLE/USB)
   - Command/response dispatch

2. **State machine**
   - Mimic Meshtastic's inbound/outbound/config pattern
   - Parse Meshcore binary protocol frames
   - Handle Ed25519 signature verification (pairing)

3. **Node discovery**
   - Handle Meshcore advertisements
   - Build NodeSummary from public key hash + alias
   - Track role (Client/Repeater/RoomServer)

4. **Text messaging**
   - Implement send_text() via Companion Protocol
   - Parse incoming text frames
   - Broadcast subscriptions

### Phase 3: GUI Adaptation (1-2 weeks)

1. **Settings tab refactoring**
   - Extract Meshtastic-specific sections (LoRa, admin, waypoints)
   - Use SettingsPanel trait (already designed)
   - Meshcore panel is minimal (time sync, device name only)

2. **Chat UI updates**
   - Remove voice compose for Meshcore builds
   - Show "Text only" badge or disable voice UI
   - Handle variable-length node IDs in display

3. **Build targets**
   ```toml
   # Cargo.toml
   [features]
   default = ["meshtastic-ui", "voice"]
   meshtastic-ui = []
   meshcore-only = []  # disables voice, meshtastic admin UI
   ```

---

## Risk Assessment

### High Risk

1. **Feature parity paradox**: Meshcore is simpler (text-only) but voicetastic is voice-first
   - **Mitigation**: Accept that Meshcore build is limited; document as "text-only companion"

2. **Node ID representation**: Changing from u32 to enum breaks many existing code paths
   - **Mitigation**: Thorough testing; wrap in helper functions (meshtastic_node_id(), meshcore_node_id())

3. **Transport incompatibility**: Companion Radio Protocol is complex; implementation bugs could brick pairing
   - **Mitigation**: Extensive loopback tests; reference the official SDK

### Medium Risk

1. **Configuration drift**: Different device models (Meshtastic vs. Meshcore radios) with overlapping UI
   - **Mitigation**: Separate SettingsPanel implementations; no shared state

2. **Node model mismatch**: Meshcore's repeater concept not in Meshtastic
   - **Mitigation**: Optional `role` field in NodeSummary; ignore for Meshtastic

### Low Risk

1. **CLI/Android changes**: Can defer; text-only messaging works fine there

---

## Recommendation

### ✅ **Proceed with Meshcore Integration IF:**

1. **Mesh network preference** — You intend to use Meshcore as the primary protocol
2. **Accept text-only** — Voice-only works on Meshtastic; Meshcore is text exclusively
3. **Separate builds** — Maintain `voicetastic-gui-meshtastic` and `voicetastic-gui-meshcore` as distinct products
4. **Testing investment** — Budget 3-4 weeks for loopback tests + hardware testing

### ⚠️ **Alternative: Defer Meshcore IF:**

1. **Production use** — Meshtastic is stable; Meshcore is nascent (2024+)
2. **Voice is core** — voicetastic's value prop is mesh voice calling; Meshcore can't deliver that
3. **Resource constraints** — 6-8 week effort for feature-limited (text-only) implementation

### 🎯 **Hybrid Approach** (Recommended)

1. **Now**: Finalize Meshtastic with RadioService (already done ✅)
2. **Q3 2026**: Stabilize and ship Meshtastic build as v1.0
3. **Q4 2026+**: Evaluate Meshcore maturity; plan Phase 1 (NodeId enum + features)
4. **2027**: Phase 2-3 if Meshcore gains adoption and stability

---

## Files That Would Change

### New files
- `crates/voicetastic-core/src/meshcore/` (mod.rs, service/mod.rs, transport.rs, protocol.rs)
- `crates/voicetastic-gui/src/ui/settings/meshcore.rs`
- `MESHCORE_PROTOCOL.md` (reference)

### Modified files
- `crates/voicetastic-core/src/node.rs` — extend NodeId enum
- `crates/voicetastic-core/src/radio_service.rs` — add protocol() method
- `crates/voicetastic-core/src/voice/mod.rs` — wrap in `#[cfg(feature = "voice")]`
- `crates/voicetastic-gui/Cargo.toml` — feature flags
- `crates/voicetastic-gui/src/app.rs` — conditionalize voice UI
- `crates/voicetastic-gui/src/watchers.rs` — handle both protocols

### Lines of code impact
- **New code**: ~4,000 lines (MeshcoreService + tests)
- **Modified code**: ~500 lines (NodeId, feature gates, protocol abstraction)
- **Tests**: ~1,500 lines
- **Total effort**: 6,000 lines across 6-8 weeks

---

## Conclusion

The RadioService abstraction layer you've built is **well-suited** for Meshcore, with minor extensions (NodeId enum, voice feature flag, protocol enum). The main challenge is **philosophical**: voicetastic is designed for voice mesh calling, but Meshcore is text-only. 

**Best path forward**:
1. ✅ Ship Meshtastic v1.0 (already architected)
2. ⏳ Monitor Meshcore adoption and maturity (2026-2027)
3. 🚀 Integrate Meshcore in v2.0 as text-only companion build if justified

The abstraction layer is future-proof; integration is achievable but not urgent.
