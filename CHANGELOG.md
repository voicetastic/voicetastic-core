# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release notes for past `vX.Y.Z` tags are available in the project's
[GitLab Releases](../../-/releases) page.

## [Unreleased]

### Added

- **FEC + NACK aggressiveness as settings.** Two new keys on
  `SettingsApi`:
  - `voice.fec_mode` (`auto` / `off` / `light` / `medium` / `heavy`).
    `auto` (default) picks parity by destination and modem preset: 50 %
    for broadcast, 33 % for long-range unicast, 20 % medium, 0 % short.
    Manual modes apply a flat percentage of `total_data`. Resolved at
    `build_message` time via `VoiceFecMode::resolve(broadcast, preset,
    total_data)`.
  - `voice.nack_mode` (`auto` / `off` / `conservative` / `aggressive`).
    `auto` picks `nack_window` + backoff exponent + round cap by modem
    preset. `aggressive` uses 1.5 s windows + 2× backoff; `conservative`
    uses 4× preset-pacing windows + 3× backoff; `off` disables NACK
    emission entirely.
- **Broadcast suppresses NACK unconditionally.** The assembler's
  `tick()` short-circuits NACK emission when the in-progress entry's
  destination is the broadcast address — multiple receivers all NACK'ing
  the same chunks would flood the sender. Broadcast still benefits from
  FEC + partial-on-timeout. The `voice.nack_mode` setting cannot
  re-enable broadcast NACK; the override is silently ignored.
- `AssemblerConfig.nack_backoff_base`: configurable backoff exponent for
  NACK rounds (was hardcoded `3`). `2` doubles per round, `3` triples,
  `0` disables NACK emission entirely (used by the `Off` mode and as
  the broadcast short-circuit signal).
- GUI Voice settings card grows dropdowns for both modes.

### Changed (wire-incompatible)

- **Voice protocol bumped v2 → v3.** Body envelope encryption (AES-256-GCM
  with HKDF-derived per-message key) and the keyed-MAC variant of the
  trailing header tag are both **removed**. Confidentiality now relies
  exclusively on Meshtastic's channel encryption; the 4-byte trailing
  header tag is always unkeyed `SHA-256(header[0..12])[..4]` and serves
  as an integrity check against on-air bit-flips (Meshtastic uses
  AES-CTR which is bit-flip malleable). The deprecated flag bits — `0x20`
  (encrypted) and `0x08` (mac_keyed) — are now reserved-zero; V3 parsers
  reject frames that set them. Sender and receiver must be upgraded
  together. Net wire savings: ~28 octets per chunk (12-byte GCM nonce +
  16-byte tag) ≈ 14% airtime back on the smallest presets.

  Affected public APIs:
  - `voicetastic-core`: `BuildConfig` no longer carries `encryption` or
    `mac_key`; `SendRequest` drops `encryption`, `mac_key`, `channel_psk`,
    `from_node_num`; `AssemblerConfig` drops `channel_psk`;
    `VoiceMessage` drops `encrypted`; `ChunkHeader` drops `encrypted`
    and `mac_keyed`; the `crypto` module is gone; `mac::compute_tag`
    drops its `key` parameter.
  - Android bridge UniFFI surface drops `channel_psk` / `from_node_num`
    from `BuildConfig`, `NackConfig`, `AssemblerConfig`, `SendRequestUdl`,
    and `encrypted` from `VoiceMessageOut`. Six error variants
    (`BadTag`, `BodyTooShortForEnv`, `EncryptedNack`, `EncryptedNoPsk`,
    `BadFromForEncrypted`, `MacKeyMissing`) are gone. Kotlin consumers
    will need a coordinated rebuild.

### Added

- User-configurable Opus encoder settings: bitrate
  (`voice.opus_bitrate_kbps`, 6..=64 kbps, default 12) and bandwidth
  (`voice.opus_bandwidth`, `narrow` SILK 8 kHz / `wide` SILK 16 kHz,
  default `wide`). Both are exposed in the GUI Voice settings card and
  via `settings get/set` on every front-end. The bitrate now travels in
  the protocol header's `codec_param` byte (kbps), so receivers can
  label inbound clips with the actual encoder bitrate; the bandwidth
  is sender-only since the Opus bitstream self-describes per packet.
  Full-band and super-wide modes are intentionally not exposed (no
  benefit for LoRa voice).
- Voice protocol header MAC (4-byte trailer): keyed
  `HMAC-SHA256(channel_psk, header[0..12])[..4]` when a PSK is
  configured, unkeyed `SHA-256(header[0..12])[..4]` otherwise.
  Selected by the new `mac_keyed` flag bit (`0x08`). New
  [Header-MAC-Future-Work wiki page](https://github.com/voicetastic/voicetastic-core/wiki/Header-MAC-Future-Work)
  enumerates per-sender / per-message key-scoping options for future
  work.

### Changed

- **Wire-incompatible**: voice protocol bumped v1 → v2.
  `PROTOCOL_VERSION = 0x02`, `HEADER_SIZE = 16` (12 logical bytes +
  4-byte MAC). Reserved flag mask narrowed from `0x0F` to `0x07`.
  Sender and receiver must be upgraded together.
- FEC is now opt-in: `SendRequest::parity_count` defaults to `0`.
  Callers that want forward-error-correction must set it explicitly.
- NACK give-up bound now uses consecutive `nack_rounds` (resets on
  every accepted shard) instead of a cumulative counter, so healthy
  slow-trickle messages no longer falsely trip the round cap.

### Fixed

- Sender retransmit budget aligned with the receiver's NACK ceiling:
  `MAX_RETRANSMITS_PER_MESSAGE` widened from `u8 = 32` to `u16 = 2_400`
  (and `OutgoingVoice::retransmits` to `u16`) so the sender can keep
  honouring NACKs for the full receiver-side worst case (3600 s slider
  / 1.5 s window ≈ 2400 rounds). Previously the sender's 32-batch cap
  tripped long before the receiver gave up, leaving the receiver
  NACKing into silence on slow LoRa presets.
- `AssemblerConfig::sync_nack_cap_to_timeout()` ties
  `max_nack_rounds` to `ceil(message_timeout / nack_window)`, and is
  now called everywhere a host-driven setting feeds the assembler
  (GUI constructor, GUI reassembly-timeout listener, GUI LoRa-preset
  watcher, CLI `voice listen`). The user-configured reassembly
  timeout (10 s..=3600 s) is therefore the real ceiling regardless
  of which preset is active or how large `nack_window` ends up.
- `NACK_MAX_ROUNDS` raised from `32` to `400` (and widened from `u8`
  to `u16`) so the consecutive-silence budget
  (`NACK_MAX_ROUNDS × NACK_WINDOW_MS = 600 s`) reaches the default
  `AssemblerConfig::message_timeout` of 600 s. Previously the round
  cap tripped after only ~48 s of consecutive silence and produced
  spurious "voice message (partial: N/M chunks)" finalizes on slow
  LoRa presets where inter-chunk gaps can exceed a few seconds.
  `AssemblerConfig::max_nack_rounds` and the bridge-side mirror are
  now `u16`; the diagnostic `OutboundNack::round` field too.
- Inbound voice frames were silently dropped before the assembler in
  both the GUI watcher and the CLI listen loop because the version
  gate hard-coded `Some(0x01)`. Replaced with the `PROTOCOL_VERSION`
  constant.

### Removed
