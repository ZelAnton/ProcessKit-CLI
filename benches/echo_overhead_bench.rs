//! Through-the-binary benchmark: the wall-clock overhead `run`'s pump + echo
//! (and, with `--capture-dir`, the tee into `StreamCapture::absorb`) adds over
//! a direct invocation of the same child. Drives the *built binary*
//! (`env!("CARGO_BIN_EXE_processkit-cli")`), like the through-the-binary
//! integration tests (`tests/common/mod.rs`) — the value this crate adds over
//! ProcessKit-rs is the binary plus its contracts, so that is what gets timed.
//!
//! Three scenarios per payload size: direct (baseline, no runner), under `run`
//! with plain echo, and under `run` with echo **and** `--capture-dir`. Each
//! spawns [`support::emit_bin`] (`src/bin/bench_emit.rs`), which writes an
//! exact byte count, and drains the observed stdout on a background thread —
//! required so a multi-MiB write does not block on the OS pipe buffer before
//! the child (or the runner echoing it) can finish; see
//! `support::run_and_drain`.
//!
//! Run with `cargo bench --features bench` (see `README.md`, "Benchmarks").
//! Process-spawn-bound, so the sample size below is deliberately smaller than
//! `hash_bench.rs`'s CPU-bound default.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[path = "support/mod.rs"]
mod support;

/// Payload sizes: 64KiB (a chatty-but-modest build log chunk) and 4MiB (a
/// multi-megabyte transcript, the task's stated scenario). Kept off the
/// largest end of `--capture-dir`'s ceiling
/// ([`processkit_cli::capture::CAPTURE_INFLIGHT_MAX_BYTES`] is 64MiB) so every
/// scenario here stays in the "fully captured, not truncated" regime.
const SIZES: [u64; 2] = [64 * 1024, 4 * 1024 * 1024];

fn bench_echo_overhead(c: &mut Criterion) {
    let scratch = support::Scratch::new("echo-overhead");
    let mut group = c.benchmark_group("echo_overhead");
    for &bytes in &SIZES {
        group.throughput(Throughput::Bytes(bytes));

        group.bench_with_input(BenchmarkId::new("direct", bytes), &bytes, |b, &bytes| {
            b.iter(|| support::run_and_drain(support::direct_command(bytes)));
        });
        group.bench_with_input(
            BenchmarkId::new("under_runner_no_capture", bytes),
            &bytes,
            |b, &bytes| {
                b.iter(|| support::run_and_drain(support::runner_command(&scratch, bytes, false)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("under_runner_with_capture", bytes),
            &bytes,
            |b, &bytes| {
                b.iter(|| support::run_and_drain(support::runner_command(&scratch, bytes, true)));
            },
        );
    }
    group.finish();
}

criterion_group! {
    name = benches;
    // Process-spawn-bound (not CPU-bound like hash_bench.rs): fewer samples
    // and a longer per-sample budget so the 4MiB cases still finish promptly.
    config = Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(15));
    targets = bench_echo_overhead
}
criterion_main!(benches);
