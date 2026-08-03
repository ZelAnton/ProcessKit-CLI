//! Phase-attribution benchmark for the *mutating* registry open — the one
//! per-invocation cost `run` still pays and no read-only client
//! (`list`/`prune`/`wait`/`events`, and the `inspect`/`cancel`/`kill` control
//! clients) pays at all: T-174 routed the ones that existed then through
//! `Registry::open_read_only`, and every read-only client added since has been
//! born on that path.
//!
//! `startup_latency_bench.rs` measures call-to-`run_started` end to end; it
//! cannot say *which* phase inside that window costs what. This bench isolates
//! the "owner-only registry open" phase — `Registry::open_in` ->
//! `platform::create_owner_only_dir` — and attributes it against three controls:
//!
//! - `hardening_write` — the same open against a registry directory that exists,
//!   holds records, and has **not** been hardened yet, so the permission write
//!   really runs. This is what every invocation cost before T-309, and what the
//!   repair branch still costs today.
//! - `dir_create_only` — a bare `fs::create_dir_all` on an already-existing
//!   directory: everything `create_owner_only_dir` does *except* asserting the
//!   permissions.
//! - `read_only_open` — `Registry::open_read_only_in`, which touches nothing. The
//!   floor a reader pays, i.e. what `run` would pay if the phase were free.
//!
//! Every group is swept over registry sizes (`entries=<n>`, each entry a
//! `.json` record plus its sibling `.lock`, exactly the pair a remembered run
//! leaves behind). That sweep is the point: on Windows the hardening ACE is
//! inheritable (`OICI`) and applied to a *directory*, so `SetNamedSecurityInfoW`
//! re-propagates it over the directory's existing children — a cost that would
//! grow with the number of leftover records rather than staying a constant
//! metadata write. Measuring an empty registry alone cannot tell the two apart;
//! measuring both confirms or rules out the propagation effect instead of
//! assuming it. `first_open` additionally covers the create path (a registry
//! directory that does not exist yet), which is paid once per user rather than
//! once per invocation.
//!
//! The `owner_only_open` vs `hardening_write` pair is also the regression canary
//! for T-309's verify-then-repair fast path: the two collapsing onto each other
//! means the verify stopped recognizing an already-correct directory and every
//! `run` is paying the propagating write again.
//!
//! Beyond criterion's own statistics this prints a compact attribution table
//! (medians per registry size) before the timed groups, so a plain `cargo bench
//! --features bench --bench registry_open_bench` answers the growth question
//! directly — including in the CI `perf` job's plain-text step summary, which
//! publishes stdout. `hardening_write` lives only in that table: each of its
//! samples needs a freshly built, freshly unhardened registry, which criterion's
//! iteration model cannot supply without timing the setup too — and at a
//! hundreds-of-milliseconds effect size against a sub-millisecond one, a median
//! of a few samples settles the question that statistical rigor would only
//! restate.
//!
//! Run with `cargo bench --features bench` (see `README.md`, "Benchmarks").

use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use processkit_cli::registry::Registry;
use processkit_cli::registry::test_support::write_stale_entry;

#[path = "support/mod.rs"]
mod support;

/// Registry sizes swept by every group: an empty registry, and three realistic
/// counts of leftover records. A registry only holds entries for runs that were
/// remembered and never pruned, so the interesting range is "a handful" to
/// "nobody has run `prune` in a long while", not millions.
const ENTRY_COUNTS: [usize; 4] = [0, 64, 256, 1024];

/// Samples per phase in the printed attribution table. Small on purpose: the
/// table exists to answer "does the cost grow with registry size", which a
/// median over a handful of samples settles; criterion's own groups below carry
/// the rigorous statistics.
const PROBE_SAMPLES: usize = 9;

/// Samples for the `hardening_write` column. Fewer still, because each one has
/// to rebuild a whole unhardened registry first — and the effect it measures is
/// two to three orders of magnitude above the noise floor.
const WRITE_SAMPLES: usize = 3;

/// Populate `dir` with `entries` record/lock pairs — the `.json` + `.lock` pair a
/// remembered run leaves behind, written through the real serializable record
/// type so a format change cannot leave this fixture silently stale. Creates the
/// directory if needed, **without** hardening it.
fn populate(dir: &Path, entries: usize) {
    fs::create_dir_all(dir).expect("create the registry fixture directory");
    for index in 0..entries {
        write_stale_entry(
            dir,
            &format!("run-fixture-{index:08}"),
            &format!("run-{index}"),
        );
    }
}

/// A registry directory pre-populated with `entries` record/lock pairs and
/// already hardened once, so a timed `Registry::open_in` measures the *steady
/// state* every subsequent `run` invocation pays — not the one-off creation.
struct Fixture {
    dir: PathBuf,
    entries: usize,
}

impl Fixture {
    fn new(root: &Path, entries: usize) -> Self {
        let dir = root.join(format!("entries-{entries}"));
        populate(&dir, entries);
        // Harden once up front: the steady-state cost is what `run` pays on a
        // registry directory that already exists.
        drop(Registry::open_in(dir.clone()).expect("pre-harden the registry fixture"));
        Self { dir, entries }
    }
}

/// One timed `Registry::open_in` on an existing directory — the whole
/// "owner-only registry open" phase (`fs::create_dir_all` + permission
/// hardening).
fn time_owner_only_open(dir: &Path) -> Duration {
    let path = dir.to_path_buf();
    let start = Instant::now();
    let registry = Registry::open_in(path).expect("owner-only registry open");
    let elapsed = start.elapsed();
    black_box(&registry);
    elapsed
}

/// One timed `fs::create_dir_all` on the same existing directory — the phase
/// with the permission hardening subtracted out.
fn time_dir_create_only(dir: &Path) -> Duration {
    let start = Instant::now();
    let created = fs::create_dir_all(dir);
    let elapsed = start.elapsed();
    created.expect("create_dir_all on an existing directory");
    elapsed
}

/// One timed `Registry::open_read_only_in` — the reader's floor (no filesystem
/// contact at all).
fn time_read_only_open(dir: &Path) -> Duration {
    let path = dir.to_path_buf();
    let start = Instant::now();
    let registry = Registry::open_read_only_in(path);
    let elapsed = start.elapsed();
    black_box(&registry);
    elapsed
}

/// One timed *first* open: a directory that does not exist yet, created and
/// hardened from scratch. Creation and cleanup stay outside the timed window.
fn time_first_open(root: &Path, sequence: u64) -> Duration {
    let dir = root.join(format!("first-open-{sequence}"));
    let _ = fs::remove_dir_all(&dir);
    let elapsed = time_owner_only_open(&dir);
    let _ = fs::remove_dir_all(&dir);
    elapsed
}

/// One timed open of a registry directory that exists, already holds `entries`
/// records, and has **not** been hardened — so the permission write actually
/// runs, over exactly those children. Building and removing the fixture stays
/// outside the timed window.
///
/// This is the pre-T-309 per-invocation cost, and the cost the repair branch
/// still pays whenever a pre-existing directory's permissions do not match.
fn time_hardening_write(root: &Path, sequence: usize, entries: usize) -> Duration {
    let dir = root.join(format!("unhardened-{entries}-{sequence}"));
    let _ = fs::remove_dir_all(&dir);
    populate(&dir, entries);
    let elapsed = time_owner_only_open(&dir);
    let _ = fs::remove_dir_all(&dir);
    elapsed
}

/// The median of `samples` calls to `measure`, in microseconds — deliberately
/// the median rather than the mean, so one scheduler hiccup cannot move the
/// number the attribution table reports.
fn median_micros(samples: usize, mut measure: impl FnMut() -> Duration) -> f64 {
    let mut taken: Vec<f64> = (0..samples)
        .map(|_| measure().as_secs_f64() * 1e6)
        .collect();
    taken.sort_by(|a, b| a.partial_cmp(b).expect("no NaN durations"));
    taken[taken.len() / 2]
}

/// Print the attribution table: for each registry size, the steady-state open,
/// the same open when the permission write really runs, and the two controls.
/// The `hardening_write` column is the question this bench exists to answer —
/// whether the permission share is a constant metadata write or grows with the
/// number of records the inheritable ACE has to propagate over — and the gap
/// between it and `owner_only` is what T-309's verify-then-repair fast path
/// removes from `run`'s per-invocation cost.
fn print_attribution(root: &Path, fixtures: &[Fixture]) {
    println!();
    println!(
        "registry_open attribution (median us over {PROBE_SAMPLES} samples, {WRITE_SAMPLES} for hardening_write)"
    );
    println!(
        "{:>8}  {:>16}  {:>16}  {:>14}  {:>14}",
        "entries", "owner_only", "hardening_write", "dir_create", "read_only"
    );
    for fixture in fixtures {
        let owner_only = median_micros(PROBE_SAMPLES, || time_owner_only_open(&fixture.dir));
        let mut sequence = 0;
        let hardening_write = median_micros(WRITE_SAMPLES, || {
            sequence += 1;
            time_hardening_write(root, sequence, fixture.entries)
        });
        let dir_create = median_micros(PROBE_SAMPLES, || time_dir_create_only(&fixture.dir));
        let read_only = median_micros(PROBE_SAMPLES, || time_read_only_open(&fixture.dir));
        println!(
            "{:>8}  {:>16.1}  {:>16.1}  {:>14.1}  {:>14.1}",
            fixture.entries, owner_only, hardening_write, dir_create, read_only
        );
    }
    println!();
}

fn bench_registry_open(c: &mut Criterion) {
    let scratch = support::Scratch::new("registry-open");
    let fixtures: Vec<Fixture> = ENTRY_COUNTS
        .iter()
        .map(|entries| Fixture::new(&scratch.dir, *entries))
        .collect();

    print_attribution(&scratch.dir, &fixtures);

    let mut group = c.benchmark_group("registry_open");
    for fixture in &fixtures {
        let id = fixture.entries;
        group.bench_with_input(
            BenchmarkId::new("owner_only_open", id),
            &fixture.dir,
            |b, dir| {
                b.iter_custom(|iters| (0..iters).map(|_| time_owner_only_open(dir)).sum());
            },
        );
        group.bench_with_input(
            BenchmarkId::new("dir_create_only", id),
            &fixture.dir,
            |b, dir| {
                b.iter_custom(|iters| (0..iters).map(|_| time_dir_create_only(dir)).sum());
            },
        );
        group.bench_with_input(
            BenchmarkId::new("read_only_open", id),
            &fixture.dir,
            |b, dir| {
                b.iter_custom(|iters| (0..iters).map(|_| time_read_only_open(dir)).sum());
            },
        );
    }
    group.bench_function("first_open", |b| {
        b.iter_custom(|iters| (0..iters).map(|i| time_first_open(&scratch.dir, i)).sum());
    });
    group.finish();
}

criterion_group! {
    name = benches;
    // Each iteration is a real filesystem/security-metadata operation, and the
    // populated-registry cases are milliseconds apiece — the same short-series
    // shape `startup_latency_bench.rs` uses, rather than criterion's
    // microbenchmark defaults.
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    targets = bench_registry_open
}
criterion_main!(benches);
