//! Microbenchmark: the hand-rolled incremental SHA-256 (`src/hash.rs`) — the
//! one digest primitive both the argv fingerprint and the bounded-capture
//! transcript hashing (`src/capture.rs`) build on. Reaches [`Sha256`] directly
//! (already `pub`, used elsewhere in the crate) rather than through either
//! caller, so this isolates the hasher's own cost.
//!
//! Run with `cargo bench --features bench` (see `README.md`, "Benchmarks").

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use processkit_cli::hash::{Sha256, sha256_hex};

/// One-shot hashing of buffers at a few representative sizes: a short argv
/// fingerprint (tens of bytes), a single-block-ish chunk, and a multi-block
/// buffer.
fn bench_one_shot(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_one_shot");
    for &size in &[32usize, 1024, 64 * 1024] {
        let data = vec![0xABu8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("{size}B"), |b| {
            b.iter(|| black_box(sha256_hex(black_box(&data))));
        });
    }
    group.finish();
}

/// Incremental feeding at the chunk sizes the streaming capture path
/// (`StreamCapture::absorb`) actually sees in practice: single bytes (the
/// pathological case), small (~line-sized) chunks, and large (~64KiB) chunks
/// approaching the pump's in-flight ceiling
/// ([`processkit_cli::capture::CAPTURE_INFLIGHT_MAX_BYTES`]).
fn bench_incremental(c: &mut Criterion) {
    let total = 256 * 1024;
    let data = vec![0xCDu8; total];
    let mut group = c.benchmark_group("hash_incremental");
    group.throughput(Throughput::Bytes(total as u64));
    for &chunk in &[1usize, 120, 4096, 65536] {
        group.bench_function(format!("chunk_{chunk}B_of_{total}B"), |b| {
            b.iter(|| {
                let mut hasher = Sha256::new();
                for piece in data.chunks(chunk) {
                    hasher.update(black_box(piece));
                }
                black_box(hasher.finalize_hex())
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    // Wall-clock rather than the criterion default (5s) — these are cheap,
    // CPU-bound iterations, so the default sample size converges quickly.
    config = Criterion::default().measurement_time(Duration::from_secs(3));
    targets = bench_one_shot, bench_incremental
}
criterion_main!(benches);
