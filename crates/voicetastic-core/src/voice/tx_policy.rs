//! Voice transmit pacing + backpressure policy (sans-IO).
//!
//! The *decisions* about how to pace a voice burst — the minimum airtime gap
//! between frames and when to pause for the firmware's outbound queue — are
//! pure functions of measured elapsed time and the last reported queue depth.
//! The *waiting itself* (a tokio sleep on native, a `setTimeout` in a browser)
//! belongs to the per-platform TX worker; it calls these so every driver paces
//! identically. See [`crate::meshtastic`]'s voice TX worker for the native
//! caller.

use std::time::Duration;

/// Firmware queue low-water mark. When the device reports
/// `QueueStatus.free <= RADIO_QUEUE_LOW_WATER` the TX worker pauses until the
/// next update. Meshtastic firmware sizes its outbound queue at ~16 slots;
/// leaving a small margin prevents racing the radio into "queue full"
/// rejections (and the out-of-memory reboots that follow on long bursts).
pub const RADIO_QUEUE_LOW_WATER: u32 = 2;

/// Maximum time to wait for a fresh `QueueStatus` before proceeding anyway.
/// A safety valve for the (rare) case where the firmware never publishes
/// another update; per-frame pacing still throttles underneath.
pub const RADIO_QUEUE_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait before handing the next voice frame to the transport,
/// given how long has elapsed since the previous frame was sent.
///
/// `None` (no previous frame, i.e. the first of a burst or after a long idle)
/// and a fully-elapsed gap both yield [`Duration::ZERO`] (send immediately).
pub fn pacing_delay(elapsed_since_last: Option<Duration>, pacing: Duration) -> Duration {
    match elapsed_since_last {
        Some(elapsed) if elapsed < pacing => pacing - elapsed,
        _ => Duration::ZERO,
    }
}

/// Whether the firmware's outbound queue has room for another voice frame.
pub fn queue_has_room(free: u32) -> bool {
    free > RADIO_QUEUE_LOW_WATER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_not_delayed() {
        assert_eq!(
            pacing_delay(None, Duration::from_millis(900)),
            Duration::ZERO
        );
    }

    #[test]
    fn waits_only_the_remaining_gap() {
        let pacing = Duration::from_millis(900);
        // 300 ms already elapsed → wait the remaining 600 ms.
        assert_eq!(
            pacing_delay(Some(Duration::from_millis(300)), pacing),
            Duration::from_millis(600)
        );
        // Gap already exceeded → send now.
        assert_eq!(
            pacing_delay(Some(Duration::from_millis(1000)), pacing),
            Duration::ZERO
        );
    }

    #[test]
    fn backpressure_threshold() {
        assert!(!queue_has_room(0));
        assert!(!queue_has_room(RADIO_QUEUE_LOW_WATER));
        assert!(queue_has_room(RADIO_QUEUE_LOW_WATER + 1));
    }
}
