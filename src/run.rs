//! The `run` subcommand: launch one shell-free program inside a ProcessKit
//! container, echo its output live, forward its exit code faithfully, and bound
//! the run with a hard `--timeout` and an interactive `Ctrl-C` cancel.
//!
//! This is the first executable path of the runner (see `docs/ROADMAP.md`,
//! "Runnable containment shell"). It builds strictly on the public `processkit`
//! API — the single source of truth for containment and teardown — and never
//! reimplements any of it (`AGENTS.md`, "Build strictly on the public
//! `processkit` API"). Four settled decisions are realized here:
//!
//! - **Own the group.** The child is spawned into a [`ProcessGroup`] this module
//!   owns, not a shared/global one, so the group's kernel-backed kill-on-drop —
//!   a Windows Job Object close, a Linux cgroup/POSIX-group teardown — reaps the
//!   whole tree (including any leaked grandchild) when the group drops, on every
//!   exit path. The teardown is the group's, never a hand-rolled wait/cleanup
//!   loop on top of it. The group is dropped only *after* the outcome is decided.
//! - **Live output is pipe + echo, not fd inheritance.** processkit's line pump
//!   reads the child's stdout/stderr and tees each line to *our* stdout/stderr.
//!   The child therefore sees no TTY (documented in `README.md`: colors and
//!   progress bars may degrade). Streams stay strictly separated — child stdout
//!   to our stdout, child stderr to our stderr — and no runner diagnostic is ever
//!   written to the child's stdout.
//! - **Exit-code fidelity, with distinguishable runner-imposed endings.** On a
//!   completed run the process exits with the child's *exact* code (full width,
//!   never clamped). When the runner instead *ends* the run — the `--timeout`
//!   deadline elapsed, the operator pressed `Ctrl-C`, or a control-plane
//!   `cancel`/`kill` command reached the live runner — the child did not choose to
//!   stop, so its code is not forwarded: the run reports a reserved-band code
//!   ([`exit::TIMEOUT`] / [`exit::CANCELLED`] / [`exit::CONTROL_CANCELLED`] /
//!   [`exit::CONTROL_KILLED`]) and an explanatory stderr line, kept distinct from
//!   each other and from any child result. Their machine-readable JSONL form is the
//!   `timeout` / `cancelled` / `killed` (plus terminal `runner_exit`) event written
//!   to `--jsonl` (see [`crate::events`] and `docs/schema.md`). The control-plane
//!   endings reuse the *same* teardown as the local ones — `cancel` runs the shared
//!   soft-stop → grace → hard-kill path, `kill` hard-kills the tree at once — so a
//!   remote command never invents a parallel termination mechanism.
//! - **One teardown path for every ending, honest per platform.** The deadline
//!   and the cancel share a single termination path: attempt a *soft* stop
//!   (`SIGTERM` to the whole tree on Unix), wait out `--grace`, then let the owning
//!   group's kill-on-drop hard-tear-down the tree. On **Windows** there is no
//!   soft-signal tier in the ProcessKit kernel yet (tracked in ProcessKit-rs's
//!   backlog), so no soft signal is sent — the grace window still elapses and the
//!   Job Object is then killed atomically. The runner never *pretends* a soft stop
//!   happened when it could not: the stderr message states exactly what the
//!   platform did (see [`describe_teardown`]).

use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use processkit::{
    Command as PkCommand, Error as PkError, Outcome, OutputBufferPolicy, ProcessGroup,
    RunningProcess, Signal,
};

use crate::capture::{CAPTURE_INFLIGHT_MAX_BYTES, Capture};
use crate::cli::RunArgs;
use crate::control::{self, SnapshotSource};
use crate::events::{self, Emitter, Event, Member};
use crate::exit::{self, RunnerError};
use crate::registry;

/// Execute the `run` subcommand and turn the result into a process exit code.
///
/// On a completed container the child's code is forwarded verbatim via
/// [`std::process::exit`], which preserves the full 32-bit width (a Windows code
/// such as `STATUS_CONTROL_C_EXIT` is not clamped to a `u8`). That hard exit
/// skips destructors, which is *only* safe because the container has already been
/// torn down inside [`run_inner`] — the owning [`ProcessGroup`] drops before this
/// function regains control. A runner-own failure (including a `--timeout` or a
/// `Ctrl-C` cancel) instead reports to stderr (never the child's stdout) and
/// returns a code from the reserved band.
pub fn execute(args: RunArgs) -> ExitCode {
    match run_inner(args) {
        Ok(child_code) => std::process::exit(child_code),
        Err(err) => {
            eprintln!("processkit-cli: {err}");
            ExitCode::from(err.code())
        }
    }
}

/// Build the async runtime and drive one run to its exit code.
///
/// The runtime and the container both live for the duration of [`run_async`];
/// when it returns the group has already dropped (teardown done), so the caller
/// may hard-exit with the child's code.
fn run_inner(args: RunArgs) -> Result<i32, RunnerError> {
    // A small current-thread runtime is enough: the run is one child plus its
    // output pumps, a deadline timer, and a Ctrl-C listener. `enable_all` arms the
    // I/O, time, and signal drivers those need — the child-pipe I/O driver is
    // compiled in through `processkit`'s own tokio `process`/`net` features, and
    // the `time`/`signal` features this crate now requests arm the rest (Cargo
    // unifies them into the single tokio build).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            RunnerError::new(
                exit::INTERNAL,
                format!("could not start the async runtime: {err}"),
            )
        })?;
    runtime.block_on(run_async(args))
}

/// How a run ended — the decision the race in [`run_async`] resolves to.
enum Ending {
    /// The child exited on its own; carries the raw wait result.
    Exited(processkit::Result<Outcome>),
    /// The `--timeout` deadline elapsed while the child was still running.
    TimedOut,
    /// The operator pressed `Ctrl-C`.
    Cancelled,
    /// A control-plane `cancel` command reached the live runner: the same soft-stop →
    /// grace → hard-kill teardown as `Ctrl-C`, only triggered over the network.
    ControlCancelled,
    /// A control-plane `kill` command reached the live runner: an immediate hard kill
    /// of the whole tree, no soft stop and no grace.
    ControlKilled,
}

/// A runner-imposed ending that shares the soft-stop → grace → hard-kill teardown
/// (the `kill` verb is *not* one — it hard-kills immediately, handled separately).
enum Termination {
    /// The `--timeout` deadline (the elapsed limit) was exceeded.
    Timeout(Duration),
    /// The run was cancelled interactively (`Ctrl-C`).
    Cancelled,
    /// The run was cancelled by a control-plane `cancel` command.
    ControlCancelled,
}

/// What the *soft* stop actually did, recorded so the outcome is reported
/// honestly rather than by assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoftTerminate {
    /// A real soft signal (`SIGTERM`) was delivered to the whole tree (Unix).
    Signalled,
    /// The platform has no soft-terminate tier yet (Windows): nothing was sent,
    /// and we do not claim otherwise.
    Unsupported,
    /// The soft signal could not be delivered; the run falls through to the hard
    /// kill regardless.
    Failed,
}

/// Own a group, spawn the child into it, stream its output live, write the JSONL
/// lifecycle events, and report how the run ended. The group drops when this
/// future completes — on success or on any error path — which is what tears the
/// container down.
///
/// **Event invariant.** Every return path emits exactly one terminal
/// [`Event::RunnerExit`] as its last event, so a child's code is recorded out of
/// band even on the runner's own failure (`AGENTS.md`, "Exit-code fidelity").
async fn run_async(args: RunArgs) -> Result<i32, RunnerError> {
    // clap guarantees at least one token (`num_args = 1..`, `required = true`).
    let (program, program_args) = args
        .command
        .split_first()
        .expect("clap enforces a non-empty command after `--`");

    // Open the event stream *first*, before anything is spawned. `--jsonl` is a
    // required, first-class output, so a file we cannot even create is a
    // fail-closed setup error reported before the child runs — no child code can
    // be lost to a logging failure. Once open, later write failures are
    // best-effort (see `Emitter`), never a reason to abort a healthy run.
    let mut emitter = Emitter::create(&args.jsonl).map_err(|err| {
        RunnerError::new(
            exit::INTERNAL,
            format!(
                "could not open the JSONL events file `{}`: {err}",
                args.jsonl.display()
            ),
        )
    })?;

    // Open the bounded capture files (`--capture-dir`) before anything is spawned.
    // Like `--jsonl`, a capture the operator explicitly asked for but that cannot be
    // created is a fail-closed setup error reported here — no child code is at risk
    // yet — rather than a silently-dropped diagnostic. Left `None` when the flag is
    // absent, so a run without capture is byte-for-byte unchanged (no policy, no
    // extra event, no capture files).
    let capture = match args.capture_dir.as_deref() {
        Some(dir) => match Capture::create(dir) {
            Ok(capture) => Some(capture),
            Err(err) => {
                let error = RunnerError::new(
                    exit::INTERNAL,
                    format!(
                        "could not set up output capture in `{}`: {err}",
                        dir.display()
                    ),
                );
                return Err(finish(&mut emitter, "internal", None, error));
            }
        },
        None => None,
    };

    // We own this group; the child — and anything it spawns — is a member. When
    // `group` drops at the end of this scope (every path below), the kernel reaps
    // the whole tree. Containment/teardown is the group's job; we never duplicate
    // it (`AGENTS.md`, "Never clean up by process name").
    let group = match ProcessGroup::new() {
        Ok(group) => group,
        Err(err) => {
            let error = RunnerError::new(
                exit::BACKEND,
                format!("could not create the ProcessKit container: {err}"),
            );
            emitter.emit(&Event::ContainerFailed {
                phase: "create",
                code: error.code(),
                message: err.to_string(),
            });
            return Err(finish(&mut emitter, "container_error", None, error));
        }
    };

    // Stand up the control plane *before* the child spawns, so a control-plane client
    // (`inspect`, T-008) can find and reach the live runner for the whole run:
    //
    // 1. open the per-user registry (keyed by `run_id`, never a PID),
    // 2. bind the local transport (unix socket / Windows named pipe, owner-only), and
    // 3. publish the transport's endpoint in the run's registry record.
    //
    // All three are best-effort discovery infrastructure: a failure warns and
    // degrades (no endpoint / no server / no entry) but never costs the child its
    // faithfully forwarded exit code. `registration` holds the liveness lock for the
    // whole run (so clients tell a live entry from a stale one) and its `Drop` is a
    // backstop that removes the entry on the early error returns below;
    // `control_server` is served concurrently with the output pump in the race below.
    // `started` and `run_id` are resolved once here and reused for the registry
    // record, the `run_started` event, and the control snapshot.
    let started = SystemTime::now();
    let run_id = events::resolve_run_id(args.run_id.as_deref());
    let registry_handle = open_registry();
    let control_server = registry_handle
        .as_ref()
        .and_then(|registry| control::open_server(registry.dir()));
    let endpoint = control_server
        .as_ref()
        .map(|server| server.endpoint().to_string());
    let registration = registry_handle
        .as_ref()
        .and_then(|registry| register_run(registry, &run_id, endpoint.as_deref(), started));

    let mut command = PkCommand::new(program).args(program_args);
    // Abrupt runner death skips ProcessGroup::drop. ProcessKit can still harden the
    // direct child on Linux via PR_SET_PDEATHSIG; Windows already gets the stronger
    // whole-tree guarantee from Job Object kill-on-close, while macOS/BSD document a
    // no-op. This is deliberately unconditional so the actual platform capability is
    // always enabled without pretending it covers Unix grandchildren.
    command = command.kill_on_parent_death();
    // Default cwd is the runner's own current directory (processkit leaves it
    // unset), so only override when `--cwd` was given.
    if let Some(cwd) = &args.cwd {
        command = command.current_dir(cwd);
    }
    // `--create-no-window` maps straight onto `Command::create_no_window()`
    // (`CREATE_NO_WINDOW` on Windows, a no-op elsewhere). Default: OFF. A bare
    // `run` should behave like launching the child directly, so we do not force
    // the flag — that would diverge from a direct launch and could hide a child
    // that legitimately wants its own console. Headless Windows deployments
    // (Orchestra) that must avoid a stray `conhost` window pass the flag
    // explicitly; the runner itself never allocates a console, so it spawns no
    // extra host on its own account. (See README, "Windows console".)
    if args.create_no_window {
        command = command.create_no_window();
    }
    // Pipe + echo: the pump reads the child's piped stdout/stderr and tees each
    // decoded line to our own stdout/stderr. This is the live-output mechanism —
    // not true fd inheritance — so the child gets no TTY, and the two streams are
    // never crossed or mixed with runner diagnostics.
    //
    // With `--capture-dir` the *same* tee also mirrors each stream into a bounded
    // capture file, and the child is drained under a byte-capped
    // `OutputBufferPolicy` so the pump's in-flight line assembly is bounded by the
    // kernel — the runner adds no output-draining or volume-limiting of its own
    // (see `src/capture.rs`). The live echo is unchanged either way.
    command = match &capture {
        Some(capture) => command
            .output_buffer(
                OutputBufferPolicy::bounded(0).with_max_bytes(CAPTURE_INFLIGHT_MAX_BYTES),
            )
            .stdout_tee(capture.stdout_tee(tokio::io::stdout()))
            .stderr_tee(capture.stderr_tee(tokio::io::stderr())),
        None => command
            .stdout_tee(tokio::io::stdout())
            .stderr_tee(tokio::io::stderr()),
    };

    // `ProcessGroup::start` joins the child to the group *we* own and hands back a
    // handle that deliberately does not own the group, so dropping the handle
    // leaves teardown with us (`kills_tree_on_drop()` is `false`). The deadline and
    // the cancel are *not* handed to the child handle (via `Command::timeout` /
    // `cancel_on`); we run the race ourselves so both endings share one grace +
    // kill-on-drop teardown, exactly as this task requires.
    let running = match group.start(&command).await {
        Ok(running) => running,
        Err(err) => {
            let error = map_launch_error(&err);
            emitter.emit(&launch_failure_event(&err, &error));
            let source = launch_failure_source(&error);
            return Err(finish(&mut emitter, source, None, error));
        }
    };

    // The root PID must be read *before* the race moves `running` into `wait()`. The
    // mechanism is settled now too; both are reused by the `run_started` event and the
    // control-plane snapshot below.
    let root_pid = running.pid();
    let mechanism = events::mechanism_str(group.mechanism());
    emitter.emit(&Event::RunStarted {
        run_id: run_id.clone(),
        root_pid,
        mechanism,
        abrupt_cleanup: events::abrupt_cleanup_str(),
        cwd: resolve_cwd(&args),
        command: events::CommandInfo::for_argv(&args.command, args.argv_raw),
    });
    emit_members_snapshot(&mut emitter, &group);

    // What the control server answers an `inspect` with. `members` is a live query of
    // the owning container, so a snapshot reflects the tree's composition *when
    // inspected* — the same PID-only view the `members_snapshot` event carries.
    let members_provider = || {
        group
            .members()
            .map(|pids| pids.into_iter().map(Member::from_pid).collect())
            .unwrap_or_default()
    };
    let snapshot_source =
        SnapshotSource::new(&run_id, mechanism, root_pid, started, &members_provider);

    // The channel the control server signals a mutating `cancel`/`kill` verb through.
    // The server (in the `select!` below) writes its client ack first, then sends the
    // command here; this loop's `recv` arm then wins the race and drives teardown. The
    // sender lives for the whole `select!`, so `recv` only yields `None` at teardown.
    let (command_tx, mut command_rx) =
        tokio::sync::mpsc::unbounded_channel::<control::ControlCommand>();

    let timeout = args.timeout;
    let grace = args.grace;

    // Race the child's own exit against the two runner-imposed endings. Whichever
    // fires first *decides* the outcome; only then does teardown begin, so the
    // owning group is never dropped before the outcome is known.
    //
    // `biased` order — Ctrl-C, natural exit, control command, deadline, then the
    // control server — makes the tie-breaks deliberate: a `Ctrl-C` always wins, and a
    // child that exits in the very poll a deadline or a control command fires is
    // reported as its own exit rather than a runner-imposed ending (natural exit is
    // polled before both). When a cancel/kill/deadline branch wins, `running` (moved
    // into `wait()`) is dropped; because this is a shared-group handle that does not
    // kill on drop, the child stays alive for the teardown path below, and its output
    // pumps stop (teardown is underway). The `command_rx` branch resolves when the
    // control server routes a `cancel`/`kill` verb (having already acked the client);
    // the control-server branch itself **never resolves** (its output is `Infallible`)
    // — it serves clients concurrently with the output pump, so it neither delays the
    // child's exit nor blocks teardown, and is dropped (tearing the transport down)
    // when another branch wins.
    let capturing = capture.is_some();
    let ending = tokio::select! {
        biased;
        () = wait_for_ctrl_c() => Ending::Cancelled,
        outcome = drive_to_outcome(running, capturing) => Ending::Exited(outcome),
        command = command_rx.recv() => match command {
            Some(control::ControlCommand::Cancel) => Ending::ControlCancelled,
            Some(control::ControlCommand::Kill) => Ending::ControlKilled,
            // The sender lives as long as the serve future in this same `select!`, so
            // a closed channel cannot happen while this arm is racing; park if it ever
            // did rather than misreport an ending.
            None => std::future::pending().await,
        },
        () = deadline(timeout) => Ending::TimedOut,
        never = control::serve(control_server, &snapshot_source, &command_tx) => match never {},
    };

    match ending {
        Ending::Exited(outcome) => {
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(err) => {
                    // The wait itself failed (the child's fate is unknown), but the
                    // container was still spawned and may still hold live members —
                    // this is a decided ending like any other, not a setup failure,
                    // so it must run the very same teardown tail as every other
                    // branch below rather than returning through the bare `finish`
                    // a setup-time failure uses. Hard-kill (there is no outcome to
                    // soft-stop toward), same as the natural-exit and control-kill
                    // paths.
                    let error = RunnerError::new(
                        exit::INTERNAL,
                        format!("waiting for the child to exit failed: {err}"),
                    );
                    emit_hard_teardown(&mut emitter, &group, &capture, &registration);
                    return Err(finish(&mut emitter, "internal", None, error));
                }
            };
            let (outcome_str, code, signal) = events::outcome_fields(&outcome);
            emitter.emit(&Event::RootExited {
                outcome: outcome_str,
                code,
                signal,
            });
            let child_code = match exit_code_for(outcome) {
                Ok(child_code) => child_code,
                Err(error) => return Err(finish(&mut emitter, "internal", None, error)),
            };
            // Reap any descendant the exited child leaked behind, report the
            // capture, and drop the registry entry — the shared hard-teardown tail
            // (no soft stop is attempted on the natural-exit path).
            emit_hard_teardown(&mut emitter, &group, &capture, &registration);
            emitter.emit(&Event::RunnerExit {
                code: child_code,
                source: "child_exit",
                child_code: Some(child_code),
            });
            Ok(child_code)
            // `group` drops here (a no-op after the explicit kill above).
        }
        Ending::TimedOut => {
            let limit = timeout.expect("the deadline arm only fires when --timeout is set");
            emitter.emit(&Event::Timeout {
                timeout_ms: duration_ms(limit),
                grace_ms: grace.map(duration_ms),
            });
            // `cleanup_started` brackets the whole teardown — soft stop, grace, and
            // hard kill — so `members_before` is the full tree, not a post-soft remnant.
            emit_cleanup_started(&mut emitter, &group);
            let soft = soft_terminate_then_grace(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(soft_terminate_label(soft)));
            // A forced ending still reports whatever was captured before teardown.
            emit_output_captured(&mut emitter, &capture);
            // The registry entry is removed on every decided ending, not just the
            // happy path: a timeout tears the run down cleanly too.
            clear_registration(&registration);
            let error = termination_error(Termination::Timeout(limit), soft, grace);
            Err(finish(&mut emitter, "timeout", None, error))
        }
        Ending::Cancelled => {
            emitter.emit(&Event::Cancelled {
                source: "ctrl_c",
                grace_ms: grace.map(duration_ms),
            });
            emit_cleanup_started(&mut emitter, &group);
            let soft = soft_terminate_then_grace(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(soft_terminate_label(soft)));
            // A forced ending still reports whatever was captured before teardown.
            emit_output_captured(&mut emitter, &capture);
            // A Ctrl-C cancel tears the run down cleanly too — its entry goes with it.
            clear_registration(&registration);
            let error = termination_error(Termination::Cancelled, soft, grace);
            Err(finish(&mut emitter, "cancelled", None, error))
        }
        Ending::ControlCancelled => {
            // A control-plane cancel is the network analogue of Ctrl-C: the *same*
            // `cancelled` event and teardown, told apart only by its `source` and its
            // own reserved exit code (`CONTROL_CANCELLED`, 108).
            emitter.emit(&Event::Cancelled {
                source: "control_cancel",
                grace_ms: grace.map(duration_ms),
            });
            emit_cleanup_started(&mut emitter, &group);
            let soft = soft_terminate_then_grace(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(soft_terminate_label(soft)));
            emit_output_captured(&mut emitter, &capture);
            clear_registration(&registration);
            let error = termination_error(Termination::ControlCancelled, soft, grace);
            Err(finish(&mut emitter, "control_cancel", None, error))
        }
        Ending::ControlKilled => {
            // A control-plane kill is immediate: no soft stop, no grace. The dedicated
            // `killed` event marks the reason; `cleanup_finished` carries `None` for
            // `soft_terminate`, exactly like the natural-exit path where no soft stop
            // is attempted. The single hard kill is the container's kill-on-drop, run
            // explicitly via `emit_cleanup_finished`.
            emitter.emit(&Event::Killed {
                source: "control_kill",
            });
            emit_hard_teardown(&mut emitter, &group, &capture, &registration);
            let error = control_kill_error();
            Err(finish(&mut emitter, "control_kill", None, error))
        }
    }
}

/// Emit the terminal [`Event::RunnerExit`] for a runner-own failure and return the
/// error unchanged, so each failing path reads as one expression. `source` names
/// the ending and `child_code` carries the child's own code when one exists (it is
/// `None` for every runner-own failure, where the child never produced one).
fn finish(
    emitter: &mut Emitter,
    source: &'static str,
    child_code: Option<i32>,
    error: RunnerError,
) -> RunnerError {
    emitter.emit(&Event::RunnerExit {
        code: i32::from(error.code()),
        source,
        child_code,
    });
    error
}

/// Drive the child to its exit, returning the raw wait result the race resolves
/// to.
///
/// With capture on the child is drained through [`RunningProcess::output_string`]
/// so the byte-capped [`OutputBufferPolicy`] set on the command is actually honored
/// (the discarding [`RunningProcess::wait`] applies its own fixed discard policy and
/// ignores the command's); the retained text is discarded — the transcript is the
/// capturing tee's job — and only the [`Outcome`] is kept. Without capture it is the
/// plain `wait`, exactly as before. **Both paths share one bounded teardown spine**
/// (ProcessKit's `PUMP_TEARDOWN`): a descendant that keeps a stdout/stderr handle
/// open past the root's exit cannot hang the runner in either mode — the pump drain
/// is time-bounded, not the runner's to police.
async fn drive_to_outcome(running: RunningProcess, capturing: bool) -> processkit::Result<Outcome> {
    if capturing {
        running.output_string().await.map(|result| result.outcome())
    } else {
        running.wait().await
    }
}

/// Emit the terminal [`Event::OutputCaptured`] for a run that had `--capture-dir`,
/// finalizing both streams' files and metadata first. A no-op without capture, so a
/// run that did not request it emits no such event (backward compatibility).
fn emit_output_captured(emitter: &mut Emitter, capture: &Option<Capture>) {
    if let Some(capture) = capture {
        let (stdout, stderr) = capture.finalize();
        emitter.emit(&Event::OutputCaptured { stdout, stderr });
    }
}

/// The shared **hard** teardown tail — mark cleanup started, hard-kill the
/// container immediately (no soft stop), report the capture, and drop the
/// registry entry, in that order — for every decided ending that has no
/// soft-stop tier of its own: a clean natural exit, a wait failure (the
/// child's fate is unknown, so there is no outcome to soft-stop toward
/// either), and a control-plane `kill`. Routing all three through this one
/// site makes it structurally impossible for one of them to again drift from
/// the others, as the wait-failure branch once did (it used to return
/// through the bare [`finish`] instead, skipping this whole tail).
///
/// The three endings with a soft-stop tier (`timeout` / `cancel` /
/// `control_cancel`, in [`run_async`]'s `Ending` match) are not funneled
/// through here: they run `soft_terminate_then_grace` between
/// `cleanup_started` and `cleanup_finished`, so their `cleanup_finished`
/// carries `Some(label)` instead of this function's fixed `None`. That is
/// the *only* difference in their tail — every other step matches this one.
fn emit_hard_teardown(
    emitter: &mut Emitter,
    group: &ProcessGroup,
    capture: &Option<Capture>,
    registration: &Option<registry::Registration>,
) {
    emit_cleanup_started(emitter, group);
    emit_cleanup_finished(emitter, group, None);
    emit_output_captured(emitter, capture);
    clear_registration(registration);
}

/// Open the per-user run registry so control-plane clients (`inspect`, T-008) can
/// find the live runner.
///
/// **Best-effort by design.** A failure is reported on stderr but never aborts an
/// otherwise-healthy run: the registry is control-plane *discovery* infrastructure,
/// separate from the containment the run depends on. Losing it only makes this run
/// un-inspectable — it must never cost the child its faithfully forwarded exit code
/// (`AGENTS.md`, "Exit-code fidelity"; the same degradation as
/// [`emit_members_snapshot`]).
fn open_registry() -> Option<registry::Registry> {
    match registry::Registry::open() {
        Ok(registry) => Some(registry),
        Err(err) => {
            eprintln!("processkit-cli: warning: could not open the run registry: {err}");
            None
        }
    }
}

/// Publish this run's registry record — its `run_id`, its transport `endpoint` (the
/// address a client connects to, or `None` when no transport could be stood up), and
/// the liveness lock the returned [`registry::Registration`] holds for the run.
/// Best-effort, like [`open_registry`]: a failure warns and yields `None`.
fn register_run(
    registry: &registry::Registry,
    run_id: &str,
    endpoint: Option<&str>,
    started: SystemTime,
) -> Option<registry::Registration> {
    match registry.register(run_id, endpoint, started) {
        Ok(registration) => Some(registration),
        Err(err) => {
            eprintln!("processkit-cli: warning: could not create the run registry entry: {err}");
            None
        }
    }
}

/// Remove the registry entry on a decided ending. A no-op when registration was
/// skipped (best-effort) or already removed (idempotent).
fn clear_registration(registration: &Option<registry::Registration>) {
    if let Some(registration) = registration {
        registration.remove();
    }
}

/// The child's working directory as recorded in `run_started`: the explicit
/// `--cwd`, else the runner's own current directory (which processkit inherits),
/// rendered lossily to a string, or `None` if it cannot be resolved.
fn resolve_cwd(args: &RunArgs) -> Option<String> {
    args.cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Snapshot the container's members and emit a PID-only `members_snapshot`. A read
/// failure is a diagnostics gap, not a run failure, so it warns and skips the event.
fn emit_members_snapshot(emitter: &mut Emitter, group: &ProcessGroup) {
    match group.members() {
        Ok(pids) => emitter.emit(&Event::MembersSnapshot {
            members: pids.into_iter().map(Member::from_pid).collect(),
        }),
        Err(err) => {
            eprintln!("processkit-cli: warning: could not snapshot container members: {err}");
        }
    }
}

/// Mark the start of container teardown with the full tree size about to be
/// reaped. Emitted before any termination action (including the soft stop on a
/// runner-imposed ending), so `members_before` is the whole tree, not a post-soft
/// remnant.
fn emit_cleanup_started(emitter: &mut Emitter, group: &ProcessGroup) {
    let members_before = group.members().map(|pids| pids.len()).unwrap_or(0);
    emitter.emit(&Event::CleanupStarted { members_before });
}

/// Hard-kill the container and mark teardown finished with a post-kill member
/// snapshot. The hard kill is [`ProcessGroup::kill_all`] — the group's own kernel
/// teardown, the same mechanism its drop would run — invoked explicitly so
/// `remaining_pids` reflects the post-kill state rather than a pre-drop guess. Any
/// kill error is best-effort: the group's drop is still a backstop. `soft` labels
/// the soft-stop tier of a runner-imposed ending, or `None` on the natural-exit
/// path where no soft stop was attempted.
fn emit_cleanup_finished(emitter: &mut Emitter, group: &ProcessGroup, soft: Option<&'static str>) {
    let _ = group.kill_all();
    let remaining_pids = group.members().unwrap_or_default();
    emitter.emit(&Event::CleanupFinished {
        remaining: remaining_pids.len(),
        remaining_pids,
        soft_terminate: soft,
    });
}

/// The machine label for a soft-stop tier, mirroring the honest stderr message.
fn soft_terminate_label(soft: SoftTerminate) -> &'static str {
    match soft {
        SoftTerminate::Signalled => "signalled",
        SoftTerminate::Unsupported => "unsupported",
        SoftTerminate::Failed => "failed",
    }
}

/// A duration as whole milliseconds for the JSONL timing fields (`u64` is ample
/// for any deadline a run could carry; the source `Duration` is already bounded by
/// the CLI parser).
fn duration_ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// The launch-failure event for a backend error, chosen by the runner-own code
/// rather than by re-matching the backend error: [`exit::SPAWN`] is a
/// `spawn_failed`, anything else a `container_failed` at the `attach` phase.
fn launch_failure_event(err: &PkError, error: &RunnerError) -> Event {
    if error.code() == exit::SPAWN {
        Event::SpawnFailed {
            code: error.code(),
            message: err.to_string(),
        }
    } else {
        Event::ContainerFailed {
            phase: "attach",
            code: error.code(),
            message: err.to_string(),
        }
    }
}

/// The `runner_exit` `source` for a launch failure, paired with
/// [`launch_failure_event`].
fn launch_failure_source(error: &RunnerError) -> &'static str {
    if error.code() == exit::SPAWN {
        "spawn_error"
    } else {
        "container_error"
    }
}

/// The runner-imposed deadline: sleep `limit`, or (with no `--timeout`) never
/// resolve, so the race falls through to the other arms.
async fn deadline(limit: Option<Duration>) {
    match limit {
        Some(limit) => tokio::time::sleep(limit).await,
        None => std::future::pending::<()>().await,
    }
}

/// Resolve when the operator presses `Ctrl-C`. If the signal handler cannot be
/// installed we degrade to "no cancel" (never resolving) after an honest warning,
/// rather than aborting an otherwise-healthy run.
async fn wait_for_ctrl_c() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => {}
        Err(err) => {
            eprintln!("processkit-cli: warning: Ctrl-C handling is unavailable: {err}");
            std::future::pending::<()>().await;
        }
    }
}

/// The shared teardown path for both runner-imposed endings: try a soft stop,
/// wait out `--grace`, and report what the soft stop actually did. The *hard*
/// teardown is not done here — the caller drops the owning [`ProcessGroup`]
/// afterwards, and its kernel-backed kill-on-drop is the single hard-kill path.
///
/// On Unix the soft stop is a `SIGTERM` broadcast to the whole tree. On Windows
/// [`ProcessGroup::signal`] supports only `Signal::Kill`, so a `SIGTERM` request
/// returns [`PkError::Unsupported`]: no soft signal is delivered, and we record
/// that faithfully instead of pretending. Either way the grace window still
/// elapses (giving a child that *can* stop — e.g. one that received the console's
/// own `Ctrl-C` on Windows — a chance to exit first) before the atomic kill.
async fn soft_terminate_then_grace(group: &ProcessGroup, grace: Option<Duration>) -> SoftTerminate {
    let soft = match group.signal(Signal::Term) {
        Ok(()) => SoftTerminate::Signalled,
        Err(PkError::Unsupported { .. }) => SoftTerminate::Unsupported,
        // Best-effort: a delivery failure does not stop teardown — the group's
        // kill-on-drop still reaps the tree — but it is reported honestly.
        Err(_) => SoftTerminate::Failed,
    };
    if let Some(grace) = grace {
        tokio::time::sleep(grace).await;
    }
    soft
}

/// Turn a runner-imposed ending into the reserved-band error it surfaces:
/// [`exit::TIMEOUT`] / [`exit::CANCELLED`] plus a message that names the ending
/// and describes, truthfully, how the tree was torn down.
fn termination_error(
    kind: Termination,
    soft: SoftTerminate,
    grace: Option<Duration>,
) -> RunnerError {
    let (code, headline) = match kind {
        Termination::Timeout(limit) => (
            exit::TIMEOUT,
            format!("run timed out after {}", format_duration(limit)),
        ),
        Termination::Cancelled => (exit::CANCELLED, "run cancelled (Ctrl-C)".to_string()),
        Termination::ControlCancelled => (
            exit::CONTROL_CANCELLED,
            "run cancelled by a control-plane command".to_string(),
        ),
    };
    RunnerError::new(
        code,
        format!("{headline}: {}", describe_teardown(soft, grace)),
    )
}

/// The error a control-plane `kill` surfaces: the reserved [`exit::CONTROL_KILLED`]
/// and a message stating, truthfully, that the whole tree was hard-killed at once —
/// no soft stop, no grace. Unlike [`termination_error`] there is no soft-terminate
/// tier or grace window to describe, because a kill has neither.
fn control_kill_error() -> RunnerError {
    RunnerError::new(
        exit::CONTROL_KILLED,
        "run killed by a control-plane command: hard-killed the whole process tree \
         immediately via the container's kill-on-drop (no soft stop, no grace)"
            .to_string(),
    )
}

/// A truthful, human-readable description of the teardown that just happened —
/// the load-bearing part of the "honest degradation" contract. It states whether
/// a real soft signal was delivered, whether a grace window was waited, and that
/// the hard kill is the container's kill-on-drop (a Windows Job Object terminate).
fn describe_teardown(soft: SoftTerminate, grace: Option<Duration>) -> String {
    let waited = match grace {
        Some(grace) => format!("waited {} grace, then ", format_duration(grace)),
        None => String::new(),
    };
    match soft {
        SoftTerminate::Signalled => format!(
            "sent SIGTERM to the process tree, {waited}hard-killed it via the container's kill-on-drop"
        ),
        SoftTerminate::Unsupported => format!(
            "Windows has no soft-terminate signal yet, so — after {}— the process tree was \
             hard-killed atomically via the Job Object",
            match grace {
                Some(grace) => format!("a {} grace delay ", format_duration(grace)),
                None => "no grace delay ".to_string(),
            }
        ),
        SoftTerminate::Failed => format!(
            "the soft-terminate signal could not be delivered, so {waited}the process tree was \
             hard-killed via the container's kill-on-drop"
        ),
    }
}

/// A compact, honest rendering of a duration for diagnostics: whole seconds when
/// it divides evenly (`5s`), otherwise milliseconds (`500ms`). Not a full
/// human-time formatter — just enough to echo the deadline/grace back clearly.
fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms != 0 && ms.is_multiple_of(1_000) {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// Map a `processkit` launch failure onto the runner-own exit-code band.
///
/// A locate/start failure is [`exit::SPAWN`] — the child never ran; every other
/// backend/containment failure is [`exit::BACKEND`]. A child's own exit is never
/// routed through here (it is an [`Outcome`], not an [`Err`]).
fn map_launch_error(err: &PkError) -> RunnerError {
    match err {
        PkError::NotFound { .. } | PkError::Spawn { .. } => {
            RunnerError::new(exit::SPAWN, format!("could not start the program: {err}"))
        }
        other => RunnerError::new(
            exit::BACKEND,
            format!("ProcessKit backend failure: {other}"),
        ),
    }
}

/// Derive the process exit code from a completed run's [`Outcome`].
///
/// A clean exit forwards the child's code untouched. A signal death — Unix only;
/// Windows reports [`Outcome::Exited`] even for `Ctrl-C` — has no code of its
/// own, so it is rendered as `128 + signo`, the POSIX shell convention. That sits
/// above the runner-own band, so it can never be mistaken for a runner failure or
/// a child code. A `TimedOut` outcome cannot occur here: the runner arms no
/// `Command::timeout` (the `--timeout` deadline is raced in [`run_async`] and
/// reported as [`exit::TIMEOUT`] instead), so a `TimedOut` from the backend is an
/// invariant violation rather than a result.
fn exit_code_for(outcome: Outcome) -> Result<i32, RunnerError> {
    match outcome {
        Outcome::Exited(code) => Ok(code),
        Outcome::Signalled(Some(signal)) => Ok(128 + (signal & 0x7f)),
        Outcome::Signalled(None) => Ok(128),
        Outcome::TimedOut => Err(RunnerError::new(
            exit::INTERNAL,
            "the run reported a timeout, but no deadline was armed on the child",
        )),
        // `Outcome` is `#[non_exhaustive]`; a variant this build predates cannot
        // be faithfully rendered as a child code, so report a runner fault rather
        // than guess at one.
        _ => Err(RunnerError::new(
            exit::INTERNAL,
            "the run produced an outcome this build does not recognize",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exited_code_is_forwarded_verbatim() {
        assert_eq!(exit_code_for(Outcome::Exited(0)).unwrap(), 0);
        assert_eq!(exit_code_for(Outcome::Exited(7)).unwrap(), 7);
        // Full-width Windows codes survive: no clamp to a u8.
        assert_eq!(
            exit_code_for(Outcome::Exited(-1073741510)).unwrap(),
            -1073741510
        );
    }

    #[test]
    fn signal_death_uses_the_posix_convention() {
        // 128 + SIGKILL(9), 128 + SIGTERM(15).
        assert_eq!(exit_code_for(Outcome::Signalled(Some(9))).unwrap(), 137);
        assert_eq!(exit_code_for(Outcome::Signalled(Some(15))).unwrap(), 143);
        assert_eq!(exit_code_for(Outcome::Signalled(None)).unwrap(), 128);
    }

    #[test]
    fn a_timeout_without_a_deadline_is_a_runner_fault() {
        let err = exit_code_for(Outcome::TimedOut).unwrap_err();
        assert_eq!(err.code(), exit::INTERNAL);
    }

    #[test]
    fn other_backend_failures_map_to_the_backend_code() {
        // `NotFound`/`Spawn` are `#[non_exhaustive]`, so they cannot be built
        // here; the SPAWN mapping is proved through the binary instead (running a
        // program that does not exist — see `tests/run.rs`). Every remaining
        // launch failure lands on the BACKEND code.
        let io = map_launch_error(&PkError::Io(std::io::Error::from(
            std::io::ErrorKind::AddrInUse,
        )));
        assert_eq!(io.code(), exit::BACKEND);
    }

    #[test]
    fn timeout_and_cancel_carry_distinct_reserved_codes() {
        let timed_out = termination_error(
            Termination::Timeout(Duration::from_secs(5)),
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let cancelled = termination_error(
            Termination::Cancelled,
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        assert_eq!(timed_out.code(), exit::TIMEOUT);
        assert_eq!(cancelled.code(), exit::CANCELLED);
        assert_ne!(timed_out.code(), cancelled.code());
    }

    #[test]
    fn timeout_message_names_the_ending_and_the_limit() {
        let err = termination_error(
            Termination::Timeout(Duration::from_secs(5)),
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("timed out"),
            "message should name the timeout: {msg}"
        );
        assert!(msg.contains("5s"), "message should echo the limit: {msg}");
    }

    #[test]
    fn cancel_message_names_ctrl_c() {
        let err = termination_error(Termination::Cancelled, SoftTerminate::Signalled, None);
        let msg = err.to_string();
        assert!(
            msg.contains("cancelled"),
            "message should say cancelled: {msg}"
        );
        assert!(msg.contains("Ctrl-C"), "message should name Ctrl-C: {msg}");
    }

    #[test]
    fn the_four_runner_imposed_endings_carry_distinct_codes() {
        // Every runner-imposed ending must be tellable apart by exit code: a timeout,
        // a Ctrl-C, a control-plane cancel, and a control-plane kill.
        let timeout = termination_error(
            Termination::Timeout(Duration::from_secs(5)),
            SoftTerminate::Signalled,
            None,
        );
        let ctrl_c = termination_error(Termination::Cancelled, SoftTerminate::Signalled, None);
        let control_cancel = termination_error(
            Termination::ControlCancelled,
            SoftTerminate::Signalled,
            None,
        );
        let control_kill = control_kill_error();
        let codes = [
            timeout.code(),
            ctrl_c.code(),
            control_cancel.code(),
            control_kill.code(),
        ];
        assert_eq!(control_cancel.code(), exit::CONTROL_CANCELLED);
        assert_eq!(control_kill.code(), exit::CONTROL_KILLED);
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "two runner-imposed endings collided on code {a}");
            }
        }
    }

    #[test]
    fn control_cancel_message_names_the_command_and_describes_teardown() {
        // A control-plane cancel shares the honest teardown wording (it is the same
        // path as Ctrl-C) but names the *command* as the trigger, not the keyboard.
        let err = termination_error(
            Termination::ControlCancelled,
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let msg = err.to_string();
        assert!(
            msg.contains("control-plane command"),
            "message should name the control command: {msg}"
        );
        assert!(
            !msg.contains("Ctrl-C"),
            "a control cancel is not a Ctrl-C: {msg}"
        );
        assert!(
            msg.contains("SIGTERM"),
            "the shared teardown is described: {msg}"
        );
        assert!(msg.contains("2s"), "the grace is echoed: {msg}");
    }

    #[test]
    fn control_kill_message_is_immediate_and_ungraceful() {
        let err = control_kill_error();
        let msg = err.to_string();
        assert!(msg.contains("killed"), "message should say killed: {msg}");
        assert!(msg.contains("immediately"), "a kill is immediate: {msg}");
        assert!(
            msg.contains("no soft stop") && msg.contains("no grace"),
            "a kill waits for nothing: {msg}"
        );
        assert!(msg.contains("hard-killed"), "the hard kill is named: {msg}");
    }

    #[test]
    fn unix_teardown_reports_a_real_soft_signal_and_the_grace() {
        // Where the soft path exists, the message states the SIGTERM was sent and
        // the grace was waited — no "Windows"/"Job Object" wording.
        let msg = describe_teardown(SoftTerminate::Signalled, Some(Duration::from_secs(2)));
        assert!(msg.contains("SIGTERM"), "{msg}");
        assert!(msg.contains("2s"), "{msg}");
        assert!(msg.contains("grace"), "{msg}");
        assert!(!msg.contains("Windows"), "{msg}");
    }

    #[test]
    fn windows_teardown_is_reported_honestly_without_pretending() {
        // The "honest degradation" contract: when no soft signal could be sent,
        // the message says so plainly and names the atomic Job Object kill — it
        // must never imply a graceful soft-terminate was performed.
        let msg = describe_teardown(SoftTerminate::Unsupported, Some(Duration::from_secs(2)));
        assert!(msg.contains("Windows"), "{msg}");
        assert!(msg.contains("Job Object"), "{msg}");
        assert!(msg.contains("no soft-terminate"), "{msg}");
        assert!(
            !msg.contains("sent SIGTERM"),
            "must not claim a soft signal was delivered: {msg}"
        );
    }

    #[test]
    fn teardown_without_grace_omits_the_grace_wording() {
        let msg = describe_teardown(SoftTerminate::Signalled, None);
        assert!(msg.contains("SIGTERM"), "{msg}");
        assert!(!msg.contains("grace"), "no grace was configured: {msg}");
    }

    #[test]
    fn failed_soft_terminate_is_reported_but_still_hard_kills() {
        let msg = describe_teardown(SoftTerminate::Failed, Some(Duration::from_secs(1)));
        assert!(msg.contains("could not be delivered"), "{msg}");
        assert!(msg.contains("hard-killed"), "{msg}");
    }

    #[test]
    fn format_duration_is_compact_and_honest() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_duration(Duration::ZERO), "0ms");
    }

    /// Forcing a real wait *failure* through the child's actual OS-level wait
    /// call is practically unreachable from a test (`RunningProcess::wait`'s own
    /// `Err` path is backend-internal plumbing, not something a spawned test
    /// child can be made to trigger deterministically). So this proves the
    /// thing that *is* reachable and is the actual fix: [`emit_hard_teardown`],
    /// the exact shared tail the wait-failure branch now runs (see the
    /// `Err(err)` arm of `Ending::Exited` in `run_async`), fires
    /// `cleanup_started` → the hard kill via `cleanup_finished` (with no
    /// soft-terminate tier) → `output_captured` → nothing else, in that order,
    /// for *any* caller — natural exit, control-kill, and the wait-failure path
    /// alike. A future edit that special-cases one of those callers back out of
    /// this shared function (as the wait-failure path used to be) has nowhere
    /// to silently diverge: it would have to stop calling this helper, which is
    /// visible on review.
    #[tokio::test]
    async fn hard_teardown_tail_emits_the_shared_sequence_in_order() {
        let group = ProcessGroup::new().expect("create a ProcessGroup");
        let command = if cfg!(windows) {
            PkCommand::new("cmd").args(["/c", "exit", "0"])
        } else {
            PkCommand::new("true")
        };
        let running = group
            .start(&command)
            .await
            .expect("start a trivial, fast-exiting child");
        running.wait().await.expect("the trivial child exits");

        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-run-unit-hard-teardown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let jsonl = dir.join("events.jsonl");
        let mut emitter = Emitter::create(&jsonl).expect("create the events file");
        // A real `Capture` (not `None`) so `output_captured` actually fires too —
        // proving all three events, not just the two cleanup ones.
        let capture = Some(Capture::create(&dir.join("capture")).expect("create the capture dir"));

        emit_hard_teardown(&mut emitter, &group, &capture, &None);

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&jsonl)
            .expect("read the events file back")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
            .collect();
        let kinds: Vec<&str> = lines
            .iter()
            .map(|value| value["event"].as_str().expect("every event has a tag"))
            .collect();
        assert_eq!(
            kinds,
            vec!["cleanup_started", "cleanup_finished", "output_captured"],
            "the shared hard-teardown tail must emit exactly these three events \
             in this order for every caller"
        );
        assert!(
            lines[1]["soft_terminate"].is_null(),
            "the hard-teardown tail never soft-stops, so cleanup_finished's \
             soft_terminate must be null: {:?}",
            lines[1]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
