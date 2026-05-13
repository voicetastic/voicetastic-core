//! Outgoing-voice retransmit registry — thin re-export.
//!
//! The implementation lives in [`voicetastic_core::voice::outgoing`] so
//! the CLI, GUI, and any future client share the same battle-tested
//! cooldown / pending-chunk dedup logic. This file exists only to
//! preserve the historical `crate::outgoing::*` paths the rest of the
//! GUI uses; new code should import directly from
//! `voicetastic_core::voice::outgoing`.

pub use voicetastic_core::voice::outgoing::OutgoingVoiceRegistry;
