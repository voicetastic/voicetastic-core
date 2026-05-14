# Voicetastic Desktop — TODO

Roadmap of substantive improvements identified during the 2026-05-09 review.
Ordered roughly by user impact / payoff.

## High impact

- [ ] **Auto-reconnect with backoff**
  Supervise the BLE/serial connection inside `MeshService`. On disconnect,
  retry the last-known address/port with exponential backoff (e.g. 1s → 30s,
  capped) until the user explicitly disconnects. BLE devices flap constantly;
  long-running `text listen` / `voice listen` and the GUI both suffer today.
  Touches: `crates/voicetastic-core/src/service/mod.rs`,
  `crates/voicetastic-core/src/service/transport.rs`.

- [ ] **Channel encryption (AES256-CTR)**
  `service::inbound::handle_packet` currently drops every `MeshPacket` whose
  `payload_variant` is `Encrypted` instead of `Decoded`. Implement the
  Meshtastic AES-CTR scheme (PSK + packet id/from as nonce) so non-default
  channels are usable. Without this the app is effectively limited to the
  unencrypted default channel.
  Touches: `crates/voicetastic-core/src/service/inbound.rs`, new module e.g.
  `crates/voicetastic-core/src/crypto.rs`.

- [ ] **ACK / delivery tracking**
  `send_text` / `send_data` set `want_ack=true` for DMs but discard the
  result. Decode `Routing` admin payloads on inbound packets and route them
  to a `pending_ids: HashMap<u32, oneshot::Sender<AckResult>>`. Surface ✓ /
  ✓✓ / ❌ in the GUI chat log; let the CLI exit non-zero on undelivered DMs.
  Touches: `service/inbound.rs`, `service/outbound.rs`,
  `crates/voicetastic-gui/src/state.rs`, chat UI.

- [ ] **Live audio capture + playback**
  Today `voice` is `.amr` file in / `.amr` file out. Add `cpal` capture, an
  AMR-NB encoder/decoder (or Opus behind a feature flag with a wire-
  incompatible v2 protocol), and a record/play button in the GUI. Biggest
  user-visible feature; also the most work.
  New crate or feature: `voicetastic-audio`.

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

- Picking three for maximum impact: auto-reconnect, channel encryption,
  ACK tracking. Together they turn the app from "wire-compatible demo" into
  "daily driver that handles a real mesh".
- Live audio is the most user-visible feature but also the most work; treat
  it as its own milestone.
