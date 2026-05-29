# Voicetastic Desktop — TODO

Roadmap of substantive improvements identified during the 2026-05-09 review.
Ordered roughly by user impact / payoff.

## High impact

- [x] **Auto-reconnect with backoff**
  Implemented for both serial and BLE in
  `crates/voicetastic-core/src/meshtastic/service/mod.rs`. A single
  `ReconnectConfig` enum (Serial/Ble) is captured at first-connect time,
  cleared by explicit `disconnect()`. The reconnect watcher loop retries
  the last-known transport with exponential backoff (1 s → 30 s, capped)
  until the inbound stream is alive again or the user manually
  disconnects. Triggered both by the silence-probe path (serial USB
  endpoint stall) and by inbound EOF (BLE link drop). Long-running
  `text listen` / `voice listen` and the GUI now ride out flaps.

- [x] **Verify whether the firmware ever delivers `Encrypted` packets
  addressed to us, and decrypt PKC DMs if so**
  Verified against firmware: yes, the phone-API forwards `Encrypted`
  packets whose `to == my_node_num` whenever the radio's own decrypt
  failed — typically a PKC DM whose sender's public key wasn't in the
  radio's nodeDB at decrypt time. Path: `Router::handleReceived` →
  `perhapsDecode` (fails) → `MeshModule::callModules` →
  `RoutingModule::handleReceivedProtobuf` → `service->handleFromRadio`.
  Implemented host-side rescue in
  `crates/voicetastic-core/src/meshtastic/pkc.rs`:
  X25519-ECDH + SHA-256 + AES-256-CCM(8) matching the firmware's
  `CryptoEngine::decryptCurve25519`. The `Config::Security` event
  captures `private_key` into `ProtocolState`; `try_pkc_decrypt` in
  `protocol.rs` is invoked from the `Encrypted` arm of `decode_packet`
  when `to == my_node_num`. Includes the firmware's own `test_PKC`
  test vector as a ground-truth integration test. Overheard PKC DMs
  (not addressed to us) are now dropped at TRACE rather than DEBUG.

- [x] **ACK / delivery tracking**
  Implemented end-to-end. New `crates/voicetastic-core/src/meshtastic/ack.rs`
  exposes `AckResult` + `AckHandle`; `MeshtasticService` keeps a
  `pending_acks: HashMap<u32, oneshot::Sender<AckResult>>` swept on
  every registration. `send_text_tracked` / `send_data_tracked`
  register a slot before the packet leaves the host so there's no
  race. The inbound decoder routes ROUTING_APP packets to
  `InboundEvent::AckOrNak { request_id, result }`; the driver signals
  the matching slot. CLI `text send --to` waits 30 s for delivery and
  exits non-zero on Failed / TimedOut. GUI's `ChatEntry` carries a
  `delivery: Option<DeliveryStatus>` and renders ⏳ / ✓ / ❌ / ⏱
  next to outgoing DMs.

- [x] **Live audio capture + playback**
  Shipped behind the GUI's `audio` feature (default-on). `cpal` capture
  feeds the codec pipeline (AMR-NB / Opus / Codec2 / PCM); the chat UI's
  `VoiceCompose` state machine drives record / send / play with a Drop-
  safe `Recorder` and a streaming `PlaybackHandle`. Codec, bitrate,
  bandwidth, denoise are all settings-driven.
  Lives in: `crates/voicetastic-gui/src/audio.rs`,
  `crates/voicetastic-gui/src/ui/chat.rs`,
  `crates/voicetastic-core/src/codec/`.

## Medium impact

- [ ] **Persistence layer** (`$XDG_DATA_HOME/voicetastic/`)
  - chat history scrollback across runs
  - known-nodes cache (offline directory)
  - channel/PSK cache so settings tab populates before first config burst
  - retry queue for unACKed DMs
  Suggested: SQLite via `rusqlite`, or JSONL append-only as a first cut.
  Also bounds the in-memory `chat_log` (currently unbounded).

- [ ] **Cap `chat_log` size** (interim, until persistence lands)
  `SharedState::chat_log: Vec<ChatEntry>` grows forever. Switch to
  `VecDeque` capped at ~1000, drop from the front.
  Touches: `crates/voicetastic-gui/src/state.rs`,
  `crates/voicetastic-gui/src/watchers.rs`.

- [ ] **Forward-compatible `AppSettings::save`**
  `ui/devices.rs` builds a fresh `AppSettings { last_device, .. }` on every
  Connect, dropping any future fields. Load → mutate → save.
  Touches: `crates/voicetastic-gui/src/ui/devices.rs`.

- [ ] **GUI architecture: snapshot-per-frame**
  Replace per-card `shared.lock()` calls with a single snapshot at the top
  of `App::update()` and a `Vec<Mutation>` applied at end-of-frame. Reduces
  contention with watcher tasks and makes dirty/clean logic testable
  without re-implementing the watcher in unit tests.
  Touches: `crates/voicetastic-gui/src/app.rs`, all of
  `crates/voicetastic-gui/src/ui/settings/`.

- [ ] **Structured tracing spans**
  Instrument `connect → configuring → ready` lifecycle, voice message
  lifecycle (`message_id`, `total`, `received`, `bytes`), and admin writes
  (`request_id` ↔ `ack`). `tracing-subscriber` is already a dep; just needs
  `#[instrument]` on service methods.

- [x] **CI pipeline**
  Both GitLab CI (`.gitlab-ci.yml`) and GitHub Actions (`.github/workflows/ci.yml`) are
  set up. Linux matrix that runs `fmt --check`, `clippy -D warnings`, `test --workspace`,
  and a release build. Catches submodule-init regressions and toolchain breaks.

- [ ] **Fuzz inbound decode path**
  `cargo-fuzz` target on `FromRadio::decode` + `MeshService::handle_from_radio`.
  This is the untrusted-bytes entry point; high leverage for stability.

- [ ] **Property tests for the voice protocol**
  `proptest` round-trip: arbitrary AMR bytes + arbitrary loss/reorder
  patterns through `VoiceChunker` ↔ `VoiceAssembler`. Already well unit-
  tested but properties would catch edge cases the hand-written tests miss.

## Lower impact / cleanup

- [ ] **`refresh_config` ordering**
  Clear section snapshots only after `send_want_config` succeeds, so a
  failed refresh doesn't blank the UI.
  `crates/voicetastic-core/src/service/mod.rs::refresh_config`.

- [ ] **CLI BLE pre-scan diagnostics**
  When the 10s scan deadline passes without spotting the device, surface a
  clear "device not seen during scan" error instead of letting the
  subsequent `peripheral_by_address` produce a generic "no peripheral"
  message. Consider making the scan window configurable.
  `crates/voicetastic-cli/src/connect.rs`.

- [ ] **Reject empty stdin text in CLI**
  `text send` with no `--message` and EOF on stdin currently transmits an
  empty body. Reject before sending.
  `crates/voicetastic-cli/src/main.rs`.

- [ ] **Single config-watchdog token**
  Stacked `spawn_config_watchdog` tasks on rapid Refresh clicks are no-ops
  but messy. Replace with a shared cancel token / generation counter.

- [ ] **`hex_to_bytes` prefix handling**
  `clean.replace("0x", "")` strips the substring anywhere, not just the
  leading prefix. Use `strip_prefix` on the trimmed input.
  `crates/voicetastic-gui/src/ui/settings/widgets.rs`.

- [ ] **Channel Apply when `settings` is None**
  `next.settings.unwrap_or_default()` would silently overwrite missing
  channel settings with empty defaults. Disable Apply in that case.
  `crates/voicetastic-gui/src/ui/settings/channels.rs`.

- [ ] **Typed error for serial double-subscribe**
  `SerialConnection::subscribe_inbound` returns `Error::Other` on second
  call; add a dedicated variant (`Error::AlreadySubscribed`).

- [ ] **Voice broadcast lag warning in GUI**
  `watchers.rs` swallows `RecvError::Lagged` silently for the voice stream.
  CLI logs it; GUI should too.

- [ ] **Consistent closure shapes in settings cards**
  Some `card<T>` getters move `Copy` configs (`|s| s.power`), others clone
  (`|s| s.lora.clone()`). Pick one for readability.

- [ ] **`eframe::App` impl signature**
  Currently uses `ui(&mut self, ui, frame)`; consider migrating to the more
  conventional `update(ctx, frame)` form for forward-compat with eframe
  bumps.

## Notes

- The four originally-flagged high-impact items (auto-reconnect, PKC
  decrypt, ACK tracking, live audio) all shipped. Remaining work is
  concentrated in the "Medium impact" persistence layer and GUI
  architecture cleanups, plus the smaller polish items below.
