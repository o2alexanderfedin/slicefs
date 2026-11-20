//! Shared utility helpers for SliceFS CLI.

use std::time::SystemTime;

/// The wall-clock time as epoch seconds with milliseconds (`t=` in the probe
/// lines, so that a harness can match them to its own times).
pub(crate) fn epoch() -> String {
    let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

/// Return the current wall-clock time formatted as `HH:MM:SS.mmm` (UTC).
pub(crate) fn ts() -> String {
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}.{:03}", h, m, s, millis)
}
