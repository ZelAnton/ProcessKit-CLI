//! The **runtime qualification** (`processkit-cli doctor`): does *this host*
//! actually run a contained process, and clean up after it?
//!
//! [`probe`](crate::probe) answers a different question, and deliberately only that
//! one: "is the binary I found compatible with the surface I need?". It reads
//! compile-time constants and the in-memory clap tree, spawns nothing, and touches
//! no registry, container, or transport — which is exactly what makes it safe to run
//! as a preflight, and exactly what makes a passing probe *not* evidence that a run
//! will work here. A binary can satisfy every `--require-*` expectation and still
//! fail its first real run on a registry directory it cannot create, a containment
//! mechanism the kernel will not give it, or a local IPC endpoint it cannot bind.
//!
//! `doctor` is the side-effecting counterpart that closes that gap. It performs a
//! **bounded scratch run of this binary's own harmless child** and reports the facts
//! it observed while doing so:
//!
//! - the per-user registry directory was created and is protected owner-only;
//! - which containment mechanism this host selected, and what abrupt-cleanup
//!   guarantee that mechanism carries;
//! - the local control transport bound, answered an `inspect`, accepted a `cancel`,
//!   and the run reached its terminal state;
//! - teardown left a **confirmed** empty container — the emptiness was read, not
//!   assumed (`read_error`), and no member remained;
//! - optionally, whether a whole-tree resource cap can be enforced here;
//! - how long each phase took, so a slow host is diagnosable rather than a generic
//!   failure.
//!
//! # The report is facts, never one boolean
//!
//! Every fact above is reported on its own, in both the human rendering and
//! `--json`, whether or not the invocation asked anything of it. The `--require-*`
//! flags gate only the **exit code** ([`exit::HOST_UNQUALIFIED`], 116): a caller that
//! pins one property still receives the whole picture, and two invocations that
//! differ only in a `--require-*` flag print the same report and differ only in what
//! they exit with. That is a tested property
//! (`tests/doctor.rs`), not a convention.
//!
//! # What it runs, and why that is this binary
//!
//! The scratch child is `processkit-cli doctor --scratch-child <duration>` — this
//! very executable, in a mode that sleeps briefly and does nothing else (see
//! [`crate::cli::DoctorArgs::scratch_child`]). Contained code has to come from
//! somewhere, and every alternative is worse: a shell builtin means qualifying the
//! host's shell rather than this runner, and a second helper binary would have to be
//! shipped, found, and trusted. Running ourselves is the pattern
//! `src/bin/e2e_helper.rs` already established for the end-to-end tier — a compiled
//! helper the tier fully controls — applied to the one executable that is guaranteed
//! to be present whenever `doctor` is.
//!
//! The scratch run itself is driven the same way: `doctor` spawns
//! `processkit-cli run` and then acts as an ordinary control-plane client against
//! it. Nothing here re-implements containment, teardown, or the registry — the
//! qualification is worth something precisely because it exercises the *real* path a
//! caller's own `run` will take, not a parallel one written to pass.
//!
//! # On success nothing is left; on failure the evidence is
//!
//! A qualified host ends with its registry record removed, its control endpoint
//! released, and the scratch directory deleted — the report says so, having checked
//! each. When a phase fails, that directory is **kept** instead, carrying the scratch
//! run's JSONL stream, its stdout/stderr, and this report; the report names its path
//! (`diagnostics_dir`) so the next step is a path, not a guess.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::DoctorArgs;
use crate::control::{self, ControlCommand, PROCESS_GROUP_MECHANISM};
use crate::duration_fmt::format_duration;
use crate::error_envelope::ErrorKind;
use crate::exit::{self, RunnerError};
use crate::registry;
use crate::text;

/// The doctor report's own format version. Independent of the JSONL event
/// [`schema_version`](crate::events::SCHEMA_VERSION), the probe report's
/// [`probe_version`](crate::probe::PROBE_VERSION), the control-plane
/// [`snapshot_version`](crate::control::SNAPSHOT_VERSION), and the rest: a doctor
/// report is a diagnostic artifact meant to be *kept* — written into the diagnostics
/// directory a failed qualification leaves behind, attached to a bug report, read
/// back by someone who did not run it — so it carries its own pin rather than
/// relying on the reader knowing which build produced it. Bump it only on a breaking
/// change to the report's shape (`fixtures/schema/cli/README.md`, "Versioning").
pub const DOCTOR_VERSION: u32 = 1;

/// How long the scratch run is given past the qualification's own budget before its
/// `--timeout` tears it down regardless. The scratch run must outlive `doctor`'s own
/// deadline (otherwise a slow host would end the run *while* `doctor` is still
/// waiting on it, and the report would blame the wrong phase), but only just: this is
/// the margin, not a second budget.
const SCRATCH_RUN_MARGIN: Duration = Duration::from_secs(5);

/// The whole-tree process cap the optional resource-controller check asks for. High
/// enough that the two-process scratch tree can never trip it — the check is about
/// whether the cap can be *installed*, never about enforcing it — and low enough to
/// be a plausible real request.
const RESOURCE_CHECK_MAX_PROCESSES: u32 = 64;

/// How long the resource-controller check's own scratch child lives. It only has to
/// exist long enough for the container to be created with the cap on it; nothing
/// inspects or cancels it.
const RESOURCE_CHECK_CHILD: Duration = Duration::from_millis(50);

/// The floor under any duration this module puts on a scratch run's command line.
/// A budget that has already run out would otherwise render as `0ms`, which
/// `--timeout`/`--scratch-child` reject at parse time (`parse_positive_duration`) —
/// turning an exhausted deadline into a usage error about `doctor`'s own argv rather
/// than the honest "the budget ran out" the phase is about to report anyway.
const MIN_SCRATCH_BUDGET: Duration = Duration::from_millis(100);

/// How long to sleep between polls while waiting for the scratch run to publish its
/// record, or to disappear again. Matches `wait`'s and `events --follow`'s own
/// cadence: fast enough that a healthy host adds no perceptible delay, slow enough
/// that a slow one is not hammered.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The window [`ScratchRun::cleanup_best_effort`] gives a runner whose control-plane
/// cancel already landed (or might have) to finish its own soft-stop -> grace ->
/// hard-kill teardown -- releasing its container, removing its registry record and
/// control socket -- before this resorts to `Child::kill`. Scratch runs are spawned
/// with no `--grace` of their own (`Session::spawn_run`), so that teardown is local
/// process/filesystem work, not a wait on a child that ignored a soft stop; long
/// enough to absorb that work under a loaded host, nowhere near the five-second
/// `--timeout` margin ([`SCRATCH_RUN_MARGIN`]) a runner left alive would otherwise
/// still owe.
const CLEANUP_GRACE_BUDGET: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// One machine-readable qualification report — the whole of what `doctor` observed.
///
/// `Serialize` to print it, `Deserialize` so a consumer (and the tests) parse it back
/// and check fields rather than scraping text, exactly as
/// [`ProbeReport`](crate::probe::ProbeReport) does.
///
/// A phase that never ran leaves its facts `null` rather than a fabricated default:
/// "not observed" and "observed to be empty" are different answers, and this shape
/// keeps them apart.
#[derive(Debug, Serialize, Deserialize)]
pub struct DoctorReport {
    /// This report's format version ([`DOCTOR_VERSION`]).
    pub doctor_version: u32,
    /// The binary's package name (`processkit-cli`), so a report read out of context
    /// still names what produced it.
    pub binary: String,
    /// The binary's semantic version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The host's OS as this build names it (`windows` / `linux` / `macos` / …,
    /// `std::env::consts::OS`) — the coarsest fact about *which* host was qualified,
    /// and the one a reader of a kept report needs first.
    pub os: String,
    /// Whether every phase succeeded **and** every `--require-*` expectation was met.
    /// The one-line verdict; `failures` and `mismatches` are the two independent
    /// reasons it can be `false`, and everything else here is the evidence.
    pub qualified: bool,
    /// The per-user run registry as observed, or `null` if that phase never ran.
    pub registry: Option<RegistryFacts>,
    /// The containment mechanism as observed, or `null` if no scratch run ever
    /// reported one.
    pub containment: Option<ContainmentFacts>,
    /// The local control transport round-trip as observed, or `null` if it never got
    /// far enough to report anything.
    pub control: Option<ControlFacts>,
    /// Teardown and artifact cleanup as observed, or `null` if the scratch run never
    /// reached teardown.
    pub cleanup: Option<CleanupFacts>,
    /// The optional resource-controller check, or `null` when
    /// `--check-resource-controller` was not asked for. **Null means "not checked",
    /// never "not available"** — an absent fact and a negative one are different
    /// answers.
    pub resource_controller: Option<ResourceControllerFacts>,
    /// Each phase this invocation ran, in the order it ran them, with what it cost.
    /// A phase that failed is the last one present.
    pub phases: Vec<PhaseReport>,
    /// Wall-clock milliseconds for the whole qualification.
    pub elapsed_ms: u64,
    /// One human-readable reason per **failed phase** — what this host could not do.
    /// Always present; empty when every phase succeeded.
    pub failures: Vec<String>,
    /// One human-readable reason per unmet `--require-*` expectation — what this host
    /// does, that the caller needed to be otherwise. Always present; empty when none
    /// was requested or all were met. Kept apart from `failures`: "this host is
    /// broken" and "this host is not the one you asked for" are different verdicts,
    /// even though both make `qualified` false.
    pub mismatches: Vec<String>,
    /// The directory holding this run's scratch artifacts (the scratch run's JSONL
    /// stream, its stdout/stderr, and a copy of this report), kept **only** when a
    /// phase failed. `null` on a qualified host, which leaves nothing behind at all.
    pub diagnostics_dir: Option<String>,
}

/// What was observed about the per-user run registry — the first thing any run
/// needs, and the first thing that can stop one on a fresh host.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegistryFacts {
    /// The registry directory this host resolves to (`PROCESSKIT_CLI_REGISTRY_DIR`
    /// if set, else the platform default — `docs/registry.md`). Named explicitly
    /// because "which directory" is the first question a permissions problem raises.
    pub dir: String,
    /// Whether the directory is protected to its owner alone, re-read from the
    /// filesystem after the create — never assumed from the create having returned
    /// `Ok`. Checked by the same predicate the registry's own tests use: a `0700`
    /// mode compare on unix, a binary-SID ACE compare against a protected DACL on
    /// Windows.
    pub owner_only: bool,
    /// How that protection is expressed on this platform (`posix_0700` /
    /// `windows_owner_only_dacl`), so `owner_only` names a mechanism rather than an
    /// opinion.
    pub protection: String,
}

/// What was observed about containment: which mechanism this host gave the scratch
/// run, and what that mechanism still guarantees if the runner is killed outright.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContainmentFacts {
    /// `job_object` / `cgroup_v2` / `process_group` / `process_reaper` / `unknown` —
    /// the same vocabulary the JSONL `run_started` event publishes
    /// (`docs/schema.md`), read from that very event.
    pub mechanism: String,
    /// `whole_tree` / `direct_child_only` / `none`: what still reaps the tree if the
    /// runner dies without running destructors. An OS-derived property of the
    /// mechanism, not a setting (`docs/platform-support.md`).
    pub abrupt_cleanup: String,
    /// The scratch child's PID, as the run reported it — evidence that a real
    /// process was really contained, not that a container was merely created.
    pub root_pid: Option<u32>,
}

/// What was observed about the local control transport, driven as an ordinary
/// client against the scratch run: bind, `inspect`, `cancel`, and the run's terminal
/// state.
#[derive(Debug, Serialize, Deserialize)]
pub struct ControlFacts {
    /// The endpoint the scratch run published and this qualification connected to (a
    /// unix socket path, a Windows named pipe name).
    pub endpoint: String,
    /// `unix_socket` / `windows_named_pipe` — which transport this host uses.
    pub transport: String,
    /// How many container members the `inspect` round-trip came back with. At least
    /// one on a healthy host (the scratch child itself), so this is the round-trip's
    /// content and not merely its success.
    pub inspected_members: usize,
    /// Whether the runner acknowledged the `cancel` verb — the mutating half of the
    /// round-trip, which a read-only `inspect` cannot establish.
    pub cancel_acknowledged: bool,
    /// The exit code the scratch runner ended with. `108`
    /// ([`exit::CONTROL_CANCELLED`]) on a healthy host: the cancel this
    /// qualification sent is what ended it.
    pub terminal_exit_code: Option<i32>,
    /// The terminal `runner_exit` event's `source` — `control_cancel` on a healthy
    /// host. Read from the run's own stream, so the run's account and this report
    /// cannot disagree.
    pub terminal_source: Option<String>,
}

/// What was observed about teardown and artifact cleanup — the half of containment
/// that a host can fail *after* everything else worked.
#[derive(Debug, Serialize, Deserialize)]
pub struct CleanupFacts {
    /// The container's size as teardown began (`cleanup_started.members_before`).
    pub members_before: Option<u64>,
    /// Members still alive after the hard kill (`cleanup_finished.remaining`).
    pub remaining: Option<u64>,
    /// Their PIDs, when there were any (`cleanup_finished.remaining_pids`).
    pub remaining_pids: Vec<u32>,
    /// Whether **either** member read failed during teardown
    /// (`cleanup_started.read_error` or `cleanup_finished.read_error`). This is what
    /// makes `remaining: 0` mean something: a `0` from a failed read is a gap, not a
    /// confirmed-empty container (`docs/schema.md`, "cleanup_finished").
    pub read_error: bool,
    /// Confirmed empty: the reads succeeded **and** nothing remained. The
    /// conjunction is the point — either half alone would be an unearned claim.
    pub confirmed_empty: bool,
    /// Whether a **non-empty** post-teardown snapshot would have been conclusive
    /// evidence that something survived.
    ///
    /// `true` on every mechanism whose teardown is atomic over the whole tree.
    /// `false` on the POSIX `process_group` fallback alone, where that snapshot is a
    /// `kill(pid, 0)` probe and a just-exited child nobody has reaped yet still
    /// answers it (`docs/schema.md`, "cleanup_finished") — so there, a
    /// `confirmed_empty: false` is the honest limit of what the platform can report
    /// rather than proof of a survivor, and does not fail the qualification.
    ///
    /// Published as a fact of its own rather than left in prose because it is the
    /// difference between two readings of the same `remaining` count, and a consumer
    /// deciding what to do about a non-zero one needs it.
    pub teardown_snapshot_conclusive: bool,
    /// Whether the scratch run's registry record is gone.
    pub registry_record_removed: bool,
    /// Whether the control endpoint was released: nothing answers a connection to it
    /// any more, and — on unix, where the transport leaves a private socket
    /// directory on disk — that directory is gone too. On Windows a named pipe is a
    /// kernel object that cannot outlive its runner, so only the first half is a
    /// filesystem question there.
    pub endpoint_released: bool,
    /// Whether the scratch directory (JSONL stream, runner stdout/stderr) was
    /// deleted. `false` on a failed qualification, where it is deliberately kept as
    /// `diagnostics_dir`.
    pub scratch_removed: bool,
}

/// What the optional resource-controller check observed. Present only when
/// `--check-resource-controller` asked for it.
#[derive(Debug, Serialize, Deserialize)]
pub struct ResourceControllerFacts {
    /// The cap this check asked for, spelled as the `run` flag that asks for it —
    /// so the report names a reproducible invocation rather than an abstraction.
    pub requested: String,
    /// Whether the container could be created with that cap installed. `false` means
    /// this host's mechanism cannot enforce whole-tree resource limits, which is a
    /// property of the platform (`docs/resource-limits.md`), not a fault.
    pub available: bool,
    /// What the scratch run said when it could not, taken from its own `limit_hit`
    /// event where there was one. `null` when the cap installed cleanly.
    pub detail: Option<String>,
}

/// One phase of the qualification, and what it cost.
#[derive(Debug, Serialize, Deserialize)]
pub struct PhaseReport {
    /// The phase's stable name: `registry`, `launch`, `inspect`, `cancel`,
    /// `terminal_wait`, `cleanup`, `resource_controller`.
    pub phase: String,
    /// Whether it succeeded. A failed phase is the last one in `phases` — except
    /// `resource_controller`, which is isolated by design and never stops the rest.
    pub ok: bool,
    /// Wall-clock milliseconds this phase took. The reason a slow host is
    /// diagnosable: the totals say *where* the time went.
    pub elapsed_ms: u64,
    /// Why it failed, or `null` when it did not. Free text: a diagnostic, not a
    /// contract — a fact worth acting on gets a field of its own instead, rather than
    /// a note here that only a human could read (the mechanism-dependent teardown
    /// snapshot is [`CleanupFacts::teardown_snapshot_conclusive`]).
    pub detail: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run `doctor`.
///
/// With `--scratch-child`, this is the harmless child contract instead and nothing
/// below happens: sleep, exit `0` (clap has already refused that flag in combination
/// with any other, so this branch can never swallow a requested qualification —
/// `src/cli/doctor.rs`).
///
/// Otherwise: qualify the host, print the report (in **both** the qualified and the
/// unqualified case, so the caller always has a parseable result — the same contract
/// `probe` keeps), and return the verdict. A host that failed a phase or missed a
/// requirement is [`exit::HOST_UNQUALIFIED`] (116); a `doctor` that could not run the
/// check at all — no scratch directory, no path to this executable, no async runtime
/// — is [`exit::SETUP`] (111), because that is this command's own machinery failing
/// rather than a verdict about the host.
pub fn run(args: &DoctorArgs) -> Result<(), RunnerError> {
    if let Some(budget) = args.scratch_child {
        return scratch_child(budget);
    }

    let started = Instant::now();
    let deadline = started + args.timeout;
    let exe = std::env::current_exe().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not resolve this executable's own path: {err}"),
        )
    })?;
    let scratch = Scratch::create()?;
    let runtime = control::current_thread_runtime()?;

    let mut session = Session {
        exe,
        scratch,
        runtime,
        deadline,
        phases: Vec::new(),
        failures: Vec::new(),
        registry: None,
        containment: None,
        control: None,
        cleanup: None,
        resource_controller: None,
    };
    session.qualify(args);

    let mismatches = session.evaluate(args);
    let report = session.into_report(started, mismatches);
    let rendered = if args.json {
        serde_json::to_string(&report).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not render the doctor report: {err}"),
            )
        })?
    } else {
        render_human(&report)
    };
    println!("{rendered}");

    if report.qualified {
        Ok(())
    } else {
        Err(
            RunnerError::new(exit::HOST_UNQUALIFIED, unqualified_message(&report))
                .with_kind(ErrorKind::HostUnqualified),
        )
    }
}

/// The one-line summary a failed qualification reports on stderr. It names both
/// reasons separately, because "this host could not" and "this host does not" call
/// for different responses.
fn unqualified_message(report: &DoctorReport) -> String {
    let mut parts = Vec::new();
    if !report.failures.is_empty() {
        parts.push(format!(
            "this host did not complete the runtime qualification: {}",
            report.failures.join("; ")
        ));
    }
    if !report.mismatches.is_empty() {
        parts.push(format!(
            "this host does not meet the requested requirements: {}",
            report.mismatches.join("; ")
        ));
    }
    let mut message = parts.join("; ");
    if let Some(dir) = &report.diagnostics_dir {
        message.push_str(&format!("; diagnostics kept in `{dir}`"));
    }
    message
}

/// The `--scratch-child` contract: sleep for at most `budget`, then exit `0`.
///
/// Deliberately the whole implementation. It opens nothing, writes nothing, and
/// contacts nothing — the container around it is what is under test, so the process
/// inside it must contribute no behavior of its own to explain a result by.
fn scratch_child(budget: Duration) -> Result<(), RunnerError> {
    std::thread::sleep(budget);
    Ok(())
}

// ---------------------------------------------------------------------------
// The qualification itself
// ---------------------------------------------------------------------------

/// One qualification in progress: the fixed inputs, the deadline everything is
/// bounded by, and the facts gathered so far.
struct Session {
    exe: PathBuf,
    scratch: Scratch,
    runtime: tokio::runtime::Runtime,
    deadline: Instant,
    phases: Vec<PhaseReport>,
    failures: Vec<String>,
    registry: Option<RegistryFacts>,
    containment: Option<ContainmentFacts>,
    control: Option<ControlFacts>,
    cleanup: Option<CleanupFacts>,
    resource_controller: Option<ResourceControllerFacts>,
}

impl Session {
    /// Run every phase in order, stopping at the first mandatory failure.
    ///
    /// Deliberately infallible: a phase failing is a *fact about the host*, which is
    /// what this command exists to report, so it is recorded and the report is still
    /// produced. Only this command's own machinery failing (before this point) is an
    /// error.
    fn qualify(&mut self, args: &DoctorArgs) {
        if !self.phase_registry() {
            return;
        }
        let Some(mut run) = self.phase_launch() else {
            return;
        };
        // From here on the scratch run exists, so every exit path must end it — a
        // `doctor` that gave up must not leave a container behind, which is the very
        // failure mode it exists to catch.
        let qualified = self.drive(&mut run);
        run.finish(self, qualified);
        if args.check_resource_controller {
            self.phase_resource_controller();
        }
    }

    /// The phases that need a live scratch run, from the first round-trip to the
    /// teardown check. Separated from [`Session::qualify`] so the run is torn down on
    /// every path out of them.
    fn drive(&mut self, run: &mut ScratchRun) -> bool {
        self.phase_inspect(run) && self.phase_cancel(run) && self.phase_terminal_wait(run)
    }

    /// Phase `registry`: open the per-user registry the way a real run does (the
    /// *mutating* open, which creates the directory and asserts its permissions),
    /// then re-read those permissions off the filesystem to confirm them.
    fn phase_registry(&mut self) -> bool {
        let started = Instant::now();
        let outcome = registry::Registry::open()
            .map_err(|err| format!("could not open the per-user run registry: {err}"))
            .and_then(|registry| {
                let dir = registry.dir().to_path_buf();
                let owner_only = registry::dir_is_owner_only(&dir).map_err(|err| {
                    format!(
                        "the run registry `{}` was created, but its owner-only protection could \
                         not be read back: {err}",
                        dir.display()
                    )
                })?;
                Ok((dir, owner_only))
            });
        match outcome {
            Ok((dir, owner_only)) => {
                self.registry = Some(RegistryFacts {
                    dir: dir.to_string_lossy().into_owned(),
                    owner_only,
                    protection: registry::OWNER_ONLY_PROTECTION.to_string(),
                });
                if owner_only {
                    self.pass("registry", started);
                    true
                } else {
                    self.fail(
                        "registry",
                        started,
                        format!(
                            "the run registry `{}` is not protected to its owner alone; another \
                             account on this host can read or alter its records",
                            dir.display()
                        ),
                    );
                    false
                }
            }
            Err(detail) => {
                self.fail("registry", started, detail);
                false
            }
        }
    }

    /// Phase `launch`: start the scratch run and wait until it is discoverable — a
    /// live registry record publishing a control endpoint. That is the same
    /// resolution every control client performs, so reaching it proves the run is
    /// reachable *by the ordinary path*, not just alive.
    fn phase_launch(&mut self) -> Option<ScratchRun> {
        let started = Instant::now();
        let run_id = scratch_run_id();
        let jsonl = self.scratch.path.join("run.jsonl");
        let child = match self.spawn_run(
            &run_id,
            &jsonl,
            &[],
            self.remaining() + SCRATCH_RUN_MARGIN,
            self.remaining(),
        ) {
            Ok(child) => child,
            Err(detail) => {
                self.fail("launch", started, detail);
                return None;
            }
        };
        let mut run = ScratchRun {
            run_id,
            jsonl,
            child,
            endpoint: None,
        };

        loop {
            match self
                .runtime
                .block_on(control::resolve_live_endpoint("doctor", &run.run_id))
            {
                Ok(endpoint) => {
                    run.endpoint = Some(endpoint);
                    self.pass("launch", started);
                    return Some(run);
                }
                Err(err) => {
                    if let Some(code) = run.exited() {
                        self.fail(
                            "launch",
                            started,
                            format!(
                                "the scratch run exited with code {code} before it published a \
                                 control endpoint{}",
                                self.evidence_hint()
                            ),
                        );
                        return None;
                    }
                    if Instant::now() >= self.deadline {
                        self.fail(
                            "launch",
                            started,
                            format!(
                                "the scratch run did not become discoverable within the budget: \
                                 {err}{}",
                                self.evidence_hint()
                            ),
                        );
                        // A run that never became discoverable was never registered
                        // where a control-plane cancel could reach it, so a cancel
                        // here would only pay a round-trip for nothing (`doctor`
                        // never kills by PID otherwise — `AGENTS.md`, "Never clean up
                        // by process name" — but this is the one exception: the same
                        // best-effort `Child::kill` every cancel-is-pointless
                        // post-spawn failure path uses, addressed at the exact handle
                        // this call spawned, not by name or PID lookup). Best-effort:
                        // it must not leave this run alive until its own `--timeout`,
                        // independent of whatever `doctor` does next.
                        run.kill_and_reap();
                        return None;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    }

    /// Phase `inspect`: the read-only half of the control round-trip — connect to the
    /// endpoint the run published and read a snapshot back, through the very client
    /// code `processkit-cli inspect` uses.
    fn phase_inspect(&mut self, run: &ScratchRun) -> bool {
        let started = Instant::now();
        let endpoint = run.endpoint().to_string();
        match self
            .runtime
            .block_on(control::inspect_endpoint(&endpoint, &run.run_id))
        {
            Ok(snapshot) => {
                self.control = Some(ControlFacts {
                    endpoint,
                    transport: control::TRANSPORT.to_string(),
                    inspected_members: snapshot.members.len(),
                    cancel_acknowledged: false,
                    terminal_exit_code: None,
                    terminal_source: None,
                });
                self.pass("inspect", started);
                true
            }
            Err(err) => {
                self.fail(
                    "inspect",
                    started,
                    format!("the scratch run's control transport did not answer an inspect: {err}"),
                );
                false
            }
        }
    }

    /// Phase `cancel`: the mutating half — ask the live runner to end the run and
    /// require its acknowledgement, again through the client code `cancel` uses.
    fn phase_cancel(&mut self, run: &ScratchRun) -> bool {
        let started = Instant::now();
        match self
            .runtime
            .block_on(control::mutate_one(&run.run_id, ControlCommand::Cancel))
        {
            Ok(_ack) => {
                if let Some(control) = self.control.as_mut() {
                    control.cancel_acknowledged = true;
                }
                self.pass("cancel", started);
                true
            }
            Err(err) => {
                self.fail(
                    "cancel",
                    started,
                    format!("the scratch run did not accept a control-plane cancel: {err}"),
                );
                false
            }
        }
    }

    /// Phase `terminal_wait`: wait for the cancelled run to actually end — the runner
    /// process exits, and its registry record disappears. Both are required: a
    /// runner that exits without removing its record leaves exactly the stale entry
    /// `prune` exists to clean up, and a caller qualifying a host wants to know that
    /// before it happens in production.
    fn phase_terminal_wait(&mut self, run: &mut ScratchRun) -> bool {
        let started = Instant::now();
        let code = match run.wait_for_exit(self.deadline) {
            Ok(code) => code,
            Err(detail) => {
                self.fail("terminal_wait", started, detail);
                return false;
            }
        };
        if let Some(control) = self.control.as_mut() {
            control.terminal_exit_code = code;
        }
        loop {
            match self.record_present(&run.run_id) {
                Ok(false) => {
                    self.pass("terminal_wait", started);
                    return true;
                }
                Ok(true) => {}
                Err(detail) => {
                    self.fail("terminal_wait", started, detail);
                    return false;
                }
            }
            if Instant::now() >= self.deadline {
                self.fail(
                    "terminal_wait",
                    started,
                    format!(
                        "the scratch run exited, but its registry record was still there when the \
                         budget ran out{}",
                        self.evidence_hint()
                    ),
                );
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Phase `cleanup`: read the run's own account of its teardown and confirm it —
    /// the container was read (not merely assumed) empty, no member survived, and
    /// every artifact the run created is gone.
    ///
    /// Run after the scratch run has fully ended, from [`ScratchRun::finish`], so it
    /// reads a complete stream.
    fn phase_cleanup(&mut self, run: &ScratchRun) {
        let started = Instant::now();
        let events = match read_events(&run.jsonl) {
            Ok(events) => events,
            Err(detail) => {
                self.fail("cleanup", started, detail);
                return;
            }
        };
        self.absorb_containment(&events);
        if let Some(control) = self.control.as_mut() {
            control.terminal_source = event(&events, "runner_exit")
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }

        let cleanup_started = event(&events, "cleanup_started");
        let cleanup_finished = event(&events, "cleanup_finished");
        let members_before = cleanup_started
            .and_then(|value| value.get("members_before"))
            .and_then(Value::as_u64);
        let remaining = cleanup_finished
            .and_then(|value| value.get("remaining"))
            .and_then(Value::as_u64);
        let remaining_pids: Vec<u32> = cleanup_finished
            .and_then(|value| value.get("remaining_pids"))
            .and_then(Value::as_array)
            .map(|pids| {
                pids.iter()
                    .filter_map(Value::as_u64)
                    .filter_map(|pid| u32::try_from(pid).ok())
                    .collect()
            })
            .unwrap_or_default();
        let read_error = [cleanup_started, cleanup_finished].iter().any(|event| {
            event
                .and_then(|value| value.get("read_error"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
        // Both halves have to be there *and* agree: a missing teardown event is not a
        // clean teardown, it is an unreported one.
        let confirmed_empty = !read_error && remaining == Some(0) && cleanup_started.is_some();

        let endpoint_released = self.endpoint_released(run);
        let conclusive = self.teardown_snapshot_is_conclusive();
        self.cleanup = Some(CleanupFacts {
            members_before,
            remaining,
            remaining_pids,
            read_error,
            confirmed_empty,
            teardown_snapshot_conclusive: conclusive,
            registry_record_removed: true,
            endpoint_released,
            scratch_removed: false,
        });

        let mut problems = Vec::new();
        match (read_error, remaining) {
            (true, _) => problems.push(
                "the container's members could not be read during teardown, so it was never \
                 confirmed empty"
                    .to_string(),
            ),
            (false, None) => problems.push(
                "the scratch run reported no teardown at all, so nothing confirms its container \
                 was reaped"
                    .to_string(),
            ),
            (false, Some(0)) => {}
            // A survivor is a containment failure on every mechanism whose teardown is
            // atomic over the whole tree — but **not** on the POSIX process-group
            // fallback, where the post-kill member probe is `kill(pid, 0)` and a
            // just-exited child that nobody has reaped yet still answers to it
            // (`docs/schema.md`, "cleanup_finished"; `docs/platform-support.md`). On
            // that mechanism the count cannot tell a survivor from a zombie, so
            // failing the phase on it would be a verdict nothing established — the
            // same discipline `read_error` exists to enforce one field over. Either
            // way the count is published, alongside the
            // `teardown_snapshot_conclusive` flag that says which of the two readings
            // applies here; what it is *not* is silently dropped.
            (false, Some(remaining)) if conclusive => problems.push(format!(
                "teardown left {remaining} member(s) of the scratch container in the post-kill \
                 snapshot"
            )),
            (false, Some(_)) => {}
        }
        if !endpoint_released {
            problems.push(
                "the scratch run's control endpoint was still present after it ended".to_string(),
            );
        }
        if problems.is_empty() {
            self.pass("cleanup", started);
        } else {
            self.fail("cleanup", started, problems.join("; "));
        }
    }

    /// Whether a non-empty post-teardown member snapshot is conclusive evidence that
    /// something survived.
    ///
    /// True for every mechanism whose teardown is atomic over the whole tree, and
    /// deliberately keyed on the single documented exception rather than on a list of
    /// the mechanisms that qualify: a mechanism this build has never heard of counts
    /// as conclusive, which is the strict direction to be wrong in.
    fn teardown_snapshot_is_conclusive(&self) -> bool {
        self.containment
            .as_ref()
            .is_none_or(|facts| facts.mechanism != PROCESS_GROUP_MECHANISM)
    }

    /// The optional phase `resource_controller`: a second, separate scratch run that
    /// asks for a whole-tree process cap. Its verdict is isolated — it runs after
    /// every mandatory phase has already finished against its own run, so a host
    /// without a resource controller still gets a complete report about everything
    /// else.
    ///
    /// **`available` is only ever written when this check reached a verdict.** Every
    /// way the check could not be *performed* — no budget left for it, a scratch run
    /// that would not spawn, a wait that could not complete, or a run that ended for
    /// some reason other than the cap — fails the phase and leaves
    /// `resource_controller` `null` instead, because "the controller is not
    /// available" and "nobody looked" are different answers and only one of them was
    /// established. (`--require-resource-controller` then reports the requirement as
    /// unmet on the honest ground that the fact was never observed, rather than on a
    /// negative nobody proved — see [`Session::evaluate`].)
    ///
    /// That last case is why the verdict is [`classify_resource_outcome`]'s and not
    /// the exit code's: a non-zero scratch run is *not* on its own evidence that the
    /// cap could not be installed. `BACKEND` (102) is the code for every container
    /// creation failure, and a run can also end on an unwritable stream
    /// ([`exit::SETUP`]), a deadline ([`exit::TIMEOUT`]), or a signal. The dedicated
    /// machine-readable signal that *this* ending was the cap is the run's own
    /// `limit_hit` event (`src/run/launch.rs`, `docs/schema.md`), so that is what
    /// `available: false` is written on.
    fn phase_resource_controller(&mut self) {
        let started = Instant::now();
        let requested = format!("--max-processes {RESOURCE_CHECK_MAX_PROCESSES}");
        if self.remaining().is_zero() {
            self.fail(
                "resource_controller",
                started,
                "the qualification budget ran out before the resource-controller check could \
                 run, so nothing was established about it either way"
                    .to_string(),
            );
            return;
        }
        let run_id = scratch_run_id();
        let jsonl = self.scratch.path.join("resource.jsonl");
        let cap = RESOURCE_CHECK_MAX_PROCESSES.to_string();
        let spawned = self.spawn_run(
            &run_id,
            &jsonl,
            &["--max-processes", &cap],
            self.remaining() + SCRATCH_RUN_MARGIN,
            RESOURCE_CHECK_CHILD,
        );
        let child = match spawned {
            Ok(child) => child,
            Err(detail) => {
                self.fail("resource_controller", started, detail);
                return;
            }
        };
        let mut run = ScratchRun {
            run_id,
            jsonl,
            child,
            endpoint: None,
        };
        let code = match run.wait_for_exit(self.deadline) {
            Ok(code) => code,
            Err(detail) => {
                // The wait itself failed or ran out of budget, so the run's own
                // ending was never observed and it may still be live: kill and reap
                // it by the exact handle this call spawned, not left to its own
                // `--timeout`. No cancel here — the run either is not answering the
                // control plane or has already died, so a cancel round-trip cannot
                // help and could itself add up to ten seconds (`CONNECT_DEADLINE` +
                // `CONVERSATION_DEADLINE`) on top of a budget that has already run
                // out.
                run.kill_and_reap();
                self.fail("resource_controller", started, detail);
                return;
            }
        };
        // The scratch run's own account of its ending, which is what decides this
        // verdict — the exit code alone cannot, since `BACKEND` (102) is equally the
        // code for a container that could not be created for reasons having nothing
        // to do with a cap (`src/run/launch.rs`, `create_group`).
        let events = read_events(&run.jsonl).unwrap_or_default();
        let (available, detail) = match classify_resource_outcome(code, &events) {
            ResourceOutcome::Installed => (true, None),
            ResourceOutcome::Refused(detail) => (false, Some(detail)),
            ResourceOutcome::Undecided(detail) => {
                // The run already exited (`wait_for_exit` above succeeded), so a
                // cancel would reach nothing and this is a no-op reap in practice —
                // kept here anyway so every path out of this phase, decided or not,
                // goes through a reap rather than only the ones known to still be
                // live.
                run.kill_and_reap();
                self.fail("resource_controller", started, detail);
                return;
            }
        };
        self.resource_controller = Some(ResourceControllerFacts {
            requested,
            available,
            detail,
        });
        // The check itself succeeded whenever it produced an answer: "this host has
        // no resource controller" is an observation, not a phase failure. Only
        // `--require-resource-controller` turns it into a verdict.
        self.pass("resource_controller", started);
    }

    /// Spawn one scratch run of this binary against this binary's own harmless
    /// child, with its stdio captured into the scratch directory.
    fn spawn_run(
        &self,
        run_id: &str,
        jsonl: &Path,
        extra: &[&str],
        run_budget: Duration,
        child_budget: Duration,
    ) -> Result<Child, String> {
        let stdout = self.scratch.create_file(&format!("{run_id}.stdout.log"))?;
        let stderr = self.scratch.create_file(&format!("{run_id}.stderr.log"))?;
        let mut command = Command::new(&self.exe);
        command
            .arg("run")
            .arg("--run-id")
            .arg(run_id)
            .arg("--jsonl")
            .arg(jsonl)
            .arg("--timeout")
            .arg(format_duration(run_budget.max(MIN_SCRATCH_BUDGET)))
            .args(extra)
            .arg("--")
            .arg(&self.exe)
            .arg("doctor")
            .arg("--scratch-child")
            .arg(format_duration(child_budget.max(MIN_SCRATCH_BUDGET)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        command.spawn().map_err(|err| {
            format!(
                "could not start a scratch run of `{}`: {err}",
                self.exe.display()
            )
        })
    }

    /// Read the containment facts out of the scratch run's own `run_started` event.
    fn absorb_containment(&mut self, events: &[Value]) {
        let Some(started) = event(events, "run_started") else {
            return;
        };
        let Some(mechanism) = started.get("mechanism").and_then(Value::as_str) else {
            return;
        };
        let Some(abrupt_cleanup) = started.get("abrupt_cleanup").and_then(Value::as_str) else {
            return;
        };
        self.containment = Some(ContainmentFacts {
            mechanism: mechanism.to_string(),
            abrupt_cleanup: abrupt_cleanup.to_string(),
            root_pid: started
                .get("root_pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok()),
        });
    }

    /// Whether the endpoint the ended run published has been released: nothing
    /// answers a connection to it, and no filesystem residue of it is left.
    fn endpoint_released(&self, run: &ScratchRun) -> bool {
        let Some(endpoint) = run.endpoint.as_deref() else {
            return true;
        };
        let still_serving = self
            .runtime
            .block_on(control::connect_live(endpoint, "doctor", &run.run_id))
            .is_ok();
        !still_serving && !endpoint_residue(endpoint)
    }

    /// Whether the registry still holds a record for `run_id`.
    fn record_present(&self, run_id: &str) -> Result<bool, String> {
        let registry = registry::Registry::open_read_only()
            .map_err(|err| format!("could not open the run registry: {err}"))?;
        let entries = registry
            .entries()
            .map_err(|err| format!("could not read the run registry: {err}"))?;
        Ok(entries.iter().any(|entry| entry.record.run_id == run_id))
    }

    /// How much of the budget is left, never negative.
    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// The pointer to the retained evidence, for a failure message that would
    /// otherwise leave the reader nowhere to go.
    fn evidence_hint(&self) -> String {
        format!(
            "; the scratch run's output is in `{}`",
            self.scratch.path.display()
        )
    }

    fn pass(&mut self, phase: &str, started: Instant) {
        self.phases.push(PhaseReport {
            phase: phase.to_string(),
            ok: true,
            elapsed_ms: elapsed_ms(started),
            detail: None,
        });
    }

    fn fail(&mut self, phase: &str, started: Instant, detail: String) {
        self.phases.push(PhaseReport {
            phase: phase.to_string(),
            ok: false,
            elapsed_ms: elapsed_ms(started),
            detail: Some(detail.clone()),
        });
        self.failures.push(detail);
    }

    /// Check every `--require-*` expectation against the facts actually observed and
    /// return the unmet ones (empty ⇒ nothing was asked for, or everything asked for
    /// holds).
    ///
    /// Deliberately a pure function of the report's own facts, run *after* every
    /// phase: a requirement can never change what was observed or which phases ran,
    /// only what the process exits with (`tests/doctor.rs` holds that to a
    /// differential test, not to this comment).
    fn evaluate(&self, args: &DoctorArgs) -> Vec<String> {
        let mut mismatches = Vec::new();
        if let Some(want) = args.require_mechanism.as_deref() {
            match self.containment.as_ref() {
                Some(facts) if facts.mechanism == want => {}
                Some(facts) => mismatches.push(format!(
                    "requires containment mechanism `{want}`, but this host selected `{}`",
                    facts.mechanism
                )),
                None => mismatches.push(format!(
                    "requires containment mechanism `{want}`, but this qualification never \
                     observed one"
                )),
            }
        }
        if let Some(want) = args.require_abrupt_cleanup.as_deref() {
            match self.containment.as_ref() {
                Some(facts) if facts.abrupt_cleanup == want => {}
                Some(facts) => mismatches.push(format!(
                    "requires abrupt-cleanup level `{want}`, but this host reports `{}`",
                    facts.abrupt_cleanup
                )),
                None => mismatches.push(format!(
                    "requires abrupt-cleanup level `{want}`, but this qualification never \
                     observed one"
                )),
            }
        }
        if args.require_resource_controller {
            match self.resource_controller.as_ref() {
                Some(facts) if facts.available => {}
                Some(facts) => mismatches.push(format!(
                    "requires an enforceable whole-tree resource controller, but `{}` could not \
                     be applied on this host{}",
                    facts.requested,
                    facts
                        .detail
                        .as_deref()
                        .map(|detail| format!(": {detail}"))
                        .unwrap_or_default()
                )),
                // Not "nobody asked" — clap requires `--check-resource-controller`
                // alongside this flag — but "the check reached no verdict": it ran and
                // could not establish the fact either way, which its own phase has
                // already failed and explained. The requirement is reported unmet on
                // that honest ground rather than on a negative nobody proved, and a
                // future caller of this function cannot turn an unobserved fact into a
                // silent pass either.
                None => mismatches.push(
                    "requires an enforceable whole-tree resource controller, but this \
                     qualification never established whether this host has one"
                        .to_string(),
                ),
            }
        }
        mismatches
    }

    /// Assemble the report and settle the scratch directory's fate: deleted on a
    /// qualified host, kept as the named `diagnostics_dir` when a phase failed —
    /// with this very report written into it, so the evidence and the verdict travel
    /// together.
    fn into_report(mut self, started: Instant, mismatches: Vec<String>) -> DoctorReport {
        let keep = !self.failures.is_empty();
        let scratch_removed = if keep {
            false
        } else {
            let removed = self.scratch.remove();
            if !removed {
                self.failures.push(format!(
                    "the scratch directory `{}` could not be removed",
                    self.scratch.path.display()
                ));
            }
            removed
        };
        if let Some(cleanup) = self.cleanup.as_mut() {
            cleanup.scratch_removed = scratch_removed;
        }
        let diagnostics_dir =
            (!self.failures.is_empty()).then(|| self.scratch.path.to_string_lossy().into_owned());

        let report = DoctorReport {
            doctor_version: DOCTOR_VERSION,
            binary: env!("CARGO_PKG_NAME").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            qualified: self.failures.is_empty() && mismatches.is_empty(),
            registry: self.registry,
            containment: self.containment,
            control: self.control,
            cleanup: self.cleanup,
            resource_controller: self.resource_controller,
            phases: self.phases,
            elapsed_ms: elapsed_ms(started),
            failures: self.failures,
            mismatches,
            diagnostics_dir,
        };
        if report.diagnostics_dir.is_some() {
            self.scratch.keep_report(&report);
        }
        report
    }
}

// ---------------------------------------------------------------------------
// The scratch run and its directory
// ---------------------------------------------------------------------------

/// The scratch run under qualification: its id, its stream, the runner process, and
/// the endpoint it published.
struct ScratchRun {
    run_id: String,
    jsonl: PathBuf,
    child: Child,
    endpoint: Option<String>,
}

impl ScratchRun {
    /// The endpoint this run published. Only called after `launch` succeeded, which
    /// is the phase that sets it.
    fn endpoint(&self) -> &str {
        self.endpoint
            .as_deref()
            .expect("the launch phase resolves an endpoint before any round-trip")
    }

    /// The exit code of the runner process if it has already ended, without waiting.
    fn exited(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    /// Wait for the runner process to end, bounded by `deadline`.
    fn wait_for_exit(&mut self, deadline: Instant) -> Result<Option<i32>, String> {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status.code()),
                Ok(None) => {}
                Err(err) => {
                    return Err(format!(
                        "could not wait for the scratch runner to exit: {err}"
                    ));
                }
            }
            if Instant::now() >= deadline {
                // Deliberately silent on what happens to the run next: callers differ
                // (`Session::phase_resource_controller` kills it outright,
                // `ScratchRun::finish` gives an in-progress teardown a grace window
                // first), and neither leaves it to its own `--timeout` any more.
                return Err("the scratch run had not ended when the budget ran out".to_string());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// End the run's participation in the qualification: read its teardown account
    /// when it got that far, and make sure it is not left behind either way.
    fn finish(mut self, session: &mut Session, reached_terminal: bool) {
        if reached_terminal {
            session.phase_cleanup(&self);
        } else {
            // A failed round-trip leaves the run live, and it may already be
            // mid-teardown: `phase_cancel` can have landed and acknowledged before
            // `phase_terminal_wait` failed to observe the exit in time (budget ran
            // out while the runner's own soft-stop -> grace -> hard-kill was still
            // under way), or the round-trip can have failed before ever reaching
            // `phase_cancel`. Either way, route through the cancel-then-grace
            // cleanup rather than `kill_and_reap`, so a `cancel` already in flight
            // is not cut short by an immediate kill.
            self.cleanup_best_effort(session);
        }
    }

    /// Cancel/grace/kill cleanup for a run that may already be tearing down on its
    /// own: only [`ScratchRun::finish`] calls this, for a round-trip that never
    /// reached its terminal phase. Every other post-spawn failure path — where a
    /// cancel is known to help nothing, because the run never registered or has
    /// already exited or is not answering the control plane — calls
    /// [`ScratchRun::kill_and_reap`] directly instead.
    ///
    /// Three steps, all best-effort and all discarded — this is recovery from a
    /// control plane or a runner that has already misbehaved once, not a check with
    /// its own verdict:
    /// - ask the control plane to cancel, in case the run is still reachable that
    ///   way (the clean shutdown a successful qualification gets) and has not
    ///   already been asked;
    /// - give the runner [`CLEANUP_GRACE_BUDGET`] to finish tearing down on its own
    ///   — releasing its container, removing its registry record and control
    ///   socket — polling [`ScratchRun::exited`] rather than assuming; a teardown
    ///   already in progress from an earlier successful cancel must be allowed to
    ///   finish, not killed out from under itself;
    /// - only once that window has passed with the process still alive,
    ///   unconditionally kill it by the exact handle this call spawned (never by
    ///   name or PID lookup — `AGENTS.md`, "Never clean up by process name") and
    ///   wait to reap it.
    ///
    /// Never called for a run that reached its terminal phase normally — that path
    /// already exited on its own and goes through [`Session::phase_cleanup`]
    /// instead. `Child::kill`/`wait` on an already-exited child are themselves
    /// no-ops (or a harmless "already exited" error), so the final step costs
    /// nothing when the grace window already saw the process end.
    fn cleanup_best_effort(&mut self, session: &mut Session) {
        let _ = session
            .runtime
            .block_on(control::mutate_one(&self.run_id, ControlCommand::Cancel));
        let grace_deadline = Instant::now() + CLEANUP_GRACE_BUDGET;
        loop {
            if self.exited().is_some() {
                let _ = self.child.wait();
                return;
            }
            if Instant::now() >= grace_deadline {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Unconditional kill-and-reap for a post-spawn failure path where a
    /// control-plane cancel is known to help nothing: the run either never
    /// registered where a cancel could reach it (`phase_launch`'s deadline path),
    /// has already exited (`phase_resource_controller`'s undecided outcome), or is
    /// not answering the control plane anyway (`phase_resource_controller`'s
    /// `wait_for_exit` error, where a cancel round-trip could itself add up to ten
    /// seconds on top of a budget that already ran out). Kills by the exact handle
    /// this call spawned (never by name or PID lookup — `AGENTS.md`, "Never clean
    /// up by process name") and waits to reap it. `Child::kill`/`wait` on an
    /// already-exited child are themselves no-ops (or a harmless "already exited"
    /// error), so calling this after the process has already ended costs nothing.
    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The scratch directory: everything this qualification writes lives here, and
/// nothing outside it does.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Create a fresh, uniquely-named, owner-only scratch directory under the OS
    /// temp directory.
    ///
    /// **Fresh, not reused.** The create is non-recursive, so an existing path is an
    /// `AlreadyExists` failure rather than a directory this qualification adopts.
    /// The OS temp directory is world-writable on unix, and a name derived from a pid
    /// and a clock is guessable in principle; refusing to write into a directory
    /// someone else created is what keeps that from mattering — and it is the same
    /// stance the control transport takes for its own private socket directory
    /// (`docs/threat-model.md`).
    fn create() -> Result<Self, RunnerError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "processkit-cli-doctor-{}-{nanos:x}",
            std::process::id()
        ));
        create_private_dir(&path).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!(
                    "could not create the doctor scratch directory `{}`: {err}",
                    path.display()
                ),
            )
        })?;
        Ok(Self { path })
    }

    fn create_file(&self, name: &str) -> Result<fs::File, String> {
        let path = self.path.join(name);
        fs::File::create(&path)
            .map_err(|err| format!("could not create `{}`: {err}", path.display()))
    }

    /// Remove the directory and everything in it, reporting whether it is gone.
    fn remove(&self) -> bool {
        fs::remove_dir_all(&self.path).is_ok() || !self.path.exists()
    }

    /// Write the report into the diagnostics directory it just named, so the
    /// evidence and the verdict are one artifact. Best-effort: a failure here must
    /// not replace the verdict the caller is about to receive on stdout, which is the
    /// authoritative copy.
    fn keep_report(&self, report: &DoctorReport) {
        let Ok(json) = serde_json::to_string_pretty(report) else {
            return;
        };
        let Ok(mut file) = fs::File::create(self.path.join("doctor-report.json")) else {
            return;
        };
        let _ = writeln!(file, "{json}");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// What the optional resource-controller check established about this host.
///
/// Three outcomes, not two: the check can also fail to establish anything, and that
/// is deliberately *not* spelled as a negative verdict. Keeping it apart is the whole
/// point of the type — see [`Session::phase_resource_controller`].
#[derive(Debug, PartialEq, Eq)]
enum ResourceOutcome {
    /// The container was created with the requested cap installed.
    Installed,
    /// The cap could not be installed, and the scratch run said so itself.
    Refused(String),
    /// Nothing was established either way: the scratch run ended for some reason
    /// other than the cap. Fails the phase, and leaves the facts `null`.
    Undecided(String),
}

/// Decide what one resource-controller scratch run established, from its exit code
/// and its own event stream.
///
/// A pure function of the two pieces of evidence, so the rule can be exercised
/// directly against every ending a run has (see this module's tests) rather than only
/// against the one this host happens to produce.
///
/// The rule: exit `0` installed the cap; the run's own `limit_hit` event — under the
/// [`exit::BACKEND`] code that event accompanies — is what makes "this host cannot
/// enforce a whole-tree cap" an observation rather than an inference. Every other
/// ending is [`ResourceOutcome::Undecided`], because `BACKEND` is equally the code
/// for a generic container-creation failure, and a scratch run can also end on a
/// setup error, a deadline, or a signal — none of which says anything about a
/// resource controller. Requiring **both** halves of the signal is the strict
/// direction to be wrong in: a run that reported a limit under some other code is a
/// pairing this build does not recognize, and reporting "nobody established it" for
/// it is the honest answer.
fn classify_resource_outcome(code: Option<i32>, events: &[Value]) -> ResourceOutcome {
    if code == Some(0) {
        return ResourceOutcome::Installed;
    }
    let limit_hit = event(events, "limit_hit");
    match (code, limit_hit) {
        (Some(code), Some(limit_hit)) if code == i32::from(exit::BACKEND) => {
            ResourceOutcome::Refused(limit_refusal(limit_hit))
        }
        (Some(code), _) => ResourceOutcome::Undecided(format!(
            "the scratch run asking for `--max-processes {RESOURCE_CHECK_MAX_PROCESSES}` exited \
             with code {code} without reporting a `limit_hit`, so it ended for some reason other \
             than the cap and nothing was established about this host's resource controller"
        )),
        (None, _) => ResourceOutcome::Undecided(
            "the scratch run asking for a resource cap was ended by a signal rather than an exit, \
             so nothing was established about this host's resource controller"
                .to_string(),
        ),
    }
}

/// The refusal a `limit_hit` event describes, as the report's `detail` states it.
///
/// Both of the event's fields are read: `limit` names *which* cap could not be
/// applied (`memory` / `processes` / `cpu`) and `detail` carries the backend's own
/// words for why. Each is treated as absent-able, because this reads a stream as data
/// rather than trusting a shape.
fn limit_refusal(limit_hit: &Value) -> String {
    let limit = limit_hit.get("limit").and_then(Value::as_str);
    let reason = limit_hit.get("detail").and_then(Value::as_str);
    match (limit, reason) {
        (Some(limit), Some(reason)) => {
            format!("the scratch run's `limit_hit` names the `{limit}` limit: {reason}")
        }
        (Some(limit), None) => {
            format!("the scratch run's `limit_hit` names the `{limit}` limit")
        }
        (None, Some(reason)) => {
            format!("the scratch run reported a limit it could not apply: {reason}")
        }
        (None, None) => {
            "the scratch run reported a limit it could not apply, without naming which".to_string()
        }
    }
}

/// A unique run id for a scratch run, marked as `doctor`'s so an operator who finds
/// one in `list` — after a `doctor` that was itself killed — knows what it was.
fn scratch_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("doctor-{}-{nanos:x}", std::process::id())
}

/// Milliseconds since `started`, saturating — a duration this long never legitimately
/// overflows, and a clock that made it do so is not worth panicking over.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Every line of a JSONL stream that parses as a JSON object, in order.
fn read_events(path: &Path) -> Result<Vec<Value>, String> {
    let text = fs::read_to_string(path).map_err(|err| {
        format!(
            "could not read the scratch run's event stream `{}`: {err}",
            path.display()
        )
    })?;
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect())
}

/// The **last** event of type `name` in the stream, or `None` when it never appeared.
/// Last rather than first: every event this module reads is emitted at most once per
/// run, and where one is not (a `--snapshot-interval` re-sample), the latest is the
/// one that describes the run's end.
fn event<'a>(events: &'a [Value], name: &str) -> Option<&'a Value> {
    events
        .iter()
        .rev()
        .find(|value| value.get("event").and_then(Value::as_str) == Some(name))
}

/// Create one new directory, owner-only, failing if anything is already there.
///
/// Unix: `0700` at creation time, never a `chmod` after the fact, so there is no
/// window where the directory exists with wider permissions. Windows: the per-user
/// temp directory (`%TEMP%`, under the user's own profile) already carries that
/// restriction, so the ordinary create inherits it; what matters on both is the
/// non-recursive create, which refuses a path that already exists.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(windows)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().create(path)
}

/// Whether the transport left something on disk for the endpoint it published.
///
/// On unix the control transport creates a private directory holding the socket, so
/// a released endpoint means that directory is gone. On Windows the endpoint is a
/// named pipe — a kernel object with no filesystem presence, which cannot outlive the
/// process that created it — so there is no residue to look for and this is `false`
/// by construction rather than by a check that would always pass.
#[cfg(unix)]
fn endpoint_residue(endpoint: &str) -> bool {
    control::unix_control_endpoint_dir(endpoint).is_some_and(|dir| dir.exists())
}

#[cfg(windows)]
fn endpoint_residue(_endpoint: &str) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// The human-readable report: the same facts `--json` carries, laid out for a person
/// diagnosing a host.
///
/// Every interpolated value that did not originate in this build — a path, an OS
/// error, a mechanism name read off a stream — crosses the terminal barrier through
/// [`text::terminal_safe_bounded`], the same discipline every other human renderer in
/// this project follows (`src/text.rs`).
fn render_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    let verdict = if report.qualified {
        "qualified"
    } else {
        "NOT qualified"
    };
    out.push_str(&format!(
        "processkit-cli {} on {}: {verdict} ({}ms)\n",
        report.version, report.os, report.elapsed_ms
    ));

    if let Some(registry) = &report.registry {
        out.push_str(&format!(
            "  registry:   {} (owner-only: {}, {})\n",
            text::terminal_safe_bounded(&registry.dir),
            registry.owner_only,
            text::terminal_safe_bounded(&registry.protection)
        ));
    }
    if let Some(containment) = &report.containment {
        out.push_str(&format!(
            "  containment: {} (abrupt cleanup: {})\n",
            text::terminal_safe_bounded(&containment.mechanism),
            text::terminal_safe_bounded(&containment.abrupt_cleanup)
        ));
    }
    if let Some(control) = &report.control {
        out.push_str(&format!(
            "  control:    {} ({}); inspect saw {} member(s), cancel acknowledged: {}, run ended \
             {}\n",
            text::terminal_safe_bounded(&control.endpoint),
            text::terminal_safe_bounded(&control.transport),
            control.inspected_members,
            control.cancel_acknowledged,
            control
                .terminal_source
                .as_deref()
                .map(|source| format!("with source `{}`", text::terminal_safe_bounded(source)))
                .unwrap_or_else(|| "without reporting a terminal event".to_string()),
        ));
    }
    if let Some(cleanup) = &report.cleanup {
        out.push_str(&format!(
            "  cleanup:    confirmed empty: {} (read error: {}, remaining: {}{}); record removed: \
             {}, endpoint released: {}, scratch removed: {}\n",
            cleanup.confirmed_empty,
            cleanup.read_error,
            cleanup
                .remaining
                .map(|remaining| remaining.to_string())
                .unwrap_or_else(|| "unreported".to_string()),
            if cleanup.teardown_snapshot_conclusive {
                ""
            } else {
                ", post-kill snapshot inconclusive on this mechanism"
            },
            cleanup.registry_record_removed,
            cleanup.endpoint_released,
            cleanup.scratch_removed,
        ));
    }
    if let Some(resource) = &report.resource_controller {
        out.push_str(&format!(
            "  resources:  `{}` enforceable: {}{}\n",
            text::terminal_safe_bounded(&resource.requested),
            resource.available,
            resource
                .detail
                .as_deref()
                .map(|detail| format!(" ({})", text::terminal_safe_bounded(detail)))
                .unwrap_or_default(),
        ));
    }

    out.push_str("  phases:     ");
    let phases: Vec<String> = report
        .phases
        .iter()
        .map(|phase| {
            format!(
                "{} {}{}ms",
                text::terminal_safe_bounded(&phase.phase),
                if phase.ok { "" } else { "FAILED " },
                phase.elapsed_ms
            )
        })
        .collect();
    out.push_str(&phases.join(", "));
    out.push('\n');

    for failure in &report.failures {
        out.push_str(&format!(
            "  failure:    {}\n",
            text::terminal_safe_bounded(failure)
        ));
    }
    for mismatch in &report.mismatches {
        out.push_str(&format!(
            "  mismatch:   {}\n",
            text::terminal_safe_bounded(mismatch)
        ));
    }
    if let Some(dir) = &report.diagnostics_dir {
        out.push_str(&format!(
            "  diagnostics kept in {}\n",
            text::terminal_safe_bounded(dir)
        ));
    }
    // The trailing newline belongs to `println!`, not to the body.
    out.pop();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// A `limit_hit` line as `run` really emits it (`src/run/launch.rs`): the cap that
    /// could not be applied, and the backend's own words for why.
    fn limit_hit(limit: &str) -> Value {
        json!({
            "event": "limit_hit",
            "limit": limit,
            "detail": "the active mechanism cannot enforce a whole-tree process cap",
        })
    }

    /// The generic container-creation failure that carries the *same* exit code and
    /// no `limit_hit` — the ending this verdict has to tell apart from a refused cap.
    fn container_failed() -> Value {
        json!({
            "event": "container_failed",
            "phase": "create",
            "code": 102,
            "message": "the container could not be created",
        })
    }

    #[test]
    fn a_clean_scratch_run_is_the_only_way_the_cap_installs() {
        assert_eq!(
            classify_resource_outcome(Some(0), &[]),
            ResourceOutcome::Installed
        );
    }

    #[test]
    fn a_limit_hit_under_backend_is_what_establishes_an_unavailable_controller() {
        // The whole pairing: the dedicated machine-readable signal, under the code it
        // accompanies. Nothing else may write `available: false`.
        let outcome =
            classify_resource_outcome(Some(i32::from(exit::BACKEND)), &[limit_hit("processes")]);
        let ResourceOutcome::Refused(detail) = outcome else {
            panic!("a reported limit is a decided verdict: {outcome:?}");
        };
        assert!(
            detail.contains("processes"),
            "the detail names which cap could not be applied: {detail}"
        );
        assert!(
            detail.contains("whole-tree process cap"),
            "and carries the backend's own explanation: {detail}"
        );
    }

    #[test]
    fn a_container_failure_that_is_not_a_limit_establishes_nothing() {
        // `BACKEND` (102) is equally the code for a container that could not be
        // created for reasons having nothing to do with a cap. Reading it as a verdict
        // would publish `available: false` about a host nobody asked the question of.
        let outcome =
            classify_resource_outcome(Some(i32::from(exit::BACKEND)), &[container_failed()]);
        let ResourceOutcome::Undecided(detail) = outcome else {
            panic!("a generic container failure decides nothing: {outcome:?}");
        };
        assert!(
            detail.contains("102") && detail.contains("limit_hit"),
            "the phase failure says what it saw and what was missing: {detail}"
        );
    }

    #[test]
    fn every_other_ending_establishes_nothing_either() {
        for code in [
            exit::SETUP,
            exit::TIMEOUT,
            exit::SPAWN,
            exit::CONTROL_CANCELLED,
            exit::INTERNAL,
        ] {
            let outcome = classify_resource_outcome(Some(i32::from(code)), &[container_failed()]);
            assert!(
                matches!(outcome, ResourceOutcome::Undecided(_)),
                "code {code} says nothing about a resource controller: {outcome:?}"
            );
        }
    }

    #[test]
    fn a_reported_limit_under_an_unrecognized_code_is_not_a_verdict_either() {
        // The strict direction: this build knows the `limit_hit` + `BACKEND` pairing,
        // and a stream that reports a limit under some other ending is a combination
        // it does not recognize — "nobody established it" rather than a negative read
        // off half a signal.
        let outcome = classify_resource_outcome(Some(i32::from(exit::SETUP)), &[limit_hit("cpu")]);
        assert!(
            matches!(outcome, ResourceOutcome::Undecided(_)),
            "an unrecognized pairing decides nothing: {outcome:?}"
        );
    }

    #[test]
    fn a_signal_ending_establishes_nothing() {
        let outcome = classify_resource_outcome(None, &[]);
        let ResourceOutcome::Undecided(detail) = outcome else {
            panic!("a killed scratch run decides nothing: {outcome:?}");
        };
        assert!(
            detail.contains("signal"),
            "the phase failure names how the run ended: {detail}"
        );
    }

    #[test]
    fn a_limit_hit_missing_its_own_fields_still_reads_as_data() {
        // The stream is read as data, never trusted for a shape: an event without its
        // `limit`/`detail` fields still yields a usable refusal rather than a panic or
        // an empty string.
        let bare = json!({ "event": "limit_hit" });
        let outcome = classify_resource_outcome(Some(i32::from(exit::BACKEND)), &[bare]);
        let ResourceOutcome::Refused(detail) = outcome else {
            panic!("the pairing still decides: {outcome:?}");
        };
        assert!(
            detail.contains("without naming which"),
            "the detail is still a sentence, and says what it could not read: {detail}"
        );
    }

    #[test]
    fn the_last_limit_hit_in_a_stream_is_the_one_read() {
        // `event` reads the last occurrence; a stream carrying more than one must not
        // silently report the first.
        let outcome = classify_resource_outcome(
            Some(i32::from(exit::BACKEND)),
            &[limit_hit("memory"), limit_hit("cpu")],
        );
        let ResourceOutcome::Refused(detail) = outcome else {
            panic!("a reported limit is a decided verdict: {outcome:?}");
        };
        assert!(detail.contains("cpu"), "{detail}");
    }
}
