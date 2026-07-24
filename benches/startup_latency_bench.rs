//! Through-the-binary benchmark: latency from invoking `run` to the moment it
//! stamps its `run_started` JSONL event — the container-creation/spawn cost a
//! caller waits through before it can rely on the run being underway. Drives
//! the *built binary*, like `echo_overhead_bench.rs` and the through-the-binary
//! integration tests.
//!
//! Each iteration spawns `run -- bench_emit --bytes 0` (a child that writes
//! nothing and exits immediately, so its own runtime never dilutes the
//! measured startup cost) and diffs a host-side timestamp taken just before
//! `spawn` against the `time` field the runner itself stamped on
//! `run_started` (see `support::measure_startup_latency`). Uses
//! [`criterion::Bencher::iter_custom`] rather than the default closure timing:
//! criterion would otherwise time this bench's *own* wall-clock (spawn + wait
//! + file read), not the derived event latency this bench exists to report.
//!
//! Run with `cargo bench --features bench` (see `README.md`, "Benchmarks").

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "support/mod.rs"]
mod support;

fn bench_startup_latency(c: &mut Criterion) {
    support::self_check_calendar_math();
    let scratch = support::Scratch::new("startup-latency");

    c.bench_function("startup_latency/call_to_run_started", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += support::measure_startup_latency(&scratch);
            }
            total
        });
    });
}

criterion_group! {
    name = benches;
    // A short series of runner invocations per the task's "series of short
    // runs" scenario; process-spawn-bound like echo_overhead_bench.rs.
    config = Criterion::default()
        .sample_size(30)
        .measurement_time(Duration::from_secs(10));
    targets = bench_startup_latency
}
criterion_main!(benches);
