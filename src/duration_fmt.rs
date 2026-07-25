//! A single, shared rendering of a [`Duration`] for user-facing diagnostics.
//!
//! `run` and `wait` both echo a deadline/grace back to the operator in stderr
//! messages, and both want the same compact, honest text for the same value —
//! not two renderers that quietly drift (`run`'s hand-written compact form vs.
//! `wait`'s `{limit:?}` `Duration::fmt` Debug output, which is not a
//! documented, stable rendering and disagrees with `run` for values like
//! `1500ms`, printing `1.5s`). [`format_duration`] is the one contract both
//! live behind.

use std::time::Duration;

/// A compact, honest rendering of a duration for diagnostics: whole seconds when
/// it divides evenly (`5s`), otherwise milliseconds (`500ms`). Not a full
/// human-time formatter — just enough to echo the deadline/grace back clearly.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms != 0 && ms.is_multiple_of(1_000) {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compact rendering: whole seconds when the value divides evenly,
    /// milliseconds otherwise, `0ms` for zero (never the degenerate `0s`).
    #[test]
    fn format_duration_is_compact_and_honest() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_duration(Duration::ZERO), "0ms");
    }
}
