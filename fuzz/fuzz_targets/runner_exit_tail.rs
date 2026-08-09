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
//! [`processkit_cli::wait::scan_runner_exit_tail`], did not exist on BASE —
//! they are newly **extracted** from `read_terminal_outcome`'s body (the head
//! check, the `usable`-window computation, the partial-first-line drop, and
//! the reverse tail scan) and exposed `#[doc(hidden)] pub`. This is the same
//! technique T-186 used to expose `registry_record`'s
//! `parse_and_validate_record` (extracted from `Registry::scan`'s inline
//! guards) and `control_wire`'s `classify_request`/`RequestVerb` (extracted
//! from `serve_one`'s inline `match request.trim()`); of those three
//! precedents, only `cli_parsers` (K-041/K-060) was visibility-only, i.e.
//! made `#[doc(hidden)] pub` without being extracted from surrounding code.
//! They stay two functions rather than merging into one bytes-in function so
//! the real path's short-circuit is preserved — the tail is never read when
//! the head doesn't match `expected_run_id` — which is also why this target
//! below `return`s on a failed `head_matches_run_id` check before ever
//! touching `scan_runner_exit_tail`. The extraction is behavior-preserving:
//! verified line-by-line against `read_terminal_outcome`'s pre-extraction
//! body and covered by the existing
//! `terminal_outcome_reader_finds_the_last_runner_exit_in_a_bounded_tail`
//! test (`src/wait.rs`), which exercises both the `start != 0` window and the
//! partial-first-line drop. This target treats fuzz input `data` as a
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
//! Scope note: the `events` subcommand, which this target's original doc comment
//! anticipated as a possible second consumer, landed reading streams a different
//! way and deliberately does **not** route through these primitives — it walks the
//! whole stream incrementally, handing out complete lines as a file grows, rather
//! than scanning a bounded head/tail window for one terminal event (see
//! `src/events_cmd/mod.rs`). So this target covers `wait --report-outcome`'s
//! read-back only, and its coverage does not extend to `events`. That gap is
//! recorded where a reader looks for coverage claims rather than only here:
//! `docs/threat-model.md` names the `events` reader as an untrusted-input surface
//! and says in as many words that this tier does not reach it, and
//! `CONTRIBUTING.md`'s "Fuzzing" section says the same — so nothing in this
//! repository claims a fifth target that does not exist.
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
    // Mirror `read_terminal_outcome`'s own boundary check (T-322): the window's
    // first line is complete not only when the window is the whole stream, but
    // also when the seek that produced it happened to land exactly after a `\n`.
    let tail_starts_at_line_boundary = tail_start == 0 || data[tail_start - 1] == b'\n';
    let _ = scan_runner_exit_tail(tail, tail_starts_at_line_boundary);
});
