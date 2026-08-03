//! The run itself: own a container, spawn the child into it, route the child's
//! standard I/O, and race its exit against every runner-imposed ending.
//!
//! This is the module the parent's first four settled decisions live in —
//! [`run_async`] owns the [`ProcessGroup`], selects the I/O path (pipe + echo,
//! `--inherit-stdio`, `--capture-dir`, `--no-echo`, and the `--idle-timeout`
//! re-arming tee), stands up the control plane, hands a POSIX terminal to an
//! interactive child when required, and dispatches whichever arm of the race won
//! to the shared teardown tail in [`super::teardown`]. The deadlines and cancel
//! triggers it races against live in [`super::signals`]; the `--detach` wrapper
//! that never enters this path at all lives in [`super::detach`].

use std::convert::Infallible;
use std::time::{Duration, SystemTime};

use processkit::{
    Command as PkCommand, LimitKind, Mechanism, Outcome, OutputBufferPolicy, ProcessGroup,
    ProcessGroupOptions, RunningProcess, Stdin, StdioMode,
};

use crate::capture::{
    CAPTURE_INFLIGHT_MAX_BYTES, CAPTURE_MAX_BYTES, Capture, CaptureOverflow, IdleClock,
};
use crate::cli::{CaptureOverflowPolicy, RunArgs, parse_env_kv};
use crate::control::{self, SnapshotSource};
use crate::events::{self, Emitter, Event, Member};
use crate::exit::{self, RunnerError};
use crate::registry;

use super::signals::{deadline, effective_grace_for, idle_deadline, wait_for_cancel_signal};
use super::teardown::{
    clear_registration, control_kill_error, duration_ms, emit_cleanup_finished,
    emit_cleanup_started, emit_hard_teardown, emit_members_snapshot, emit_output_captured,
    exit_code_for, finish, finish_foreground_failure, graceful_teardown, launch_failure_event,
    launch_failure_source, map_launch_error, termination_error,
};
use super::{Ending, SnapshotReason, Termination, TimeoutTrigger};

/// Own a group, spawn the child into it, stream its output live, write the JSONL
/// lifecycle events, and report how the run ended. The group drops when this
/// future completes — on success or on any error path — which is what tears the
/// container down.
///
/// **Event invariant.** Every return path emits exactly one terminal
/// [`Event::RunnerExit`] as its last event, so a child's code is recorded out of
/// band even on the runner's own failure (`AGENTS.md`, "Exit-code fidelity").
pub(super) async fn run_async(args: RunArgs) -> Result<i32, RunnerError> {
    // clap guarantees at least one token (`num_args = 1..`, `required = true`).
    let (program, program_args) = args
        .command
        .split_first()
        .expect("clap enforces a non-empty command after `--`");

    // Resolve artifact locations once, before any later `--cwd` changes the child
    // context. The registry and inspect snapshot publish absolute paths so another
    // process can discover a detached run's observability files without inheriting
    // the launcher's working directory.
    let artifact_base = std::env::current_dir().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not resolve the runner's current directory: {err}"),
        )
    })?;
    let jsonl_path = absolute_from(&artifact_base, &args.jsonl);
    let capture_dir = args
        .capture_dir
        .as_deref()
        .map(|path| absolute_from(&artifact_base, path));
    let jsonl_locator = jsonl_path.to_string_lossy().into_owned();
    let capture_locator = capture_dir
        .as_deref()
        .map(|path| path.to_string_lossy().into_owned());

    // Open the event stream *first*, before anything is spawned. `--jsonl` is a
    // required, first-class output, so a file we cannot even create is a
    // fail-closed setup error reported before the child runs — no child code can
    // be lost to a logging failure. Once open, later write failures are
    // best-effort (see `Emitter`), never a reason to abort a healthy run.
    let mut emitter = Emitter::create(&args.jsonl).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
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
    // extra event, no capture files). `--capture-max-bytes` (T-181) only resolves
    // here, applied as both streams' per-stream ceiling — omitted, it falls back to
    // `CAPTURE_MAX_BYTES` (the prior hard-coded 8 MiB), so a bare
    // `--capture-dir` without `--capture-max-bytes` is byte-for-byte unchanged too.
    let capture_max_bytes = args.capture_max_bytes.unwrap_or(CAPTURE_MAX_BYTES);
    let capture = match args.capture_dir.as_deref() {
        Some(dir) => match Capture::create(dir, capture_max_bytes) {
            Ok(capture) => Some(capture),
            Err(err) => {
                let error = RunnerError::new(
                    exit::SETUP,
                    format!(
                        "could not set up output capture in `{}`: {err}",
                        dir.display()
                    ),
                );
                return Err(finish(&mut emitter, "setup", None, error));
            }
        },
        None => None,
    };

    // `Stdin::from_file` opens and streams the file through ProcessKit's own pump.
    // Open it once here too, before a child exists, so an ordinary bad path or
    // permission error is a fail-closed SETUP result rather than a child that ran
    // with accidentally absent input. The core opens it again when it starts the
    // writer; a later filesystem race still surfaces as its truthful stdin error.
    if let Some(path) = args.stdin_file.as_deref()
        && let Err(err) = std::fs::File::open(path)
    {
        let error = RunnerError::new(
            exit::SETUP,
            format!("could not open stdin file `{}`: {err}", path.display()),
        );
        return Err(finish(&mut emitter, "setup", None, error));
    }

    // Environment files are operator-provided setup inputs, so read and validate
    // all of them before a child exists. Their contents never enter argv, events,
    // or the registry; only the resulting builder calls below see the values.
    let file_env = match load_env_files(&args.env_file) {
        Ok(entries) => entries,
        Err(error) => return Err(finish(&mut emitter, "setup", None, error)),
    };

    // We own this group; the child — and anything it spawns — is a member. When
    // `group` drops at the end of this scope (every path below), the kernel reaps
    // the whole tree. Containment/teardown is the group's job; we never duplicate
    // it (`AGENTS.md`, "Never clean up by process name"). `create_group` uses the
    // plain `ProcessGroup::new()` unless a `--max-*`/`--cpu-quota` flag asked for a
    // whole-tree resource cap, in which case it goes through
    // `ProcessGroup::with_options` (see that function's decision note).
    let group = match create_group(&args) {
        Ok(group) => group,
        Err(err) => {
            // A requested resource cap the active mechanism cannot honor surfaces as
            // `ErrorReason::ResourceLimit`, *pre-spawn* (no child ran). Emit the
            // resource-specific `limit_hit` first — the dedicated, machine-readable
            // signal that this creation failure was a limit, not a generic backend
            // fault — then take the *same* `container_failed{create}` →
            // `container_error`/`BACKEND` (102) tail every other group-creation
            // failure uses (the exit-code reuse is a deliberate decision — see
            // `create_group`). `limit_kind` reads the kind off the error without
            // destructuring its `#[non_exhaustive]` variant, and is `None` for every
            // non-limit error, so this branch fires only for a real limit failure.
            let message = if let Some(kind) = err.limit_kind() {
                emitter.emit(&Event::LimitHit {
                    limit: limit_kind_str(kind).to_string(),
                    detail: Some(err.to_string()),
                });
                format!("could not apply the requested resource limit: {err}")
            } else {
                format!("could not create the ProcessKit container: {err}")
            };
            let error = RunnerError::new(exit::BACKEND, message);
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
    let labels = crate::labels::to_map(&args.labels);
    let registry_handle = open_registry();
    // `open_server` no longer takes the registry directory (it never gated the
    // transport's bind location on either platform, see `control::open_server`'s
    // docstring) — but the transport is still gated on the registry being present:
    // without a registry there is nowhere to publish the endpoint, so a client could
    // never discover this run's control transport even if it were bound. Skipping the
    // bind in that case is a deliberate choice, not leftover coupling to `dir`.
    let control_server = registry_handle
        .as_ref()
        .and_then(|_| control::open_server());
    let endpoint = control_server
        .as_ref()
        .map(|server| server.endpoint().to_string());
    let registration = registry_handle.as_ref().and_then(|registry| {
        register_run(
            registry,
            &run_id,
            endpoint.as_deref(),
            started,
            // The redaction-safe identification of this run's command — a one-way
            // argv fingerprint plus the worker-shape hint — from the same argv and
            // the same code path the `run_started` event's `command` object uses, so
            // the registry record and the JSONL stream can never disagree about which
            // run is which. Raw argv is not part of it and never reaches the registry
            // (see `register_run`). Derived here, where it is actually published, so
            // a run whose registry could not be opened at all computes nothing.
            &events::CommandFingerprint::for_argv(&args.command),
            &labels,
            registry::ArtifactLocators {
                jsonl: Some(&jsonl_locator),
                capture_dir: capture_locator.as_deref(),
            },
        )
    });

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
    // Environment configuration maps onto ProcessKit's builder in the documented
    // order: clear, remove, file entries, explicit sets. Later calls win for a
    // duplicate key, so `--env` always overrides every file without exposing a
    // file's secret values in the runner's command line.
    if args.env_clear {
        command = command.env_clear();
    }
    for key in &args.env_remove {
        command = command.env_remove(key);
    }
    for (key, value) in &file_env {
        command = command.env(key, value);
    }
    for (key, value) in &args.env {
        command = command.env(key, value);
    }
    // `--create-no-window` maps straight onto `Command::create_no_window()`
    // (`CREATE_NO_WINDOW` on Windows, a no-op elsewhere). Default: OFF. A bare
    // `run` should behave like launching the child directly, so we do not force
    // the flag — that would diverge from a direct launch and could hide a child
    // that legitimately wants its own console. Headless Windows deployments
    // that must avoid a stray `conhost` window pass the flag
    // explicitly; the runner itself never allocates a console, so it spawns no
    // extra host on its own account. (See README, "Windows console".)
    if args.create_no_window {
        command = command.create_no_window();
    }
    // Opt-in console children can receive ProcessKit's Windows `CTRL_BREAK` soft
    // stop before Job Object escalation. The builder is a documented no-op off
    // Windows; clap has already rejected the two consoleless Windows modes.
    if args.windows_graceful_ctrl_break {
        command = command.windows_graceful_ctrl_break();
    }
    // Stdin defaults to ProcessKit's closed/null mode. The inheritance opt-ins
    // share this runner's real stdin, while a file is streamed through the core's
    // managed pipe and closed at EOF. Clap rejects every contradictory pair before
    // this point, matching the core's own guard.
    if args.inherit_stdio || args.inherit_stdin {
        command = command.inherit_stdin();
    } else if let Some(path) = args.stdin_file.as_deref() {
        command = command.stdin(Stdin::from_file(path));
    }
    // The shared idle-timeout clock. Every chunk of the child's output the runner
    // sees re-arms it (below), and the idle-deadline arm of the race reads it. There
    // is exactly one, so both output paths (default echo and the `--capture-dir` tee)
    // re-arm the *same* timer. It is only wired in — and only ever touched — when
    // `--idle-timeout` is set, so a run without the flag is byte-for-byte unchanged
    // (no wrapper on the sinks, no clock reads); the clone is otherwise inert.
    let idle_clock = IdleClock::new();
    let idle_timeout = args.idle_timeout;

    // `--inherit-stdio` gives the child the runner's actual output handles. No
    // pump, decoding, or tee sits between a terminal and the child, so terminal
    // status and full-screen behavior are preserved. Capture, `--no-echo` — there
    // is no echo to suppress under inheritance — and `--idle-timeout`, which has no
    // output pump to observe under inheritance, all conflict with it at parse time.
    //
    // Otherwise pipe + echo remains the compatibility default. Live echo uses
    // ProcessKit's raw tee: one awaited write per pipe-read chunk instead of two
    // writes plus a mutex round-trip for every decoded line. Besides removing the
    // measured per-line cost, this is the public API that preserves the settled
    // byte-for-byte passthrough contract (invalid UTF-8, CRLF, and unterminated
    // tails included). `--no-echo` simply omits that raw sink unless an idle clock
    // still needs to observe the stream.
    //
    // Capture deliberately remains on the decoded tee. Its counters, hashes, and
    // truncation boundary therefore stay byte-compatible with the established
    // capture contract; it is a second observation sink and never feeds live echo.
    // The idle clock belongs on the raw path so an unterminated prompt re-arms the
    // timeout immediately rather than waiting for a line boundary.
    command = if args.inherit_stdio {
        command
            .stdout(StdioMode::Inherit)
            .stderr(StdioMode::Inherit)
    } else if let Some(capture) = &capture {
        let command = command.output_buffer(
            OutputBufferPolicy::bounded(0).with_max_bytes(CAPTURE_INFLIGHT_MAX_BYTES),
        );
        if idle_timeout.is_some() {
            if args.no_echo {
                command
                    .stdout_tee(capture.stdout_tee(tokio::io::sink()))
                    .stderr_tee(capture.stderr_tee(tokio::io::sink()))
                    .stdout_raw_tee(idle_clock.tee(tokio::io::sink()))
                    .stderr_raw_tee(idle_clock.tee(tokio::io::sink()))
            } else {
                command
                    .stdout_tee(capture.stdout_tee(tokio::io::sink()))
                    .stderr_tee(capture.stderr_tee(tokio::io::sink()))
                    .stdout_raw_tee(idle_clock.tee(tokio::io::stdout()))
                    .stderr_raw_tee(idle_clock.tee(tokio::io::stderr()))
            }
        } else if args.no_echo {
            command
                .stdout_tee(capture.stdout_tee(tokio::io::sink()))
                .stderr_tee(capture.stderr_tee(tokio::io::sink()))
        } else {
            command
                .stdout_tee(capture.stdout_tee(tokio::io::sink()))
                .stderr_tee(capture.stderr_tee(tokio::io::sink()))
                .stdout_raw_tee(tokio::io::stdout())
                .stderr_raw_tee(tokio::io::stderr())
        }
    } else if idle_timeout.is_some() {
        if args.no_echo {
            command
                .stdout_raw_tee(idle_clock.tee(tokio::io::sink()))
                .stderr_raw_tee(idle_clock.tee(tokio::io::sink()))
        } else {
            command
                .stdout_raw_tee(idle_clock.tee(tokio::io::stdout()))
                .stderr_raw_tee(idle_clock.tee(tokio::io::stderr()))
        }
    } else if args.no_echo {
        command
    } else {
        command
            .stdout_raw_tee(tokio::io::stdout())
            .stderr_raw_tee(tokio::io::stderr())
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
    let group_mechanism = group.mechanism();
    let terminal_foreground =
        match TerminalForegroundGuard::acquire(args.inherit_stdio, group_mechanism, root_pid) {
            Ok(guard) => guard,
            Err(err) => {
                // The interactive terminal-foreground handoff failed. The container
                // was already created and the child already spawned (we hold
                // `running`), so this is a `foreground`-phase container failure —
                // reported to the JSONL stream before the shared teardown tail, not
                // left only on stderr (see `finish_foreground_failure` for the
                // phase-choice rationale).
                let error = RunnerError::new(
                    exit::BACKEND,
                    format!("could not give the interactive child terminal control: {err}"),
                );
                return Err(finish_foreground_failure(
                    &mut emitter,
                    &group,
                    &capture,
                    &registration,
                    error,
                    err.to_string(),
                ));
            }
        };
    // The POSIX process-group backend must place the child in its own group for
    // containment. If it tried reading the inherited terminal in the tiny window
    // before the foreground handoff, the kernel may have stopped it with SIGTTIN;
    // resume through ProcessKit after the handoff so interactive input is usable.
    if terminal_foreground.is_active()
        && let Err(err) = group.resume()
    {
        // The other half of the interactive terminal handoff — resuming the child
        // that a stray SIGTTIN may have stopped in the tiny pre-handoff window —
        // failed. Same `foreground`-phase container failure as the sibling acquire
        // path above (the child is spawned but `run_started` is not yet written),
        // reported to the stream before the teardown tail.
        let error = RunnerError::new(
            exit::BACKEND,
            format!("could not resume the interactive child process group: {err}"),
        );
        return Err(finish_foreground_failure(
            &mut emitter,
            &group,
            &capture,
            &registration,
            error,
            err.to_string(),
        ));
    }

    let mechanism = events::mechanism_str(group_mechanism);
    emitter.emit(&Event::RunStarted {
        run_id: run_id.clone(),
        root_pid,
        mechanism,
        abrupt_cleanup: events::abrupt_cleanup_str(),
        cwd: resolve_cwd(&args),
        command: events::CommandInfo::for_argv(&args.command, args.argv_raw),
        labels: labels.clone(),
    });
    emit_members_snapshot(&mut emitter, &group, SnapshotReason::Spawn);

    // What the control server answers an `inspect` with. `members` is a live query of
    // the owning container, so a snapshot reflects the tree's composition *when
    // inspected* — the same enriched view the `members_snapshot` event carries
    // (both read through `members_info()`, so `inspect` and the JSONL stream never
    // drift on what a "container member" looks like).
    let members_provider = || {
        group
            .members_info()
            .map(|infos| infos.into_iter().map(Member::from_info).collect())
            .unwrap_or_default()
    };
    let snapshot_source = SnapshotSource::new(
        &run_id,
        mechanism,
        root_pid,
        started,
        &jsonl_locator,
        capture_locator.as_deref(),
        &members_provider,
    );

    // The channel the control server signals a mutating `cancel`/`kill` verb through.
    // The server (in the `select!` below) writes its client ack first, then sends the
    // command here; this loop's `recv` arm then wins the race and drives teardown. The
    // sender lives for the whole `select!`, so `recv` only yields `None` at teardown.
    let (command_tx, mut command_rx) =
        tokio::sync::mpsc::unbounded_channel::<control::ControlCommand>();

    let timeout = args.timeout;
    let grace = args.grace;

    // Arm the idle window from *here* — the moment the race begins, right after the
    // child is spawned and `run_started` is written — rather than from whenever the
    // clock was constructed, so a child's first idle window is measured from the run
    // starting, not from the runner's own pre-spawn setup. A no-op read/write when
    // `--idle-timeout` is unset (the clock is never consulted then).
    idle_clock.reset();

    // Race the child's own exit against the runner-imposed endings. Whichever fires
    // first *decides* the outcome; only then does teardown begin, so the owning group
    // is never dropped before the outcome is known.
    //
    // `biased` order — local stop signal, natural exit, control command, overall
    // deadline, idle deadline, then the control server — makes the tie-breaks
    // deliberate: a cancel signal (`Ctrl-C`, Unix `SIGTERM`/`SIGHUP`, or Windows
    // `Ctrl-Break`/console close/logoff/system shutdown) always wins, and a child
    // that exits in the very poll a deadline or a control command fires is reported
    // as its own exit rather than a runner-imposed ending
    // (natural exit is polled before all of them). The new `--idle-timeout` arm sits
    // right after the overall `--timeout` arm: both are runner-imposed deadlines, so
    // they share the same low tie-break priority (behind the child's own exit and a
    // cancel), and between the two the overall deadline is polled first — an
    // arbitrary but fixed order, since a run can only cross one deadline "first" in
    // wall-clock terms anyway. When a cancel/kill/deadline branch wins, `running`
    // (moved into `wait()`) is dropped; because this is a shared-group handle that
    // does not kill on drop, the child stays alive for the teardown path below, and
    // its output pumps stop (teardown is underway). The `command_rx` branch resolves
    // when the control server routes a `cancel`/`kill` verb (having already acked the
    // client); the control-server branch itself **never resolves** (its output is
    // `Infallible`) — it serves clients concurrently with the output pump, so it
    // neither delays the child's exit nor blocks teardown, and is dropped (tearing the
    // transport down) when another branch wins. The `--snapshot-interval` cadence is
    // the second such never-resolving branch, and is deliberately polled **last**: it
    // is the one branch that acts on its own timer rather than only when a client
    // shows up, so giving every *deciding* branch the earlier tie-break is what keeps
    // a periodic snapshot from being appended in the very poll a child exits or a
    // deadline fires (see [`snapshot_cadence`]).
    let capturing = capture.is_some();
    let cancel_on_capture_overflow = args.capture_overflow == Some(CaptureOverflowPolicy::Cancel);
    let snapshot_interval = args.snapshot_interval;
    let ending = tokio::select! {
        biased;
        signal = wait_for_cancel_signal() => Ending::Cancelled(signal),
        outcome = drive_to_outcome(running, capturing) => Ending::Exited(outcome),
        command = command_rx.recv() => match command {
            Some(control::ControlCommand::Cancel) => Ending::ControlCancelled,
            Some(control::ControlCommand::Kill) => Ending::ControlKilled,
            // The sender lives as long as the serve future in this same `select!`, so
            // a closed channel cannot happen while this arm is racing; park if it ever
            // did rather than misreport an ending.
            None => std::future::pending().await,
        },
        () = deadline(timeout) => Ending::TimedOut(TimeoutTrigger::Overall),
        () = idle_deadline(idle_timeout, &idle_clock) => Ending::TimedOut(TimeoutTrigger::Idle),
        overflow = capture_overflow(&capture, cancel_on_capture_overflow) => Ending::OutputOverflow(overflow),
        never = control::serve(control_server, &snapshot_source, &command_tx) => match never {},
        never = snapshot_cadence(snapshot_interval, &mut emitter, &group) => match never {},
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
                Err(error) => {
                    // The outcome decoded, but it is not one `exit_code_for` can turn
                    // into a child exit code (an untimed `Outcome::TimedOut`, or an
                    // unrecognized `#[non_exhaustive]` variant). The container was
                    // still spawned and may still hold live members, so this is a
                    // decided ending like any other and must run the same teardown
                    // tail as every other branch rather than returning through the
                    // bare `finish` a setup-time failure uses. Same shared hard-kill
                    // path as the sibling wait-failure arm above (T-157).
                    emit_hard_teardown(&mut emitter, &group, &capture, &registration);
                    return Err(finish(&mut emitter, "internal", None, error));
                }
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
        Ending::TimedOut(trigger) => {
            // Both `--timeout` (overall) and `--idle-timeout` (idle) resolve here and
            // share everything but the `reason`: the reserved `TIMEOUT` (106) code,
            // the `timeout` runner-exit `source`, and the soft-stop → grace →
            // hard-kill teardown. `limit` is the deadline that actually elapsed — the
            // whole-run window for `overall`, the idle window for `idle` — so the
            // event's `timeout_ms` and the stderr message both echo the right duration.
            let limit = match trigger {
                TimeoutTrigger::Overall => {
                    timeout.expect("the overall-deadline arm only fires when --timeout is set")
                }
                TimeoutTrigger::Idle => idle_timeout
                    .expect("the idle-deadline arm only fires when --idle-timeout is set"),
            };
            emitter.emit(&Event::Timeout {
                timeout_ms: duration_ms(limit),
                grace_ms: grace.map(duration_ms),
                reason: trigger.reason(),
            });
            // `cleanup_started` brackets the whole teardown — soft stop, grace, and
            // hard kill — so `members_before` is the full tree, not a post-soft remnant.
            emit_cleanup_started(&mut emitter, &group);
            let teardown = graceful_teardown(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(&teardown));
            // A forced ending still reports whatever was captured before teardown.
            emit_output_captured(&mut emitter, &capture);
            // The registry entry is removed on every decided ending, not just the
            // happy path: a timeout tears the run down cleanly too.
            clear_registration(&registration);
            let error =
                termination_error(Termination::Timeout { limit, trigger }, &teardown, grace);
            Err(finish(&mut emitter, "timeout", None, error))
        }
        Ending::OutputOverflow(overflow) => {
            emitter.emit(&Event::OutputOverflow {
                stream: overflow.stream,
                max_bytes: overflow.max_bytes,
                grace_ms: grace.map(duration_ms),
            });
            emit_cleanup_started(&mut emitter, &group);
            let teardown = graceful_teardown(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(&teardown));
            emit_output_captured(&mut emitter, &capture);
            clear_registration(&registration);
            let error = termination_error(Termination::OutputOverflow(overflow), &teardown, grace);
            Err(finish(&mut emitter, "output_overflow", None, error))
        }
        Ending::Cancelled(signal) => {
            // Every local stop signal — `Ctrl-C`, on Unix `SIGTERM`/`SIGHUP`, and on
            // Windows `Ctrl-Break`/console-close/logoff/shutdown — resolves here and
            // shares everything but the `source`: the reserved `CANCELLED` (107) code,
            // the `cancelled` runner-exit source, and the soft-stop → grace →
            // hard-kill teardown. Which signal it was is recorded honestly rather
            // than flattened onto `ctrl_c` (see `CancelSignal`). `grace` is resolved
            // through `effective_grace_for` — identical to the requested `--grace`
            // for every trigger except Windows `CtrlClose`, whose OS-imposed
            // termination deadline it must fit within (see that function and
            // `CTRL_CLOSE_WINDOW`) — and the *effective* value is what gets reported,
            // waited, and echoed, so the JSONL stream and the stderr line never claim
            // a wait that could not actually happen.
            let grace = effective_grace_for(signal, grace);
            emitter.emit(&Event::Cancelled {
                source: signal.source(),
                grace_ms: grace.map(duration_ms),
            });
            emit_cleanup_started(&mut emitter, &group);
            let teardown = graceful_teardown(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(&teardown));
            // A forced ending still reports whatever was captured before teardown.
            emit_output_captured(&mut emitter, &capture);
            // A signal cancel tears the run down cleanly too — its entry goes with it.
            clear_registration(&registration);
            let error = termination_error(Termination::Cancelled(signal), &teardown, grace);
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
            let teardown = graceful_teardown(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(&teardown));
            emit_output_captured(&mut emitter, &capture);
            clear_registration(&registration);
            let error = termination_error(Termination::ControlCancelled, &teardown, grace);
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

/// Park forever unless capture overflow is configured as an ending trigger.
async fn capture_overflow(capture: &Option<Capture>, cancel: bool) -> CaptureOverflow {
    match (capture.as_ref(), cancel) {
        (Some(capture), true) => capture.overflowed().await,
        _ => std::future::pending().await,
    }
}

/// The opt-in periodic `members_snapshot` cadence (`run --snapshot-interval`): sleep
/// one interval, re-emit the snapshot through the shared
/// [`emit_members_snapshot`]/`members_info()` path, repeat for as long as the run
/// races. With no `--snapshot-interval` it parks forever, so a run without the flag
/// never reads the clock, never touches the container, and **adds no event**: the
/// number, type, and position of the events such a run emits are exactly what they
/// were before this arm existed, which is what
/// `a_run_without_snapshot_interval_still_emits_exactly_one_members_snapshot`
/// actually proves (an event-type sequence comparison plus a snapshot count, not a
/// byte comparison). The *wire form* of `members_snapshot` did change for every run,
/// flag or no flag — it gained the always-present `reason` and `read_error` fields
/// (additive within schema v1; see [`SnapshotReason`] and `docs/schema.md`,
/// "Versioning") — so this arm's inertness is a claim about the event stream's
/// shape, never about its bytes.
///
/// **No thread, no task: it is one more arm of the race that already exists.** The
/// runner deliberately runs on a small current-thread runtime ([`super::run_inner`]),
/// and this timer is coalesced straight into [`run_async`]'s `select!` rather than
/// given a `tokio::spawn` or an OS thread of its own. Three properties fall out of
/// that, and all three are the point:
///
/// - **No snapshot can escape into the teardown tail.** The moment any other branch
///   wins, this future is dropped mid-sleep, so a periodic snapshot is structurally
///   incapable of landing after `root_exited`/`timeout`/`cancelled`/`killed` — the
///   ordering invariant `docs/schema.md` now states is enforced by the shape of the
///   code, not by a check that could be forgotten.
/// - **No shared state, no second emitter.** The `&mut Emitter` is borrowed by this
///   branch alone for the life of the race (no other branch writes events), so the
///   JSONL stream keeps its single writer and needs no lock.
/// - **It is not a busy poll.** Each iteration sleeps the *whole* interval, so a run
///   with a 10-minute cadence wakes six times an hour — the same
///   sleep-the-remaining-window discipline [`idle_deadline`] uses, rather than a
///   tight loop re-checking a clock. (The older `wait_grace_or_empty` bounded-poll
///   helper this would otherwise have mirrored no longer exists in this crate: since
///   the ProcessKit 3 migration the grace wait is the library's own
///   `ProcessGroup::stop` driver, and `graceful_teardown` only projects its report.)
///
/// A `members_info()` read that fails still emits its sample, flagged
/// `read_error: true` with an empty `members` list — the honest degradation
/// [`emit_members_snapshot`] applies identically to the post-spawn snapshot — and
/// the cadence keeps going, so one bad read costs one interval's member detail, not
/// the feature. The flag, not the accompanying stderr warning, is the contract: a
/// detached runner's stderr is `Stdio::null()`, so in the cadence's own headline
/// scenario the stream is the only place a failure can be reported (see
/// [`SnapshotReason`]).
///
/// **The stream this arms is unbounded by design**: one line per interval per run,
/// so a `--jsonl` file grows as `duration / interval`, with each line sized by the
/// tree. Nothing here caps, rotates, or samples it — the operator's chosen interval
/// is the only bound, and the sizing arithmetic for picking one is written down in
/// `docs/running-commands.md`, "Recorded tree snapshots".
///
/// Nothing here touches the output pump, the capture tee, or the echo sinks — a
/// snapshot is a query of the container's own membership — which is exactly why the
/// flag composes with `--inherit-stdio`, where no pump exists to observe (unlike
/// `--idle-timeout`, which conflicts with it for precisely that reason).
async fn snapshot_cadence(
    interval: Option<Duration>,
    emitter: &mut Emitter,
    group: &ProcessGroup,
) -> Infallible {
    let Some(interval) = interval else {
        // No cadence armed: park forever so this arm stays inert for every run that
        // did not ask for it.
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(interval).await;
        emit_members_snapshot(emitter, group, SnapshotReason::Interval);
    }
}

/// Restores the caller's foreground terminal group after an interactive child
/// exits. Only the POSIX process-group containment fallback needs a handoff:
/// Windows console handles have no `tcsetpgrp`, and Linux cgroups leave the child
/// in the runner's foreground process group.
struct TerminalForegroundGuard {
    #[cfg(unix)]
    state: Option<TerminalForegroundState>,
}

#[cfg(unix)]
struct TerminalForegroundState {
    fd: libc::c_int,
    original_pgrp: libc::pid_t,
    sigttou: ScopedSignalIgnore,
}

/// Keep one job-control signal ignored until the foreground terminal belongs to
/// this runner again. A thread-local mask only protects the `tcsetpgrp` call;
/// the runner remains a background process for the entire child lifetime and
/// must not be stopped if any runner diagnostic reaches a `TOSTOP` terminal.
#[cfg(unix)]
struct ScopedSignalIgnore {
    signal: libc::c_int,
    previous: Option<libc::sigaction>,
}

#[cfg(unix)]
impl ScopedSignalIgnore {
    fn acquire(signal: libc::c_int) -> std::io::Result<Self> {
        // SAFETY: sigaction is a plain C value whose mask is initialized below.
        let mut ignored: libc::sigaction = unsafe { std::mem::zeroed() };
        ignored.sa_sigaction = libc::SIG_IGN;
        // SAFETY: `sa_mask` is a valid writable signal set.
        if unsafe { libc::sigemptyset(&mut ignored.sa_mask) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: sigaction initializes `previous` on success; both pointers stay
        // valid for the duration of the call.
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(signal, &ignored, &mut previous) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            signal,
            previous: Some(previous),
        })
    }

    fn restore(mut self) -> std::io::Result<()> {
        self.restore_inner()
    }

    fn restore_inner(&mut self) -> std::io::Result<()> {
        let Some(previous) = self.previous.take() else {
            return Ok(());
        };
        // SAFETY: `previous` was returned by sigaction for this same signal.
        if unsafe { libc::sigaction(self.signal, &previous, std::ptr::null_mut()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ScopedSignalIgnore {
    fn drop(&mut self) {
        if let Err(err) = self.restore_inner() {
            eprintln!("processkit-cli: warning: could not restore the SIGTTOU disposition: {err}");
        }
    }
}

impl TerminalForegroundGuard {
    fn acquire(
        enabled: bool,
        mechanism: Mechanism,
        child_pid: Option<u32>,
    ) -> std::io::Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (enabled, mechanism, child_pid);
            Ok(Self {})
        }
        #[cfg(unix)]
        {
            if !enabled || mechanism != Mechanism::ProcessGroup {
                return Ok(Self { state: None });
            }
            let fd = libc::STDIN_FILENO;
            // No terminal means there is no foreground job-control state to move;
            // direct inheritance of a file or pipe remains correct as-is.
            if unsafe { libc::isatty(fd) } != 1 {
                return Ok(Self { state: None });
            }
            // SAFETY: `fd` is the process stdin descriptor and was just confirmed
            // to refer to a terminal.
            let original_pgrp = unsafe { libc::tcgetpgrp(fd) };
            if original_pgrp < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // A background invocation must remain a background job, just like a
            // direct child started by an interactive shell. Only transfer control
            // when this runner's group currently owns the terminal.
            // SAFETY: getpgrp takes no pointers and cannot fail.
            if original_pgrp != unsafe { libc::getpgrp() } {
                return Ok(Self { state: None });
            }
            let child_pid = child_pid.ok_or_else(|| {
                std::io::Error::other(
                    "ProcessKit did not expose the child PID required for terminal control",
                )
            })?;
            let child_pgrp = child_pid as libc::pid_t;
            // Interactive shells ignore SIGTTOU while they manipulate foreground
            // groups. Keep it ignored for the full child lifetime, not only around
            // tcsetpgrp: on macOS the runner otherwise remains stoppable while it
            // is the terminal's background group, which can strand the PTY host.
            // The child has already spawned, so it does not inherit this temporary
            // disposition.
            let sigttou = ScopedSignalIgnore::acquire(libc::SIGTTOU)?;
            if let Err(err) = set_terminal_foreground(fd, child_pgrp) {
                // A very short command can exit between ProcessKit returning its
                // PID and this handoff. Preserve its real result instead of
                // replacing it with a terminal-setup failure.
                if process_group_exists(child_pgrp) {
                    return Err(err);
                }
                return Ok(Self { state: None });
            }
            Ok(Self {
                state: Some(TerminalForegroundState {
                    fd,
                    original_pgrp,
                    sigttou,
                }),
            })
        }
    }

    fn is_active(&self) -> bool {
        #[cfg(not(unix))]
        {
            false
        }
        #[cfg(unix)]
        {
            self.state.is_some()
        }
    }
}

#[cfg(unix)]
impl Drop for TerminalForegroundGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            if let Err(err) = set_terminal_foreground(state.fd, state.original_pgrp) {
                eprintln!(
                    "processkit-cli: warning: could not restore terminal foreground control: {err}"
                );
            }
            // Restore the terminal first: until this call the runner is still a
            // background process and must remain immune to SIGTTOU.
            if let Err(err) = state.sigttou.restore() {
                eprintln!(
                    "processkit-cli: warning: could not restore the SIGTTOU disposition: {err}"
                );
            }
        }
    }
}

#[cfg(not(unix))]
impl Drop for TerminalForegroundGuard {
    fn drop(&mut self) {}
}

/// Probe a positive process-group id without signalling it. `EPERM` still means
/// the group exists; every other error means it disappeared or was invalid.
#[cfg(unix)]
fn process_group_exists(pgrp: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs permission/existence checks only. `pgrp` comes
    // from an OS-assigned child PID and is positive.
    if unsafe { libc::kill(-pgrp, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Set the terminal foreground group while [`ScopedSignalIgnore`] owns an
/// ignored SIGTTOU disposition. Restoring control necessarily happens while the
/// runner is a background group, so callers must keep that guard alive.
#[cfg(unix)]
fn set_terminal_foreground(fd: libc::c_int, pgrp: libc::pid_t) -> std::io::Result<()> {
    // SAFETY: `fd` names the controlling terminal checked by the caller and
    // `pgrp` is either the contained child's group or the saved original group.
    if unsafe { libc::tcsetpgrp(fd, pgrp) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Create the owning [`ProcessGroup`], honoring any whole-tree resource cap the
/// operator requested.
///
/// **Byte-for-byte-unchanged default.** With no `--max-memory`/`--max-processes`/
/// `--cpu-quota` flag, [`build_limit_options`] returns `None` and this is exactly
/// the previous unconditional `ProcessGroup::new()` — no `limits`-feature code runs
/// at runtime, so a default run is unaffected. A cap is only ever requested when
/// the operator asked to bound the tree, in which case the group is created through
/// [`ProcessGroup::with_options`] with the caps mapped onto ProcessKit's own
/// [`ProcessGroupOptions`] builder (`AGENTS.md`, "Build strictly on the public
/// `processkit` API": the OS-level enforcement — a Windows Job Object or a Linux
/// cgroup v2 — is the crate's, never reimplemented here).
///
/// **Exit-code decision (reserved runner band `100`–`119`).** When `with_options`
/// cannot apply a requested cap it returns [`processkit::ErrorReason::ResourceLimit`], and it does
/// so **pre-spawn** — the child never started, so no child code is ever at risk.
/// Because the CLI parsers (`src/cli/parse.rs`) already reject every nonsensical value as
/// a `USAGE` (100) form error *before* we reach here, the only reasons that survive
/// to this point are the "could not be applied" ones — `Unsupported` (macOS/BSD and
/// the Linux process-group fallback have no whole-tree container at all) and
/// `Unenforceable` (a Linux cgroup v2 whose controllers can't be enabled — under
/// systemd, an ordinary container, or typical CI). That is the **same class** of
/// failure as the existing `ProcessGroup::new()` container-creation error ("a
/// whole-tree container capable of what was asked could not be established here"),
/// so the caller **deliberately reuses** the existing [`exit::BACKEND`] (102) code
/// and the `container_error` `runner_exit` `source` rather than minting a new code
/// from the reserved band's free slots. The distinguishing signal is the dedicated
/// `limit_hit` event emitted immediately before the shared tail — the authoritative,
/// machine-readable channel (`docs/exit-codes.md`, "Why a band is not enough on its
/// own"; the numeric exit code is only a best-effort hint). No reserved-band slot was
/// spent on it (no additional runner-owned code is minted here).
fn create_group(args: &RunArgs) -> processkit::Result<ProcessGroup> {
    match build_limit_options(args) {
        Some(options) => ProcessGroup::with_options(options),
        None => ProcessGroup::new(),
    }
}

/// Assemble ProcessKit's [`ProcessGroupOptions`] from the run's resource-limit
/// flags, or `None` when the operator set none — the signal [`create_group`] uses
/// to keep the plain, unchanged `ProcessGroup::new()` path. Each flag maps straight
/// onto the matching `ProcessGroupOptions` builder; the values were already
/// validated for form by the CLI parsers (`src/cli/parse.rs` is the single source of
/// truth for that), so nothing is re-validated here.
fn build_limit_options(args: &RunArgs) -> Option<ProcessGroupOptions> {
    if args.max_memory.is_none() && args.max_processes.is_none() && args.cpu_quota.is_none() {
        return None;
    }
    let mut options = ProcessGroupOptions::default();
    if let Some(bytes) = args.max_memory {
        options = options.max_memory(bytes);
    }
    if let Some(n) = args.max_processes {
        options = options.max_processes(n);
    }
    if let Some(cores) = args.cpu_quota {
        options = options.cpu_quota(cores);
    }
    Some(options)
}

/// The stable `limit_hit.limit` schema string for a [`LimitKind`]. `LimitKind` is
/// `#[non_exhaustive]`, so a future kind this build predates maps to `"unknown"`
/// rather than a guess — mirroring [`events::mechanism_str`]'s treatment of an
/// unrecognized `Mechanism`. The three known strings match the golden fixture and
/// `docs/schema.md` (`"memory"` / `"processes"` / `"cpu"`).
fn limit_kind_str(kind: LimitKind) -> &'static str {
    match kind {
        LimitKind::Memory => "memory",
        LimitKind::Processes => "processes",
        LimitKind::Cpu => "cpu",
        _ => "unknown",
    }
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

/// Parse one environment file's bytes with the exact `KEY=VALUE` grammar used
/// by `--env`. This is public only so the fuzz target can drive the real parser
/// with arbitrary bytes; it is not part of the CLI compatibility surface.
///
/// Errors identify the malformed line or encoding without repeating values,
/// because environment files routinely carry secrets.
#[doc(hidden)]
pub fn parse_env_file_contents(contents: &[u8]) -> Result<Vec<(String, String)>, String> {
    let text = std::str::from_utf8(contents)
        .map_err(|_| "environment file must contain valid UTF-8".to_string())?;
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let entry = parse_env_kv(line).map_err(|err| format!("line {}: {err}", index + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Read environment files in argument order before applying their parsed
/// entries to the child command.
fn load_env_files(paths: &[std::path::PathBuf]) -> Result<Vec<(String, String)>, RunnerError> {
    let mut entries = Vec::new();
    for path in paths {
        let contents = std::fs::read(path).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!(
                    "could not read environment file `{}`: {err}",
                    path.display()
                ),
            )
        })?;
        let parsed = parse_env_file_contents(&contents).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("invalid environment file `{}`: {err}", path.display()),
            )
        })?;
        entries.extend(parsed);
    }
    Ok(entries)
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
/// address a client connects to, or `None` when no transport could be stood up), the
/// redaction-safe `command` fingerprint that lets `list` tell this run apart from the
/// operator's other live ones (T-215), and the liveness lock the returned
/// [`registry::Registration`] holds for the run. Best-effort, like [`open_registry`]:
/// a failure warns and yields `None`.
///
/// `command` is an [`events::CommandFingerprint`], never the argv: the registry is
/// handed the one-way fingerprint and the categorical hint only, so this run's
/// command line cannot reach a registry record whether or not `--argv-raw` was
/// passed (that flag widens the JSONL `run_started` event alone — see
/// [`registry::Registry::register`]).
fn register_run(
    registry: &registry::Registry,
    run_id: &str,
    endpoint: Option<&str>,
    started: SystemTime,
    command: &events::CommandFingerprint,
    labels: &std::collections::BTreeMap<String, String>,
    artifacts: registry::ArtifactLocators<'_>,
) -> Option<registry::Registration> {
    match registry
        .register_with_labels_and_artifacts(run_id, endpoint, started, command, labels, artifacts)
    {
        Ok(registration) => Some(registration),
        Err(err) => {
            eprintln!("processkit-cli: warning: could not create the run registry entry: {err}");
            None
        }
    }
}

fn absolute_from(base: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    // `std::path::absolute` also handles Windows drive-relative forms (`C:foo`),
    // whose resolution cannot be reproduced correctly with a plain `base.join`.
    // `base` is the already-resolved current directory and is only a defensive
    // fallback for an environment race after the earlier successful lookup.
    std::path::absolute(path).unwrap_or_else(|_| base.join(path))
}

/// The child's absolute working directory as recorded in `run_started`. This shares
/// artifact locators' absolute-path rule, including Windows drive-relative paths.
/// Rendered lossily to a string, or `None` if the runner directory cannot be read.
fn resolve_cwd(args: &RunArgs) -> Option<String> {
    let base = std::env::current_dir().ok()?;
    let path = args
        .cwd
        .as_deref()
        .map(|path| absolute_from(&base, path))
        .unwrap_or(base);
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::PathBuf;

    fn run_args(argv: &[&str]) -> RunArgs {
        let mut full = vec!["processkit-cli", "run", "--jsonl", "events.jsonl"];
        full.extend_from_slice(argv);
        full.extend_from_slice(&["--", "true"]);
        match crate::cli::Cli::try_parse_from(full)
            .expect("valid run")
            .command
        {
            crate::cli::Command::Run(args) => *args,
            _ => panic!("expected run"),
        }
    }

    /// The `limit_hit.limit` string for each `LimitKind` matches the schema and the
    /// golden fixture (`"memory"`/`"processes"`/`"cpu"`). Cross-platform and
    /// container-free: it proves the mapping the `limit_hit` emission uses without
    /// depending on Job Object / cgroup availability (the enforcement itself is
    /// platform-specific and is exercised, honestly per platform, through the
    /// binary in `tests/run.rs`).
    #[test]
    fn limit_kind_maps_to_the_documented_schema_strings() {
        assert_eq!(limit_kind_str(LimitKind::Memory), "memory");
        assert_eq!(limit_kind_str(LimitKind::Processes), "processes");
        assert_eq!(limit_kind_str(LimitKind::Cpu), "cpu");
    }

    /// A run with no `--max-*`/`--cpu-quota` flag requests no options, so
    /// `create_group` keeps the plain `ProcessGroup::new()` path unchanged; any one
    /// flag flips it to `with_options`.
    #[test]
    fn build_limit_options_is_none_without_flags_and_some_with_any() {
        assert!(
            build_limit_options(&run_args(&[])).is_none(),
            "no limit flag ⇒ the unchanged ProcessGroup::new() path"
        );
        for flag in [
            vec!["--max-memory", "256m"],
            vec!["--max-processes", "8"],
            vec!["--cpu-quota", "1.5"],
        ] {
            assert!(
                build_limit_options(&run_args(&flag)).is_some(),
                "the {flag:?} flag must request ProcessGroupOptions"
            );
        }
    }

    /// A relative `--cwd` is resolved against the runner's own working directory,
    /// just as child spawn resolves it. The relative suffix is retained rather than
    /// lexically normalized so symlink/junction traversal keeps the OS's semantics;
    /// the absolute prefix still makes the value self-contained for consumers.
    #[test]
    fn resolve_cwd_makes_a_relative_flag_absolute() {
        let runner_cwd = std::env::current_dir().expect("resolve the test runner cwd");
        let expected = absolute_from(
            &runner_cwd,
            std::path::Path::new("../processkit-cli-cwd-target"),
        );
        let resolved = resolve_cwd(&run_args(&["--cwd", "../processkit-cli-cwd-target"]))
            .expect("resolve the relative cwd");

        let resolved = PathBuf::from(resolved);
        assert!(resolved.is_absolute());
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_cwd_without_a_flag_reports_the_absolute_runner_cwd() {
        let resolved = resolve_cwd(&run_args(&[])).expect("resolve inherited cwd");
        assert_eq!(
            PathBuf::from(resolved),
            std::env::current_dir().expect("resolve the test runner cwd")
        );
    }

    #[test]
    fn env_file_content_parser_accepts_comments_blanks_and_first_equals_split() {
        assert_eq!(
            parse_env_file_contents(b"# settings\r\n\r\nTOKEN=left=right\nEMPTY=\n").unwrap(),
            vec![
                ("TOKEN".to_string(), "left=right".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn env_file_content_errors_reject_non_utf8_and_never_repeat_values() {
        assert_eq!(
            parse_env_file_contents(&[0xff]).unwrap_err(),
            "environment file must contain valid UTF-8"
        );

        let error = parse_env_file_contents(b"GOOD=ok\nBAD KEY=SUPER_SECRET_VALUE\n")
            .expect_err("whitespace in a key must fail closed");
        assert!(error.contains("line 2"));
        assert!(!error.contains("SUPER_SECRET_VALUE"));
    }
}
