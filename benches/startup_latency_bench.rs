//! Through-the-binary benchmark: latency from invoking `run` to the moment it
//! stamps its `run_started` JSONL event — the container-creation/spawn cost a
//! caller waits through before it can rely on the run being underway — paired
//! with a `direct` control arm that spawns the exact same child with no
//! runner in between. Drives the *built binary*, like `echo_overhead_bench.rs`
//! and the through-the-binary integration tests.
//!
//! Absolute numbers from either arm alone are not comparable across hosts
//! (antivirus, CPU, and OS-scheduler noise dominate); the **delta between the
//! two arms on the same host** is what is publishable, since it isolates what
//! the runner itself adds over the OS's own process-creation cost for the
//! same child rather than folding both into one absolute (see `README.md`,
//! "Benchmarks", and upstream thread `msg-send-ba9dc66e1b832e104c35c9a1e75a6588`,
//! which raised exactly this attribution point).
//!
//! `under_runner` spawns `run -- bench_emit --bytes 0` (a child that writes
//! nothing and exits immediately, so its own runtime never dilutes the
//! measured startup cost) and diffs a host-side timestamp taken just before
//! `spawn` against the `time` field the runner itself stamped on
//! `run_started` (see `support::measure_startup_latency`); it uses
//! [`criterion::Bencher::iter_custom`] rather than the default closure timing,
//! since criterion would otherwise time this bench's *own* wall-clock
//! (spawn + wait + file read), not the derived event latency this bench
//! exists to report. `direct` spawns the same `bench_emit --bytes 0` with no
//! runner and times spawn-to-exit with the default closure timing (see
//! `support::run_direct_startup_child`).
//!
//! Run with `cargo bench --features bench` (see `README.md`, "Benchmarks").

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

#[path = "support/mod.rs"]
mod support;

fn bench_startup_latency(c: &mut Criterion) {
    support::self_check_calendar_math();
    let scratch = support::Scratch::new("startup-latency");
    let mut group = c.benchmark_group("startup_latency");

    group.bench_function("direct", |b| {
        b.iter(support::run_direct_startup_child);
    });
    group.bench_function("under_runner", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += support::measure_startup_latency(&scratch);
            }
            total
        });
    });

    group.finish();
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
