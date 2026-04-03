//! Shared utility helpers for SliceFS CLI.

use std::time::SystemTime;

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
