//! Fuzz the registry record's bytes → parse/validate path.
//!
//! Mirrors exactly what [`processkit_cli::registry::Registry::scan`] does with
//! every `.json` file it reads from the (owner-only, but still untrusted —
//! corrupted or hand-edited) registry directory: interpret the bytes as UTF-8
//! text (a `fs::read_to_string` failure is treated as a corrupt record and
//! skipped, never a fuzz-worthy panic path — the fuzzer would only be probing
//! `str::from_utf8`'s own well-tested rejection), then run
//! [`processkit_cli::registry::parse_and_validate_record`], the pure function
//! `scan` itself calls — JSON deserialize, then the `started_at`/`lock_file`
//! corruption guards (history: NUL/control-byte and Windows-reserved-device-name
//! `lock_file` values, and calendar-invalid `started_at` values like
//! `2026-02-31`, see K-024/K-027/K-030 and the seed corpus below). Never
//! expected to panic on any input — only to return `None` for anything corrupt.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = processkit_cli::registry::parse_and_validate_record(text);
    }
});
