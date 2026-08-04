//! Concurrency stress tier — the invariants that only break under *simultaneous*
//! load on the two resources this binary shares across every run: the per-user run
//! registry (`src/registry/mod.rs`) and the per-run control plane (`src/control/mod.rs`).
//!
//! The other tiers each prove a *functional* path (`AGENTS.md`, "Testing tiers"):
//! the unit tier proves a helper in isolation, the through-the-binary tier proves
//! one subcommand's contract, and the `e2e` tier proves containment against real
//! process trees — including a few deliberately concurrent scenarios, but always a
//! fixed handful of processes in a scripted order. This tier is the systematic
//! counterpart: it launches **dozens of simultaneous `run` invocations** and drives
//! parallel `list`/`prune`/`wait`/`inspect`/`cancel`/`kill` clients against the
//! **same** registry directory and control plane, then asserts the four properties
//! that a race would break:
//!
//! 1. [`prune_never_reaps_a_live_run_during_a_concurrent_start_storm`] — `prune`
//!    never reaps a live entry, including one racing a runner still inside its
//!    reservation window (see K-056: that race is closed by a minimum-age floor
//!    plus a post-lock file-identity re-verification — this asserts the invariant
//!    holds under load, it does not re-open the defence).
//! 2. [`registry_scans_never_lose_or_duplicate_records_under_churn`] — a registry
//!    scan (`list`, i.e. `Registry::entries`) neither loses nor duplicates a record
//!    while other runs concurrently write and delete their own.
//! 3. [`control_clients_refuse_boundedly_when_the_runner_is_gone`] — a control
//!    client aimed at an unreachable or dying runner refuses with the reserved
//!    `CONTROL` (103) code inside a bounded deadline, rather than hanging on a dead
//!    endpoint.
//! 4. [`wait_never_misses_a_completion_under_registry_load`] — `wait` never misses
//!    the completion it is watching for (and never announces one early), however
//!    busy the registry is around it.
//!
//! **Every scenario is a differential, not an absence check.** A "X never happens"
//! test can pass while proving nothing (K-059), so each scenario below carries a
//! built-in positive control: planted entries a correct `prune` *must* reap, a
//! churn stream the scanner *must* observe appearing and disappearing, live runs
//! the same control clients *must* reach with exit `0`, and a `wait` that *must*
//! report its own `WAIT_TIMEOUT` (112) against a run that is still going. If the
//! machinery under test silently stopped doing anything at all, those controls fail
//! and the scenario reports it — the "never" assertions are never left alone to
//! pass vacuously. Each test's docstring names the regression it detects.
//!
//! Gated behind the `stress` Cargo feature so the default `cargo test` never runs
//! it; the dedicated, non-gating `stress.yml` CI workflow does (weekly schedule /
//! manual dispatch). Run it locally the same way:
//!
//! ```sh
//! cargo test --features stress --test stress -- --nocapture
//! ```
//!
//! **Registry isolation.** Every scenario points its whole fleet — runners and
//! clients alike — at one *scratch* registry directory via
//! `PROCESSKIT_CLI_REGISTRY_DIR`, so the runs really do contend for a single shared
//! registry (the property under test) without touching the developer's or CI
//! runner's own per-user registry.
//!
//! **Teardown.** Nothing here is ever killed by PID: a run is torn down through the
//! control plane (`kill --run-id`, whole-tree on every platform) with the owned
//! `Child` handle as an identity-safe backstop, and every child additionally
//! self-bounds — the runner carries its own `--timeout` and the child program its
//! own bounded sleep — so even an aborted scenario leaves nothing running for more
//! than a couple of minutes.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime};

use common::{bin, headless_run_command, scratch, shell_inline};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// The environment variable every runner and client is pointed at, so one scenario's
/// whole fleet contends for a single scratch registry (see `src/registry/mod.rs`).
const REGISTRY_ENV: &str = "PROCESSKIT_CLI_REGISTRY_DIR";

/// Wall-clock ceiling on one client invocation (`list`/`prune`/`inspect`/`cancel`/
/// `kill`). Comfortably above every deadline the client itself enforces — the
/// control plane bounds its connect and its conversation at 5s each
/// (`src/control/mod.rs`) — so a client that hits *this* bound is hung, not merely slow,
/// and the harness reports it as such instead of blocking the tier forever.
const CLIENT_BOUND: Duration = Duration::from_secs(30);

/// How long a scenario waits for its runs to publish their registry records. Very
/// generous: dozens of simultaneous process launches on a shared CI runner are slow,
/// and a scenario that ends because the machine was loaded would be a false failure.
const REGISTRATION_BOUND: Duration = Duration::from_secs(90);

/// The child sleep (seconds) of a run that must stay alive for a whole scenario.
/// Self-bounding: even a leaked child ends by itself well inside a CI job.
const LONG_CHILD_SECS: u32 = 90;

/// The `--timeout` every stress runner carries — a self-bound on the *runner*, so an
/// aborted scenario cannot leave one behind indefinitely either. Above
/// [`LONG_CHILD_SECS`], so it never fires in a passing scenario.
const RUNNER_TIMEOUT_SECS: u32 = 120;

/// How many simultaneous runs the headline scenarios launch. "Dozens", tunable for a
/// smaller or larger machine via `PROCESSKIT_STRESS_RUNS`.
fn concurrent_runs() -> usize {
    env_count("PROCESSKIT_STRESS_RUNS", 24, 8)
}

/// A `usize` knob from the environment, falling back to `default` and never below
/// `min` (a scenario needs a floor of real concurrency to be worth running at all).
fn env_count(key: &str, default: usize, min: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(default)
        .max(min)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Serializes the scenarios in this tier against each other.
///
/// libtest runs `#[test]` functions on parallel threads, and each scenario here is
/// already a deliberate process storm — running four of them at once would measure
/// the host's scheduler rather than this binary's invariants, and would make the
/// timing bounds above meaningless. Holding this for the whole scenario is simpler
/// and more robust than requiring the caller to remember `--test-threads=1`: the
/// tier behaves the same however it is invoked.
///
/// Poisoning is deliberately ignored: a scenario that panicked has already reported
/// its own failure, and the remaining scenarios are still worth running.
fn arena() -> MutexGuard<'static, ()> {
    static ARENA: OnceLock<Mutex<()>> = OnceLock::new();
    ARENA
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A process-wide sequence number for unique scratch file names — the same
/// collision-proof discipline `common::scratch` applies to directories (K-026): a
/// name built from the PID alone collides between parallel `cargo test` processes,
/// and one built from a thread id alone collides between this tier's own workers.
fn next_seq() -> u32 {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The three directories a scenario's workers need. Cloned into every worker thread
/// (rather than borrowed) so a thread outlives nothing it points at.
#[derive(Clone)]
struct Paths {
    /// The shared scratch registry every runner and client in the scenario uses.
    registry: PathBuf,
    /// Where clients' redirected stdout/stderr files are created (and removed again).
    clients: PathBuf,
    /// Where each runner's `--jsonl` event file lives.
    runs: PathBuf,
}

/// One scenario: its scratch tree, its shared registry, the runs it launched, and
/// the tier-wide arena lock it holds for its whole lifetime.
struct Scenario {
    dir: PathBuf,
    paths: Paths,
    runs: Vec<RunHandle>,
    _arena: MutexGuard<'static, ()>,
}

impl Scenario {
    fn new(tag: &str) -> Self {
        // Acquired first: everything below is process-heavy and must not overlap
        // another scenario's storm.
        let guard = arena();
        let dir = scratch(tag);
        let paths = Paths {
            registry: dir.join("registry"),
            clients: dir.join("clients"),
            runs: dir.join("runs"),
        };
        for path in [&paths.registry, &paths.clients, &paths.runs] {
            fs::create_dir_all(path).expect("create a scenario scratch directory");
        }
        Self {
            dir,
            paths,
            runs: Vec::new(),
            _arena: guard,
        }
    }

    /// Launch one run into the scenario's shared registry and keep its handle.
    fn launch(&mut self, run_id: &str, child_secs: u32) {
        let run = spawn_run(&self.paths, run_id, child_secs);
        self.runs.push(run);
    }

    /// Run ids whose runner process has already exited, with the code it exited on —
    /// the scenarios assert this is empty while their storm is still running.
    fn exited_runs(&mut self) -> Vec<(String, Option<i32>)> {
        self.runs
            .iter_mut()
            .filter_map(|run| run.poll().map(|status| (run.run_id.clone(), status.code())))
            .collect()
    }

    /// Tear every still-live run down through the control plane, in parallel: a
    /// `kill` verb reaps the whole tree on every platform (`src/run/launch.rs`'s immediate
    /// hard-kill tier), unlike killing the runner process itself, which on
    /// macOS/BSD leaves the child behind (K-005). The owned handles below are the
    /// identity-safe backstop for anything that did not take the hint.
    fn shutdown(&mut self) {
        let live: Vec<String> = self
            .runs
            .iter_mut()
            .filter_map(|run| {
                if run.poll().is_none() {
                    Some(run.run_id.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut killers = Vec::with_capacity(live.len());
        for run_id in live {
            let paths = self.paths.clone();
            killers.push(thread::spawn(move || {
                let _ = client(&paths, &["kill", "--run-id", &run_id], CLIENT_BOUND);
            }));
        }
        for killer in killers {
            let _ = killer.join();
        }
        // One budget for the whole fleet, not per run: the `kill` verbs above have
        // already ended these, so each wait normally returns at once — but a
        // per-run bound would multiply by the fleet size in the case where they did
        // not, turning a scenario's teardown into minutes of waiting.
        let deadline = Instant::now() + Duration::from_secs(30);
        for run in &mut self.runs {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if run.wait_bounded(remaining).is_none() {
                run.kill_now();
            }
        }
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        self.shutdown();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// One launched `run` process, addressed by its `run_id` and owned by its `Child`
/// handle — never by PID, so teardown can never hit a recycled one.
struct RunHandle {
    run_id: String,
    child: Child,
    status: Option<ExitStatus>,
}

impl RunHandle {
    /// The runner's exit status if it has already exited, without consuming it and
    /// without blocking. Memoized: `try_wait` reaps the child once, so the answer
    /// has to be remembered rather than asked for twice.
    fn poll(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            self.status = self.child.try_wait().expect("poll a stress runner");
        }
        self.status
    }

    /// Poll until the runner exits or `bound` elapses; `None` means still running.
    fn wait_bounded(&mut self, bound: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + bound;
        loop {
            if let Some(status) = self.poll() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_millis(25));
        }
    }

    /// Kill through the owned handle (identity-safe) and reap.
    fn kill_now(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            self.status = self.child.wait().ok();
        }
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// The `-- <program> <args…>` tail of a child that stays alive for about `secs`
/// seconds, on either platform. `ping -n N` waits a second between echo requests, so
/// `N + 1` requests is roughly `N` seconds — the same idiom `tests/registry.rs` uses.
fn sleep_child(secs: u32) -> Vec<String> {
    if cfg!(windows) {
        shell_inline(&format!("ping -n {} 127.0.0.1 >nul", secs + 1))
    } else {
        shell_inline(&format!("sleep {secs}"))
    }
}

/// Launch `run --run-id <id> --jsonl <…> --no-echo --timeout <…> -- <sleep child>`
/// against the scenario's shared registry, with `null` stdio (nothing here reads the
/// runner's own streams, and an unread pipe could stall it).
fn spawn_run(paths: &Paths, run_id: &str, child_secs: u32) -> RunHandle {
    let jsonl = paths.runs.join(format!("{run_id}.jsonl"));
    let child = headless_run_command()
        .arg("--run-id")
        .arg(run_id)
        .arg("--jsonl")
        .arg(&jsonl)
        // The child's own output is irrelevant here and would only add noise to the
        // storm; `--capture-dir`/`--idle-timeout` still observe it in other tiers.
        .arg("--no-echo")
        .arg("--timeout")
        .arg(format!("{RUNNER_TIMEOUT_SECS}s"))
        .arg("--")
        .args(sleep_child(child_secs))
        .env(REGISTRY_ENV, &paths.registry)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a stress runner");
    RunHandle {
        run_id: run_id.to_string(),
        child,
        status: None,
    }
}

/// Launch a short run and wait for it to finish, returning its exit code — the churn
/// worker's unit of work (a registry record written, then deleted again).
fn run_to_completion(paths: &Paths, run_id: &str, child_secs: u32, bound: Duration) -> Option<i32> {
    let mut run = spawn_run(paths, run_id, child_secs);
    run.wait_bounded(bound).and_then(|status| status.code())
}

/// What one bounded client invocation produced.
struct ClientOutcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed: Duration,
    /// The client was still running at its deadline and had to be killed — the shape
    /// of "hung", which the control-plane scenario asserts never happens.
    timed_out: bool,
}

/// Run one client subcommand against the scenario's shared registry under a wall-clock
/// bound, killing it if it overruns.
///
/// Its stdout/stderr are redirected to **files**, not pipes: this polls the child
/// rather than blocking on `output()` (so a hang is detectable and killable), and a
/// pipe nobody is draining could stall a client that prints more than a pipe buffer's
/// worth — `list --json` over a busy registry is exactly that shape. The two files
/// are read back and removed, so a storm of thousands of invocations leaves nothing
/// behind.
fn client(paths: &Paths, args: &[&str], bound: Duration) -> ClientOutcome {
    let tag = format!("client-{}", next_seq());
    let stdout_path = paths.clients.join(format!("{tag}.out"));
    let stderr_path = paths.clients.join(format!("{tag}.err"));
    let (Ok(stdout), Ok(stderr)) = (File::create(&stdout_path), File::create(&stderr_path)) else {
        // The one realistic cause is the scenario's scratch tree having been removed
        // already — i.e. the test has failed and `Scenario::drop` ran while a worker
        // thread was still in flight. Report it as an outcome instead of panicking
        // here, so the original failure is what the run reports; a scenario still
        // running would surface this as an unexpected (`None`) exit code anyway.
        return ClientOutcome {
            code: None,
            stdout: String::new(),
            stderr: format!("stress harness: could not create the capture files for {args:?}"),
            elapsed: Duration::ZERO,
            timed_out: false,
        };
    };

    let started = Instant::now();
    let mut child = Command::new(bin())
        .args(args)
        .env(REGISTRY_ENV, &paths.registry)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn a stress client");

    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll a stress client") {
            break Some(status);
        }
        if started.elapsed() >= bound {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        sleep(Duration::from_millis(20));
    };
    let elapsed = started.elapsed();

    let read = |path: &Path| fs::read_to_string(path).unwrap_or_default();
    let outcome = ClientOutcome {
        code: status.and_then(|status| status.code()),
        stdout: read(&stdout_path),
        stderr: read(&stderr_path),
        elapsed,
        timed_out,
    };
    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);
    outcome
}

/// One `list --json` observation of the shared registry: every `(run_id, health)`
/// pair it reported, in the order it printed them — kept as a list, not a map, so a
/// scan that listed the same record twice is still visible as such.
#[derive(Clone)]
struct Snapshot {
    entries: Vec<(String, String)>,
}

impl Snapshot {
    /// The health `list` reported for `run_id`, or `None` when it listed no such
    /// entry.
    fn health(&self, run_id: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(id, _)| id == run_id)
            .map(|(_, health)| health.as_str())
    }

    /// Every run id this scan listed more than once — a scan that duplicated a
    /// record. Each id in this tier is used by exactly one run, so any repeat is a
    /// defect, never a legitimate collision.
    fn duplicated_ids(&self) -> Vec<String> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (run_id, _) in &self.entries {
            *counts.entry(run_id.as_str()).or_default() += 1;
        }
        counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(run_id, _)| run_id.to_string())
            .collect()
    }

    /// The ids in this scan that start with `prefix`.
    fn ids_with_prefix(&self, prefix: &str) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter(|(run_id, _)| run_id.starts_with(prefix))
            .map(|(run_id, _)| run_id.clone())
            .collect()
    }
}

/// Take one `list --json` scan of the shared registry. An `Err` is itself an
/// observation the scenarios assert on — a scan that fails under concurrent
/// modification is a defect, not an expected outcome.
fn list_snapshot(paths: &Paths) -> Result<Snapshot, String> {
    let outcome = client(paths, &["list", "--json"], CLIENT_BOUND);
    if outcome.timed_out {
        return Err(format!("`list --json` hung past {CLIENT_BOUND:?}"));
    }
    if outcome.code != Some(0) {
        return Err(format!(
            "`list --json` exited {:?}; stderr: {}",
            outcome.code,
            outcome.stderr.trim()
        ));
    }
    let mut entries = Vec::new();
    for line in outcome
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|err| format!("`list --json` printed a non-JSON line ({err}): {line}"))?;
        let run_id = value["run_id"]
            .as_str()
            .ok_or_else(|| format!("a `list` entry carries no run_id: {line}"))?;
        let health = value["health"]
            .as_str()
            .ok_or_else(|| format!("a `list` entry carries no health: {line}"))?;
        entries.push((run_id.to_string(), health.to_string()));
    }
    Ok(Snapshot { entries })
}

/// The tally one `prune --json` pass reported, with the instant the pass began — the
/// scenarios need the start instant to tell a pass that could already have seen every
/// live run from one that began before they registered.
#[derive(Clone, Copy)]
struct PruneTally {
    started: Instant,
    pruned: usize,
    live: usize,
    unprobed: usize,
    orphaned_locks: usize,
}

/// Run one **destructive** `prune --json` pass over the shared registry.
fn prune_pass(paths: &Paths) -> Result<PruneTally, String> {
    let started = Instant::now();
    let outcome = client(paths, &["prune", "--json"], CLIENT_BOUND);
    if outcome.timed_out {
        return Err(format!("`prune --json` hung past {CLIENT_BOUND:?}"));
    }
    if outcome.code != Some(0) {
        return Err(format!(
            "`prune --json` exited {:?}; stderr: {}",
            outcome.code,
            outcome.stderr.trim()
        ));
    }
    let value: serde_json::Value = serde_json::from_str(outcome.stdout.trim())
        .map_err(|err| format!("`prune --json` printed no JSON object ({err})"))?;
    let count = |key: &str| -> usize {
        value[key]
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(usize::MAX)
    };
    Ok(PruneTally {
        started,
        pruned: count("pruned"),
        live: count("live"),
        unprobed: count("unprobed"),
        orphaned_locks: count("orphaned_locks"),
    })
}

/// Poll `cond` until it holds or `timeout` elapses; returns whether it held.
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(50));
    }
}

/// Block until every id in `run_ids` is listed **live** in one single scan, or the
/// bound elapses. One scan for all of them on purpose: "all live at once" is the
/// precondition the scenarios need, not "each was live at some point".
fn wait_for_live_records(paths: &Paths, run_ids: &[String], bound: Duration) -> bool {
    wait_until(
        || match list_snapshot(paths) {
            Ok(snapshot) => run_ids
                .iter()
                .all(|run_id| snapshot.health(run_id) == Some("live")),
            Err(_) => false,
        },
        bound,
    )
}

/// The `run_id` of every `.json` record currently in the registry directory, read
/// straight off disk — the scenarios' independent check on what a `prune` storm left
/// behind, taken without going through the very scan path under test.
fn record_run_ids(registry: &Path) -> BTreeSet<String> {
    let Ok(read_dir) = fs::read_dir(registry) else {
        return BTreeSet::new();
    };
    let mut ids = BTreeSet::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(run_id) = value["run_id"].as_str() {
            ids.insert(run_id.to_string());
        }
    }
    ids
}

/// Plant a **confirmed-stale** registry entry: a well-formed record plus its lock
/// file, with nothing holding the lock — exactly what an abruptly-killed runner
/// leaves behind, and precisely what a correct `prune` must reap.
///
/// This is the positive control (K-059) for the prune scenario: without it, "no live
/// entry was reaped" could pass on a `prune` that reaps nothing at all — including
/// one that never even read this directory.
fn plant_stale_entry(registry: &Path, stem: &str, run_id: &str) -> (PathBuf, PathBuf) {
    let json_path = registry.join(format!("{stem}.json"));
    let lock_path = registry.join(format!("{stem}.lock"));
    File::create(&lock_path).expect("create a planted lock file");
    let record = serde_json::json!({
        "registry_version": 1,
        "run_id": run_id,
        // A stale entry's endpoint is whatever its dead runner last published; none
        // of these fixtures is ever connected to, only scanned and reaped.
        "endpoint": serde_json::Value::Null,
        // The exact shape `events::format_rfc3339_utc` produces, which the record
        // scan validates as a full calendar date (`src/registry/mod.rs`).
        "started_at": "2026-01-02T03:04:05.678Z",
        "liveness": {
            "kind": "advisory_lock",
            "lock_file": format!("{stem}.lock"),
        },
    });
    fs::write(
        &json_path,
        serde_json::to_string(&record).expect("render a planted record"),
    )
    .expect("write a planted record");
    (json_path, lock_path)
}

/// Plant an **orphaned** lock file — a `.lock` with no `.json` sibling — aged past
/// the reaper's minimum-age floor so `prune`'s orphan pass treats it as a candidate
/// instead of as a reservation still in flight.
///
/// The floor is what keeps a *fresh* reservation lock safe from that same pass
/// (K-056), so this fixture is the other half of the prune scenario's control: it
/// proves the orphan pass ran at all, which is what makes "no in-flight reservation
/// was reaped" a meaningful statement rather than a pass by inaction.
fn plant_orphan_lock(registry: &Path, stem: &str) -> PathBuf {
    let lock_path = registry.join(format!("{stem}.lock"));
    File::create(&lock_path).expect("create a planted orphan lock");
    backdate(&lock_path, Duration::from_secs(600));
    lock_path
}

/// Set `path`'s mtime `age` into the past. Opened for writing on both platforms
/// because that is what the underlying timestamp update requires; the file is a plain
/// scratch file nobody holds.
fn backdate(path: &Path, age: Duration) {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open a fixture to backdate it");
    file.set_modified(SystemTime::now() - age)
        .expect("backdate a fixture's mtime");
}

/// What a worker thread accumulates for its scenario to assert on after the storm.
type Collected<T> = Arc<Mutex<Vec<T>>>;

/// Spawn a worker thread that keeps calling `work` until `stop` is set, collecting
/// whatever it returns. The scenarios' background pressure is all built from this.
fn spawn_worker<T, F>(
    stop: &Arc<AtomicBool>,
    results: &Collected<T>,
    mut work: F,
) -> thread::JoinHandle<()>
where
    T: Send + 'static,
    F: FnMut() -> Option<T> + Send + 'static,
{
    let stop = Arc::clone(stop);
    let results = Arc::clone(results);
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(result) = work() {
                results
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(result);
            }
        }
    })
}

/// Take the collected results out of a worker's shared buffer.
fn drain<T>(results: &Collected<T>) -> Vec<T> {
    std::mem::take(
        &mut *results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// The first few of a scenario's observations, plus a count of the rest — a storm can
/// produce hundreds of identical violations, and a failure message that dumps every
/// one of them buries its own first line.
fn first_few<T: std::fmt::Display>(items: &[T]) -> String {
    const SHOWN: usize = 8;
    let mut rendered: Vec<String> = items.iter().take(SHOWN).map(T::to_string).collect();
    if items.len() > SHOWN {
        rendered.push(format!("… and {} more", items.len() - SHOWN));
    }
    format!("\n  {}", rendered.join("\n  "))
}

/// Stop every worker and join it, propagating a worker panic as this test's failure.
fn stop_workers(stop: &Arc<AtomicBool>, workers: Vec<thread::JoinHandle<()>>) {
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.join().expect("a stress worker thread panicked");
    }
}

// ---------------------------------------------------------------------------
// Invariant 1 — `prune` never reaps a live run
// ---------------------------------------------------------------------------

/// **`prune` never reaps a live entry, however many runs are starting around it.**
///
/// Dozens of runs are launched into one shared registry while several `prune` passes
/// run flat out against that same directory — so passes are guaranteed to land in
/// the window between a starting runner creating its lock file and publishing its
/// record (the reservation window K-056 closed with a minimum-age floor plus a
/// post-lock file-identity re-verification; this asserts the invariant holds under
/// load, it does not re-derive the defence). Throughout, a watcher keeps scanning the
/// registry, and the scenario then asserts that:
///
/// - no run that was once listed live ever vanished from a later scan, and none was
///   ever downgraded to `stale` while its runner process was alive — the two shapes
///   a wrongly reaped record (or a wrongly deleted reservation lock) would take;
/// - every runner is still alive, and the registry's final contents are exactly the
///   live runs' records;
/// - a `prune` pass that ran once every run had registered reported them as `live`
///   (or, failing a probe, as `unprobed`) — never as reaped.
///
/// **What proves this is not vacuous (K-059).** Planted entries a correct prune
/// *must* reap share the directory: three confirmed-stale `.json`/`.lock` pairs and
/// one aged orphan `.lock`. The scenario asserts they were all reaped and that the
/// tallies account for them, so a `prune` that had silently become a no-op — the way
/// this test could otherwise pass while proving nothing — fails here instead.
///
/// **Detection verified.** `probe_for_prune`'s live-lock verdict was temporarily
/// changed to report a *reapable* entry (`Ok(PruneProbe::Live)` →
/// `Ok(PruneProbe::Reapable(None))`, i.e. exactly the defect this scenario exists to
/// catch); this test then failed — no run could hold a live record at all, and the
/// registry was empty where it should have held 24 — and passed again once the line
/// was restored.
#[test]
fn prune_never_reaps_a_live_run_during_a_concurrent_start_storm() {
    let mut scenario = Scenario::new("stress-prune");
    let paths = scenario.paths.clone();
    let runs = concurrent_runs();
    let prune_threads = env_count("PROCESSKIT_STRESS_PRUNERS", 3, 2);

    // --- Positive control: entries a correct prune must reap -----------------
    const PLANTED_STALE: usize = 3;
    let mut planted_files = Vec::new();
    for index in 0..PLANTED_STALE {
        planted_files.push(plant_stale_entry(
            &paths.registry,
            &format!("planted-stale-{index}"),
            &format!("stress-planted-stale-{index}"),
        ));
    }
    let planted_orphan = plant_orphan_lock(&paths.registry, "planted-orphan");

    // --- The storm: prune passes and registry scans, running throughout ------
    let stop = Arc::new(AtomicBool::new(false));
    let tallies: Collected<Result<PruneTally, String>> = Collected::default();
    let snapshots: Collected<Result<Snapshot, String>> = Collected::default();
    let mut workers = Vec::new();
    for _ in 0..prune_threads {
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &tallies, move || {
            Some(prune_pass(&paths))
        }));
    }
    {
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &snapshots, move || {
            Some(list_snapshot(&paths))
        }));
    }

    // --- The launches, into the middle of that storm -------------------------
    let live_ids: Vec<String> = (0..runs)
        .map(|index| format!("stress-live-{index}"))
        .collect();
    for run_id in &live_ids {
        scenario.launch(run_id, LONG_CHILD_SECS);
    }
    let registered = wait_for_live_records(&paths, &live_ids, REGISTRATION_BOUND);
    let all_registered_at = Instant::now();
    // Keep pruning after the last registration, so at least one whole pass observes
    // the full set of live entries and has to decide what to do about it. Every
    // assertion waits until after the workers are stopped: a scenario that fails here
    // would otherwise tear its own scratch tree down (`Scenario::drop`) under threads
    // still using it, burying the real failure under their fallout.
    if registered {
        sleep(Duration::from_secs(3));
    }
    stop_workers(&stop, workers);

    let tallies = drain(&tallies);
    let snapshots = drain(&snapshots);
    let surviving_records = record_run_ids(&paths.registry);
    assert!(
        registered,
        "all {runs} runs must publish a live registry record while `prune` hammers the \
         same directory; a run that never appears — or one whose record is reaped out \
         from under it — is the reserve/prune race (K-056) reopening. Registry holds: \
         {surviving_records:?}"
    );

    // --- Nothing live was touched -------------------------------------------
    let exited = scenario.exited_runs();
    assert!(
        exited.is_empty(),
        "every stress runner must still be alive when the storm ends — these exited \
         early (run_id, exit code): {exited:?}"
    );

    let mut seen_live: BTreeSet<&str> = BTreeSet::new();
    let mut violations: Vec<String> = Vec::new();
    for (index, snapshot) in snapshots.iter().enumerate() {
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(err) => {
                violations.push(format!("scan {index} failed: {err}"));
                continue;
            }
        };
        for run_id in &live_ids {
            match snapshot.health(run_id) {
                Some("live") => {
                    seen_live.insert(run_id.as_str());
                }
                // A live runner holds its lock for its whole run, so `stale` here
                // means the lock was released or its file deleted under a live run —
                // the exact fingerprint of a reaped reservation.
                Some(other) => violations.push(format!(
                    "scan {index} reported live run {run_id} as `{other}`"
                )),
                None if seen_live.contains(run_id.as_str()) => violations.push(format!(
                    "scan {index} lost run {run_id}, which an earlier scan had listed live"
                )),
                None => {}
            }
        }
    }
    assert!(
        violations.is_empty(),
        "no live run may lose or downgrade its registry entry during a prune storm \
         ({} scans taken):{}",
        snapshots.len(),
        first_few(&violations)
    );

    let expected: BTreeSet<String> = live_ids.iter().cloned().collect();
    assert_eq!(
        surviving_records, expected,
        "after the storm the registry must hold exactly the live runs' records"
    );

    // --- The positive control: the planted entries were in fact reaped -------
    for (json_path, lock_path) in &planted_files {
        assert!(
            !json_path.exists() && !lock_path.exists(),
            "prune must reap a confirmed-stale entry it shares the directory with — \
             {} / {} survived, so this scenario proved nothing about the live entries \
             either",
            json_path.display(),
            lock_path.display()
        );
    }
    assert!(
        !planted_orphan.exists(),
        "prune's orphan-lock pass must reap an aged, unheld `.lock` file ({}) — if it \
         never ran, the untouched in-flight reservation locks prove nothing",
        planted_orphan.display()
    );

    // --- The tallies agree with all of the above ----------------------------
    let failed: Vec<&String> = tallies
        .iter()
        .filter_map(|tally| tally.as_ref().err())
        .collect();
    assert!(
        failed.is_empty(),
        "every `prune` pass must succeed while runs start around it:{}",
        first_few(&failed)
    );
    let passes: Vec<PruneTally> = tallies
        .iter()
        .filter_map(|tally| tally.as_ref().ok().copied())
        .collect();
    assert!(
        passes.len() >= 4,
        "the storm must land several prune passes to be worth anything, got {}",
        passes.len()
    );
    let reaped: usize = passes.iter().map(|pass| pass.pruned).sum();
    let reaped_orphans: usize = passes.iter().map(|pass| pass.orphaned_locks).sum();
    assert!(
        reaped >= PLANTED_STALE,
        "the passes must account for all {PLANTED_STALE} planted stale entries, tallied {reaped}"
    );
    assert!(
        reaped_orphans >= 1,
        "the passes must account for the planted orphan lock, tallied {reaped_orphans}"
    );

    // A pass that began after the last registration saw every live record. It may
    // not have been able to *probe* one (an unprobeable entry is left alone too), so
    // the two verdicts that never delete anything are counted together — but at least
    // one pass must have positively classified them all as live, which is the whole
    // point of the invariant.
    let settled: Vec<&PruneTally> = passes
        .iter()
        .filter(|pass| pass.started >= all_registered_at)
        .collect();
    assert!(
        !settled.is_empty(),
        "at least one prune pass must start after every run registered"
    );
    for pass in &settled {
        assert!(
            pass.live + pass.unprobed >= runs,
            "a prune pass over {runs} live runs must leave all of them alone \
             (live={}, unprobed={}, pruned={})",
            pass.live,
            pass.unprobed,
            pass.pruned
        );
    }
    assert!(
        settled.iter().any(|pass| pass.live >= runs),
        "a prune pass must positively classify all {runs} live runs as live"
    );
}

// ---------------------------------------------------------------------------
// Invariant 2 — a registry scan neither loses nor duplicates a record
// ---------------------------------------------------------------------------

/// **A registry scan never loses or duplicates a record under concurrent writes and
/// deletions.**
///
/// A set of anchor runs stays live for the whole scenario while short-lived churn
/// runs continuously register and deregister around them (each publishing its record
/// and deleting it again on its clean exit), a `prune` worker adds deletion pressure,
/// and a scanner reads `list --json` — `Registry::entries`, the shared `scan()` read
/// step (K-033) — as fast as it can. Every scan must:
///
/// - list every anchor, every time, once it has been seen once. A scan that misses a
///   record it is not racing at all (an anchor is neither being written nor deleted)
///   would mean a concurrent modification elsewhere in the directory can blind the
///   scan to healthy entries — the failure shape K-024/T-189 fixed one level at a
///   time, here asserted end to end;
/// - never list the same run id twice. Each id in this scenario belongs to exactly
///   one run, so a repeat can only come from the scan itself.
///
/// **What proves this is not vacuous (K-059).** A scanner watching an *idle*
/// registry would satisfy both assertions trivially, so the scenario also asserts it
/// actually observed the churn: several distinct churn ids must have been listed, and
/// several must have been listed and then legitimately disappeared — i.e. records
/// really were being created and deleted while these scans were running. It further
/// asserts every churn run exited `0`, so contention on the registry never broke a
/// run's own lifecycle.
///
/// **Detection verified**, once per half of the claim, by temporarily breaking
/// `Registry::scan`: pushing every scanned record twice made this test fail with
/// "scan 0 listed duplicates: [stress-anchor-0, …]", and dropping one record from
/// roughly every other scan made it fail with "scan 13 lost anchor
/// stress-anchor-0" — the loss branch specifically, not merely the precondition.
/// Both passed again once `scan` was restored.
#[test]
fn registry_scans_never_lose_or_duplicate_records_under_churn() {
    let mut scenario = Scenario::new("stress-scan");
    let paths = scenario.paths.clone();
    let anchors = concurrent_runs() / 3;
    let churn_threads = env_count("PROCESSKIT_STRESS_CHURN", 6, 2);

    let anchor_ids: Vec<String> = (0..anchors)
        .map(|index| format!("stress-anchor-{index}"))
        .collect();
    for run_id in &anchor_ids {
        scenario.launch(run_id, LONG_CHILD_SECS);
    }
    assert!(
        wait_for_live_records(&paths, &anchor_ids, REGISTRATION_BOUND),
        "the {anchors} anchor runs must all register before the churn starts"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let snapshots: Collected<Result<Snapshot, String>> = Collected::default();
    let churned: Collected<(String, Option<i32>)> = Collected::default();
    let prunes: Collected<Result<PruneTally, String>> = Collected::default();
    let mut workers = Vec::new();

    for _ in 0..churn_threads {
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &churned, move || {
            // ~1s of life each: long enough for a concurrent scan to observe the
            // record, short enough to keep the create/delete rate high.
            let run_id = format!("stress-churn-{}", next_seq());
            let code = run_to_completion(&paths, &run_id, 1, Duration::from_secs(60));
            Some((run_id, code))
        }));
    }
    {
        // Deletion pressure from the other direction: a reaper walking the same
        // directory the scanner is reading.
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &prunes, move || {
            let tally = prune_pass(&paths);
            sleep(Duration::from_millis(50));
            Some(tally)
        }));
    }
    {
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &snapshots, move || {
            Some(list_snapshot(&paths))
        }));
    }

    let storm_secs = u64::try_from(env_count("PROCESSKIT_STRESS_SECONDS", 15, 5)).unwrap_or(15);
    sleep(Duration::from_secs(storm_secs));
    stop_workers(&stop, workers);

    let snapshots = drain(&snapshots);
    let churned = drain(&churned);
    let prunes = drain(&prunes);

    let exited = scenario.exited_runs();
    assert!(
        exited.is_empty(),
        "every anchor run must outlive the churn — these exited early: {exited:?}"
    );

    // --- No loss, no duplication --------------------------------------------
    let mut violations: Vec<String> = Vec::new();
    let mut churn_seen: BTreeSet<String> = BTreeSet::new();
    let mut churn_gone: BTreeSet<String> = BTreeSet::new();
    for (index, snapshot) in snapshots.iter().enumerate() {
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(err) => {
                violations.push(format!("scan {index} failed: {err}"));
                continue;
            }
        };
        for run_id in &anchor_ids {
            match snapshot.health(run_id) {
                Some("live") => {}
                Some(other) => violations.push(format!(
                    "scan {index} reported anchor {run_id} as `{other}`"
                )),
                None => violations.push(format!("scan {index} lost anchor {run_id}")),
            }
        }
        let duplicates = snapshot.duplicated_ids();
        if !duplicates.is_empty() {
            violations.push(format!("scan {index} listed duplicates: {duplicates:?}"));
        }
        let present = snapshot.ids_with_prefix("stress-churn-");
        for run_id in churn_seen.difference(&present) {
            churn_gone.insert(run_id.clone());
        }
        churn_seen.extend(present);
    }
    assert!(
        violations.is_empty(),
        "a registry scan must neither lose nor duplicate a record under churn \
         ({} scans taken):{}",
        snapshots.len(),
        first_few(&violations)
    );

    // --- The churn really was concurrent with those scans (K-059) -----------
    assert!(
        snapshots.len() >= 10,
        "the scanner must take enough scans to mean anything, took {}",
        snapshots.len()
    );
    assert!(
        churn_seen.len() >= 5,
        "the scans must have observed churn records appearing (saw {}) — otherwise \
         nothing was ever concurrent with them",
        churn_seen.len()
    );
    assert!(
        churn_gone.len() >= 3,
        "the scans must have observed churn records disappearing (saw {}) — deletion \
         concurrent with scanning is half the property under test",
        churn_gone.len()
    );
    let failed_churn: Vec<String> = churned
        .iter()
        .filter(|(_, code)| *code != Some(0))
        .map(|(run_id, code)| format!("{run_id} exited {code:?}"))
        .collect();
    assert!(
        failed_churn.is_empty(),
        "registry contention must not break a run's own lifecycle; these churn runs \
         did not exit 0:{}",
        first_few(&failed_churn)
    );
    let failed_prunes: Vec<&String> = prunes
        .iter()
        .filter_map(|pass| pass.as_ref().err())
        .collect();
    assert!(
        failed_prunes.is_empty(),
        "the concurrent reaper must keep succeeding too:{}",
        first_few(&failed_prunes)
    );
}

// ---------------------------------------------------------------------------
// Invariant 3 — control clients refuse boundedly, never hang
// ---------------------------------------------------------------------------

/// The three verbs this scenario hammers — all three must refuse the same bounded way
/// when the runner is gone.
///
/// They are three of the **four** verbs that reach a live runner over its control
/// transport (`src/control/mod.rs`); `attest` is deliberately not among them. This
/// scenario's invariant is "success, or exactly `CONTROL` (103)", and that holds only
/// for verbs whose answer against a *live* runner is a success: `attest` asked by this
/// harness — a process outside every run it launches — answers `not_a_member` on any
/// platform that can name its peers, exiting `NOT_A_MEMBER` (115): a decided verdict
/// rather than the unreachability this test is about. Its refusal against a *gone* runner is the very `resolve_in_registry` path
/// all four verbs share, which these three exercise here; its own verdicts are covered
/// by `tests/attest.rs`.
const CONTROL_VERBS: [&str; 3] = ["inspect", "cancel", "kill"];

/// A `run_id` no run in this tier ever registers: the pure "unreachable" case, with
/// no record, no endpoint, and nothing racing it.
const GHOST_RUN_ID: &str = "stress-ghost";

/// One control-client invocation and what it produced.
struct Verdict {
    verb: &'static str,
    run_id: String,
    outcome: ClientOutcome,
}

/// Run one control verb against `run_id` under [`CLIENT_BOUND`]. `inspect` is passed
/// `--json` explicitly — it is optional (T-214), but this harness parses the reply as
/// JSON, so it always asks for that form; the mutating verbs take no flags.
fn control_verdict(paths: &Paths, verb: &'static str, run_id: &str) -> Verdict {
    let args: Vec<&str> = if verb == "inspect" {
        vec![verb, "--run-id", run_id, "--json"]
    } else {
        vec![verb, "--run-id", run_id]
    };
    Verdict {
        verb,
        run_id: run_id.to_string(),
        outcome: client(paths, &args, CLIENT_BOUND),
    }
}

/// **A control client aimed at an unreachable or dying runner refuses with `CONTROL`
/// (103) inside a bounded deadline — it never hangs.**
///
/// Half the fleet is killed abruptly (its `Drop`/cleanup never runs, so the registry
/// keeps a record whose endpoint is dead) while `inspect`/`cancel`/`kill` clients
/// hammer those ids from several threads, plus a `run_id` no run ever used. Every
/// invocation is run under a wall-clock ceiling and killed if it overruns, so a hang
/// is a reported failure rather than a wedged test run. The scenario asserts:
///
/// - no client ever hit that ceiling — every one returned on its own;
/// - while the fleet was dying, every verdict was either success (the client caught
///   the runner still alive) or exactly `CONTROL` (103) — never another code, never
///   an unbounded wait;
/// - once the doomed runners are confirmed gone, *every* verb against *every* dead
///   id, and against the never-registered id, is exactly `CONTROL` (103).
///
/// **What proves this is not vacuous (K-059).** A client that failed at everything —
/// a broken registry path, an environment pointed elsewhere — would satisfy the
/// "always 103" assertions while
/// testing nothing. So the same clients are also aimed at the untouched half of the
/// fleet, before and (freshly, per K-012 — a baseline taken before the storm would be
/// stale by the time it is checked) after the storm: those must succeed with exit
/// `0`, and the runs behind them must still be alive.
///
/// **Detection verified.** A 600s sleep was temporarily injected into
/// `control::resolve_live_endpoint`'s unreachable-run path (scoped to the ghost id,
/// to keep the experiment quick) — a real hang, not merely a wrong exit code; this
/// test failed with "a control client must never hang on an unreachable runner (bound
/// 30s); these did: [inspect --run-id stress-ghost]" and passed again once the sleep
/// was removed.
#[test]
fn control_clients_refuse_boundedly_when_the_runner_is_gone() {
    let mut scenario = Scenario::new("stress-control");
    let paths = scenario.paths.clone();
    let per_half = (concurrent_runs() / 2).max(4);
    let hammer_threads = env_count("PROCESSKIT_STRESS_HAMMERS", 4, 2);

    let bystander_ids: Vec<String> = (0..per_half)
        .map(|index| format!("stress-bystander-{index}"))
        .collect();
    let doomed_ids: Vec<String> = (0..per_half)
        .map(|index| format!("stress-doomed-{index}"))
        .collect();
    for run_id in bystander_ids.iter().chain(doomed_ids.iter()) {
        scenario.launch(run_id, LONG_CHILD_SECS);
    }
    let all_ids: Vec<String> = bystander_ids
        .iter()
        .chain(doomed_ids.iter())
        .cloned()
        .collect();
    assert!(
        wait_for_live_records(&paths, &all_ids, REGISTRATION_BOUND),
        "all {} runs must register before the control clients start",
        all_ids.len()
    );

    // --- Baseline: the very same client reaches a live run ------------------
    for run_id in &doomed_ids {
        let outcome = client(
            &paths,
            &["inspect", "--run-id", run_id, "--json"],
            CLIENT_BOUND,
        );
        assert_eq!(
            outcome.code,
            Some(0),
            "`inspect` must reach the live run {run_id} before it is killed (stderr: {})",
            outcome.stderr.trim()
        );
    }

    // --- The storm: clients in flight while the fleet dies under them -------
    let stop = Arc::new(AtomicBool::new(false));
    let verdicts: Collected<Verdict> = Collected::default();
    let mut workers = Vec::new();
    for thread_index in 0..hammer_threads {
        let paths = paths.clone();
        let targets = doomed_ids.clone();
        let mut turn = thread_index;
        workers.push(spawn_worker(&stop, &verdicts, move || {
            let verb = CONTROL_VERBS[turn % CONTROL_VERBS.len()];
            let run_id = targets[turn % targets.len()].clone();
            turn += 1;
            Some(control_verdict(&paths, verb, &run_id))
        }));
    }
    {
        // The run id nobody ever registered: the pure "unreachable" case, hammered
        // alongside the dying ones.
        let paths = paths.clone();
        let mut turn = 0usize;
        workers.push(spawn_worker(&stop, &verdicts, move || {
            let verb = CONTROL_VERBS[turn % CONTROL_VERBS.len()];
            turn += 1;
            Some(control_verdict(&paths, verb, GHOST_RUN_ID))
        }));
    }

    // Let the hammering get going, then pull the floor out from under it: an abrupt
    // kill through the owned handle leaves the record behind with a dead endpoint.
    // A doomed run may also be ended by the hammer's own `cancel`/`kill` verb before
    // this line reaches it — deliberately not prevented: a run torn down cleanly out
    // from under a client in flight is the same "dying under a client" case by
    // another route, and both must end in a bounded 0-or-103, never a hang.
    sleep(Duration::from_millis(500));
    for run in &mut scenario.runs {
        if run.run_id.starts_with("stress-doomed-") {
            run.kill_now();
        }
    }
    sleep(Duration::from_secs(3));
    stop_workers(&stop, workers);
    let verdicts = drain(&verdicts);

    let hung: Vec<String> = verdicts
        .iter()
        .filter(|verdict| verdict.outcome.timed_out)
        .map(|verdict| format!("{} --run-id {}", verdict.verb, verdict.run_id))
        .collect();
    assert!(
        hung.is_empty(),
        "a control client must never hang on an unreachable runner (bound {CLIENT_BOUND:?}); \
         these did:{}",
        first_few(&hung)
    );
    let unexpected: Vec<String> = verdicts
        .iter()
        .filter(|verdict| !matches!(verdict.outcome.code, Some(0) | Some(103)))
        .map(|verdict| {
            format!(
                "{} --run-id {} exited {:?}: {}",
                verdict.verb,
                verdict.run_id,
                verdict.outcome.code,
                verdict.outcome.stderr.trim()
            )
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "while a runner dies, a control client either reaches it (0) or refuses with \
         CONTROL (103) — nothing else:{}",
        first_few(&unexpected)
    );
    let ghost_bad: Vec<String> = verdicts
        .iter()
        .filter(|verdict| verdict.run_id == GHOST_RUN_ID && verdict.outcome.code != Some(103))
        .map(|verdict| format!("{} exited {:?}", verdict.verb, verdict.outcome.code))
        .collect();
    assert!(
        ghost_bad.is_empty(),
        "a run id nobody registered is always a CONTROL (103) refusal:{}",
        first_few(&ghost_bad)
    );
    let slowest = verdicts
        .iter()
        .map(|verdict| verdict.outcome.elapsed)
        .max()
        .unwrap_or_default();
    assert!(
        verdicts.len() >= 12,
        "the hammer must land enough client invocations to mean anything, landed {}",
        verdicts.len()
    );
    println!(
        "control clients: {} invocations, slowest {slowest:?} (bound {CLIENT_BOUND:?})",
        verdicts.len()
    );

    // --- Settled: every verb against every dead id is exactly 103 -----------
    for run in &mut scenario.runs {
        if run.run_id.starts_with("stress-doomed-") {
            assert!(
                run.wait_bounded(Duration::from_secs(30)).is_some(),
                "the doomed runner {} must be gone before the settled check",
                run.run_id
            );
        }
    }
    let settled_targets: Vec<&str> = doomed_ids
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(GHOST_RUN_ID))
        .collect();
    for run_id in settled_targets {
        for verb in CONTROL_VERBS {
            let verdict = control_verdict(&paths, verb, run_id);
            assert!(
                !verdict.outcome.timed_out,
                "`{verb}` hung on the unreachable run {run_id}"
            );
            assert_eq!(
                verdict.outcome.code,
                Some(103),
                "`{verb} --run-id {run_id}` must refuse with CONTROL (103) once the \
                 runner is gone (stderr: {})",
                verdict.outcome.stderr.trim()
            );
        }
    }

    // --- Fresh baseline (K-012): the untouched half is still reachable ------
    let dead_bystanders: Vec<(String, Option<i32>)> = scenario
        .runs
        .iter_mut()
        .filter(|run| run.run_id.starts_with("stress-bystander-"))
        .filter_map(|run| run.poll().map(|status| (run.run_id.clone(), status.code())))
        .collect();
    assert!(
        dead_bystanders.is_empty(),
        "a control verb aimed at one run must never touch another: {dead_bystanders:?}"
    );
    for run_id in &bystander_ids {
        let outcome = client(
            &paths,
            &["inspect", "--run-id", run_id, "--json"],
            CLIENT_BOUND,
        );
        assert_eq!(
            outcome.code,
            Some(0),
            "the bystander {run_id} must still be reachable after the storm — if this \
             failed too, the 103s above proved nothing (stderr: {})",
            outcome.stderr.trim()
        );
    }
}

// ---------------------------------------------------------------------------
// Invariant 4 — `wait` never misses a completion
// ---------------------------------------------------------------------------

/// What one `wait` subject observed: when its `wait` client started and returned,
/// when the run it was watching actually ended, and how the client exited.
struct WaitSubject {
    run_id: String,
    /// Whether the subject's own run published a live record before its `wait` client
    /// started — carried back rather than asserted inside the worker thread, so the
    /// scenario can stop its background workers before reporting any failure.
    registered: bool,
    started: Instant,
    runner_exited_at: Option<Instant>,
    wait_returned_at: Instant,
    code: Option<i32>,
    timed_out: bool,
}

/// **`wait` never misses the completion it is watching for, and never announces one
/// early.**
///
/// Several subject runs finish while a `wait --run-id` client blocks on each, with a
/// busy registry underneath: a dozen long-lived runs, a churn stream registering and
/// deregistering, and `list`/`prune` workers walking the same directory `wait` polls.
/// For every subject the scenario asserts:
///
/// - the client exited `0` — it saw the completion, rather than blocking until its
///   own `--timeout` (which would be `WAIT_TIMEOUT`, 112) or failing another way;
/// - it returned promptly after the run ended, not minutes later — a missed wake-up
///   that a long `--timeout` would otherwise hide;
/// - it did **not** return appreciably before the run ended — the opposite failure,
///   announcing a completion that had not happened (which a `wait` misreading a busy
///   registry could do: a record it failed to scan looks exactly like a finished
///   run).
///
/// **What proves this is not vacuous (K-059).** A `wait` that returned `0`
/// immediately, always, would pass an "exited 0" assertion by itself. So the scenario
/// also runs `wait --timeout 3s` against a run that is *still going* and asserts it
/// blocks for that long and then reports its own `WAIT_TIMEOUT` (112) — leaving the
/// run alive and untouched, which is separately checked. The premature-return bound
/// above closes the other direction.
///
/// **Detection verified.** `src/wait.rs`'s poll loop was temporarily made to return
/// success on `RunStatus::Live` (a `wait` that never waits): the differential fired
/// first — "must report its own WAIT_TIMEOUT (112) … left: Some(0)" — and, with the
/// same break narrowed to the subject runs so the differential still passed, the
/// premature-return assertion fired on its own: "`wait` must not announce
/// stress-subject-0's completion before it happened: it returned 8.16s before the run
/// ended". Both passed again once the loop was restored.
#[test]
fn wait_never_misses_a_completion_under_registry_load() {
    let mut scenario = Scenario::new("stress-wait");
    let paths = scenario.paths.clone();
    let load_runs = (concurrent_runs() / 2).max(4);
    let subjects = env_count("PROCESSKIT_STRESS_WAITERS", 6, 3);

    // How long a subject's child lives. Long enough that a `wait` returning at once
    // is unmistakable against `PREMATURE_SLACK` below, short enough to keep the
    // scenario brisk.
    const SUBJECT_CHILD_SECS: u32 = 8;
    // How long after the run really ended `wait` may still take to notice. It polls
    // the registry every 250ms, so this is orders of magnitude of slack — it only
    // catches a `wait` that missed the completion outright.
    const NOTICE_BOUND: Duration = Duration::from_secs(20);
    // How far before the run's own exit `wait` may legitimately return: it watches
    // for the registry record disappearing, which happens during teardown, a moment
    // before the runner process itself exits.
    const PREMATURE_SLACK: Duration = Duration::from_secs(4);
    // How much of a subject's life must still be ahead of it when its `wait` client
    // starts for "the client blocked" to be a fair thing to assert about that
    // subject — see where this is used.
    const MEANINGFUL_REMAINDER: Duration = Duration::from_secs(4);

    // --- Background load ----------------------------------------------------
    let load_ids: Vec<String> = (0..load_runs)
        .map(|index| format!("stress-load-{index}"))
        .collect();
    for run_id in &load_ids {
        scenario.launch(run_id, LONG_CHILD_SECS);
    }
    assert!(
        wait_for_live_records(&paths, &load_ids, REGISTRATION_BOUND),
        "the background runs must register before the subjects start"
    );

    let stop = Arc::new(AtomicBool::new(false));
    // Background pressure produces no results to assert on — its whole job is to be
    // in the way — so this collects nothing (every worker returns `None`).
    let noise: Collected<()> = Collected::default();
    let mut workers = Vec::new();
    for _ in 0..2 {
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &noise, move || {
            let run_id = format!("stress-noise-{}", next_seq());
            run_to_completion(&paths, &run_id, 1, Duration::from_secs(60));
            None
        }));
    }
    {
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &noise, move || {
            let _ = list_snapshot(&paths);
            sleep(Duration::from_millis(50));
            None
        }));
    }
    {
        let paths = paths.clone();
        workers.push(spawn_worker(&stop, &noise, move || {
            let _ = prune_pass(&paths);
            sleep(Duration::from_millis(100));
            None
        }));
    }

    // --- Subjects: a bounded run with a `wait` client blocked on it ---------
    let mut subject_threads = Vec::with_capacity(subjects);
    for index in 0..subjects {
        let paths = paths.clone();
        subject_threads.push(thread::spawn(move || -> WaitSubject {
            let run_id = format!("stress-subject-{index}");
            let mut run = spawn_run(&paths, &run_id, SUBJECT_CHILD_SECS);
            let registered =
                wait_for_live_records(&paths, std::slice::from_ref(&run_id), REGISTRATION_BOUND);

            let started = Instant::now();
            let waiter = {
                let paths = paths.clone();
                let run_id = run_id.clone();
                thread::spawn(move || {
                    let outcome = client(
                        &paths,
                        &["wait", "--run-id", &run_id, "--timeout", "180s"],
                        Duration::from_secs(150),
                    );
                    (outcome, Instant::now())
                })
            };
            // Observe the run's own end independently of what `wait` reports.
            let runner_exited_at = run
                .wait_bounded(Duration::from_secs(120))
                .map(|_| Instant::now());
            let (outcome, wait_returned_at) = waiter.join().expect("the wait client thread");
            WaitSubject {
                run_id,
                registered,
                started,
                runner_exited_at,
                wait_returned_at,
                code: outcome.code,
                timed_out: outcome.timed_out,
            }
        }));
    }
    let results: Vec<WaitSubject> = subject_threads
        .into_iter()
        .map(|handle| handle.join().expect("a wait subject thread panicked"))
        .collect();

    // --- The differential: `wait` really does block on a live run -----------
    let blocking_id = "stress-wait-blocking".to_string();
    scenario.launch(&blocking_id, LONG_CHILD_SECS);
    let blocking_registered = wait_for_live_records(
        &paths,
        std::slice::from_ref(&blocking_id),
        REGISTRATION_BOUND,
    );
    let blocked = client(
        &paths,
        &["wait", "--run-id", &blocking_id, "--timeout", "3s"],
        CLIENT_BOUND,
    );
    // Everything below is an assertion, so the background workers stop first (see the
    // same note in the prune scenario): a failing scenario must not pull its own
    // scratch tree out from under threads that are still running.
    stop_workers(&stop, workers);

    assert!(
        blocking_registered,
        "the still-running control run must register"
    );
    let unregistered: Vec<&str> = results
        .iter()
        .filter(|subject| !subject.registered)
        .map(|subject| subject.run_id.as_str())
        .collect();
    assert!(
        unregistered.is_empty(),
        "every subject run must publish a live record before its `wait` client starts: \
         {}",
        first_few(&unregistered)
    );
    assert!(
        !blocked.timed_out,
        "`wait --timeout 3s` must give up on its own, not hang"
    );
    assert_eq!(
        blocked.code,
        Some(112),
        "`wait` on a still-running run must report its own WAIT_TIMEOUT (112) — if it \
         returned 0 here, the 0s below would prove nothing (stderr: {})",
        blocked.stderr.trim()
    );
    assert!(
        blocked.elapsed >= Duration::from_millis(2_500),
        "`wait --timeout 3s` must actually block for its deadline, returned after {:?}",
        blocked.elapsed
    );
    let still_live = list_snapshot(&paths).expect("scan the registry after the timed-out wait");
    assert_eq!(
        still_live.health(&blocking_id),
        Some("live"),
        "a timed-out `wait` must leave the run it watched running and untouched"
    );

    // --- Every subject's completion was seen, promptly and not early --------
    let mut blocking_subjects = 0usize;
    for subject in &results {
        assert!(
            !subject.timed_out,
            "the `wait` client for {} never returned",
            subject.run_id
        );
        assert_eq!(
            subject.code,
            Some(0),
            "`wait` must report the completion of {} with exit 0",
            subject.run_id
        );
        let exited_at = subject
            .runner_exited_at
            .unwrap_or_else(|| panic!("the subject run {} must finish", subject.run_id));
        assert!(
            subject.wait_returned_at <= exited_at + NOTICE_BOUND,
            "`wait` must notice {}'s completion promptly, took {:?} longer than the run \
             itself (registry load must not make it miss the wake-up)",
            subject.run_id,
            subject
                .wait_returned_at
                .saturating_duration_since(exited_at)
        );
        assert!(
            subject.wait_returned_at + PREMATURE_SLACK >= exited_at,
            "`wait` must not announce {}'s completion before it happened: it returned \
             {:?} before the run ended",
            subject.run_id,
            exited_at.saturating_duration_since(subject.wait_returned_at)
        );
        // "It actually blocked" is only a meaningful thing to ask when the run still
        // had time left to live once the client started — on a heavily loaded runner
        // registration can eat most of the subject's own lifetime, and a fixed
        // minimum would then fail for a `wait` that behaved perfectly. Asserted only
        // for the subjects where it *is* meaningful, and the count of those is
        // checked below so the check cannot quietly evaporate for all of them.
        let remaining_at_start = exited_at.saturating_duration_since(subject.started);
        if remaining_at_start >= MEANINGFUL_REMAINDER {
            blocking_subjects += 1;
            assert!(
                subject.wait_returned_at.duration_since(subject.started) >= Duration::from_secs(2),
                "`wait` on {} returned after only {:?}, though the run still had {:?} to \
                 live — it never blocked at all",
                subject.run_id,
                subject.wait_returned_at.duration_since(subject.started),
                remaining_at_start
            );
        }
    }
    assert!(
        blocking_subjects > 0,
        "no subject run outlived its `wait` client's start by {MEANINGFUL_REMAINDER:?}, so \
         nothing here proved `wait` blocks at all — the machine is too slow for this \
         scenario's shape, or the subjects are not starting when they should"
    );
}
