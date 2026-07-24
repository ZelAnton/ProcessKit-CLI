//! Microbenchmark: `StreamCapture::absorb` (`src/capture.rs`) — the per-chunk
//! count/write/hash fold every echoed byte goes through when `--capture-dir`
//! is set. Reaches `StreamCapture` directly (made `pub` for exactly this,
//! T-187 — see the doc comment on the struct) rather than through the async
//! `CaptureTee`/`AsyncWrite` plumbing, so the timed loop measures the fold
//! itself, not an executor.
//!
//! Run with `cargo bench --features bench` (see `README.md`, "Benchmarks").

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

use processkit_cli::capture::StreamCapture;

#[path = "support/mod.rs"]
mod support;

/// Absorb a fixed-size stream in various chunk sizes — the same spread
/// `hash_bench.rs`'s incremental group uses, so the two are comparable: the
/// difference between them is exactly the file-write and ceiling/truncation
/// bookkeeping `absorb` adds on top of hashing.
fn bench_absorb(c: &mut Criterion) {
    let total = 256 * 1024;
    let data = vec![0xEFu8; total];
    let scratch = support::Scratch::new("capture");

    let mut group = c.benchmark_group("capture_absorb");
    group.throughput(Throughput::Bytes(total as u64));
    for &chunk in &[1usize, 120, 4096, 65536] {
        let path = scratch.path(&format!("stream-{chunk}.log"));
        group.bench_function(format!("chunk_{chunk}B_of_{total}B"), |b| {
            // A fresh `StreamCapture` per iteration (`iter_batched`): it opens
            // (truncates) a file and owns a running hasher/counters, so
            // reusing one across iterations would silently benchmark an
            // ever-growing byte count instead of a steady-state per-run cost.
            b.iter_batched(
                || StreamCapture::new(path.clone()).expect("open bench capture file"),
                |mut cap| {
                    for piece in data.chunks(chunk) {
                        cap.absorb(black_box(piece));
                    }
                    black_box(cap);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(3));
    targets = bench_absorb
}
criterion_main!(benches);
