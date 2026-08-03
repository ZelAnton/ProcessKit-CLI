//! Fuzz `wait --report-outcome`'s terminal-outcome read-back
//! (`src/wait.rs::read_terminal_outcome`), the newest untrusted-input parser
//! joining the fuzz tier (T-301). It opens the events file named by a registry
//! record — a path at an operator-chosen location any local process can write —
//! and reads a bounded head and tail looking for a terminal `runner_exit`, so its
//! input can legitimately be truncated mid-write, concurrently appended, or
//! arbitrary bytes.
//!
//! The two pure steps the real file-backed path composes,
//! [`processkit_cli::wait::head_matches_run_id`] and
//! [`processkit_cli::wait::scan_runner_exit_tail`], are newly exposed
//! `#[doc(hidden)] pub` by the exact same pattern `registry_record`/
//! `control_wire`/`cli_parsers` already use (K-041/K-060) — visibility is the
//! only change, behavior is untouched. This target treats fuzz input `data` as a
//! whole simulated events file and derives the identical head/tail windows
//! [`processkit_cli::wait`]'s own `read_terminal_outcome` reads from a real file
//! (first `min(len, OUTCOME_TAIL_MAX_BYTES)` bytes as the head, last
//! `min(len, OUTCOME_TAIL_MAX_BYTES)` bytes as the tail, `OUTCOME_TAIL_MAX_BYTES`
//! reused directly rather than duplicated), then calls the two steps in the same
//! order and with the same short-circuit the real read-back uses — so oversized
//! streams straddling the head/tail byte bounds are exercised exactly as they
//! would be against a real file, without touching disk. Never expected to
//! panic — an unmatched header, a truncated/interleaved/malformed tail, or
//! non-UTF-8 garbage anywhere is exactly what the real read-back already treats
//! as "no reportable outcome" (`None`), not an error.
//!
//! If a future `events` subcommand's line reader routes through the same
//! primitive (see `src/wait.rs`'s module doc), it is covered by this same target
//! for free.
#![no_main]

use libfuzzer_sys::fuzz_target;
use processkit_cli::wait::{OUTCOME_TAIL_MAX_BYTES, head_matches_run_id, scan_runner_exit_tail};

/// The `run_id` every seed corpus entry's `run_started` line names — fixed so the
/// fuzzer's mutations have a stable, learnable header to preserve while exploring
/// the tail, rather than also having to rediscover a byte-exact `run_id` string
/// from nothing.
const RUN_ID: &str = "fuzz-run";

fuzz_target!(|data: &[u8]| {
    let len = data.len() as u64;

    let head_end = len.min(OUTCOME_TAIL_MAX_BYTES) as usize;
    let head = &data[..head_end];
    if !head_matches_run_id(head, RUN_ID) {
        return;
    }

    let tail_start = len.saturating_sub(OUTCOME_TAIL_MAX_BYTES) as usize;
    let tail = &data[tail_start..];
    let _ = scan_runner_exit_tail(tail, tail_start == 0);
});
