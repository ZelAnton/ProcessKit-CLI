//! Microbenchmark: the argv worker-shape hint classifier, `classify_hint`
//! (`src/events.rs`) — run once per `run_started` event on every invocation's
//! argv. Reaches `classify_hint` directly (made `pub` for exactly this,
//! T-187 — see its doc comment) rather than through `CommandInfo::for_argv`,
//! whose only other caller also computes the (unrelated) argv SHA-256
//! fingerprint alongside it — isolating the classifier's own cost.
//!
//! Run with `cargo bench --features bench` (see `README.md`, "Benchmarks").

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

use processkit_cli::events::classify_hint;

/// A short, ordinary argv (`cmd /c build`) — the common case, no recognized
/// shape, and the cheapest input the classifier sees.
fn ordinary_argv() -> Vec<String> {
    ["cmd", "/c", "build"].map(String::from).into()
}

/// A recognized MSBuild reusable-worker argv (`docs/schema.md`'s seed hint
/// rule) — the one shape that matches, exercising every marker check to
/// completion rather than short-circuiting on the first miss.
fn msbuild_argv() -> Vec<String> {
    [
        "C:\\Program Files\\dotnet\\sdk\\MSBuild.dll",
        "/nodemode:1",
        "/nodeReuse:true",
        "/nodeReuseLimit:8",
    ]
    .map(String::from)
    .into()
}

/// A long argv of unrelated flags — a worst case for the classifier's
/// linear substring scan (`argv.join(" ")` then `contains` per marker) at a
/// size beyond a typical build-tool invocation.
fn long_argv() -> Vec<String> {
    (0..64).map(|i| format!("--flag-{i}=value{i}")).collect()
}

fn bench_classify_hint(c: &mut Criterion) {
    let mut group = c.benchmark_group("hint_classifier");
    for (name, argv) in [
        ("ordinary", ordinary_argv()),
        ("msbuild_match", msbuild_argv()),
        ("long_no_match", long_argv()),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| black_box(classify_hint(black_box(&argv))));
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(3));
    targets = bench_classify_hint
}
criterion_main!(benches);
