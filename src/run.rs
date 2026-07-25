//! The `run` subcommand: launch one shell-free program inside a ProcessKit
//! container, route its output live, forward its exit code faithfully, and bound
//! the run with a hard `--timeout` and a local stop-signal cancel (`Ctrl-C`, on
//! Unix `SIGTERM`/`SIGHUP`, and on Windows `Ctrl-Break`/console close/logoff/
//! system shutdown).
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
//! - **Output is pipe + echo by default, direct inheritance by opt-in.** The
//!   default path uses processkit's line pump and therefore exposes no TTY to the
//!   child. `--inherit-stdio` instead maps all three streams onto ProcessKit's
//!   public inheritance modes, preserving the caller's terminal handles without
//!   a runner-side pump. Streams stay strictly separated either way, and no runner
//!   diagnostic is ever written to the child's stdout.
//! - **Exit-code fidelity, with distinguishable runner-imposed endings.** On a
//!   completed run the process exits with the child's *exact* code (full width,
//!   never clamped). When the runner instead *ends* the run — the `--timeout`
//!   deadline elapsed, a local stop signal arrived (`Ctrl-C`, on Unix `SIGTERM` /
//!   `SIGHUP`, or on Windows `Ctrl-Break`/console close/logoff/system shutdown),
//!   or a control-plane
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
    Command as PkCommand, Error as PkError, LimitKind, Mechanism, Outcome, OutputBufferPolicy,
    ProcessGroup, ProcessGroupOptions, RunningProcess, Signal, Stdin, StdioMode,
};

use crate::capture::{CAPTURE_INFLIGHT_MAX_BYTES, CAPTURE_MAX_BYTES, Capture, IdleClock};
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
    // output pumps, a deadline timer, and the stop-signal listeners (`Ctrl-C`, plus
    // `SIGTERM`/`SIGHUP` on Unix, plus `Ctrl-Break`/console close/logoff/system
    // shutdown on Windows). The shared helper's
    // `enable_all` arms the I/O, time, and signal drivers those need — the
    // child-pipe I/O driver is compiled in through `processkit`'s own tokio
    // `process`/`net` features, and the `time`/`signal` features this crate now
    // requests arm the rest (Cargo unifies them into the single tokio build).
    let runtime = control::current_thread_runtime()?;
    runtime.block_on(run_async(args))
}

/// Which runner deadline fired, for the shared timeout ending — the two share the
/// reserved `TIMEOUT` (106) code and the same teardown, told apart only by this tag
/// (surfaced as the `timeout` event's `reason` field, `docs/schema.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutTrigger {
    /// The whole-run `--timeout` deadline elapsed.
    Overall,
    /// The `--idle-timeout` elapsed: the child produced no output for the idle window.
    Idle,
}

impl TimeoutTrigger {
    /// The `timeout` event's always-present `reason` value for this trigger.
    fn reason(self) -> &'static str {
        match self {
            TimeoutTrigger::Overall => "overall",
            TimeoutTrigger::Idle => "idle",
        }
    }
}

/// Which **local stop signal** asked the runner to end the run — the honest
/// `source` of the `cancelled` JSONL event and the trigger the stderr line names.
///
/// **Decision (T-188): SIGTERM and SIGHUP get their own additive `source` values**
/// (`sigterm` / `sighup`) rather than reusing `ctrl_c`. Reusing `ctrl_c` for a
/// `systemd stop`, a cancelled CI job, or a plain `kill <pid>` would report a
/// keyboard interrupt that never happened — the same lie the runner refuses to tell
/// about a soft stop it could not deliver (see [`SoftTerminate`]/[`describe_teardown`]),
/// and consumers do act on the difference: "the operator interrupted me" and "my
/// supervisor is shutting me down" call for different handling. Adding values to an
/// existing string field is an **additive** schema change (no `schema_version` bump,
/// see `docs/schema.md`, "Versioning"), so the cost is one enum entry per echo site.
///
/// **Decision (T-195): the Windows console-control events get the same additive
/// treatment** (`ctrl_break` / `ctrl_close` / `ctrl_logoff` / `ctrl_shutdown`), for
/// the identical reason — each is a distinguishable *external* trigger, not a
/// keyboard interrupt, and a consumer that only knows `ctrl_c` still sees a
/// well-formed `cancelled` event.
///
/// The exit code is *not* split the same way: every local-signal cancel keeps
/// [`exit::CANCELLED`] (107) and the `cancelled` terminal `runner_exit` source,
/// because it is the same class of ending (a local signal ended the run) and the more
/// specific `cancelled.source` already disambiguates it one event earlier — the same
/// reasoning that kept `--idle-timeout` on `TIMEOUT` (106) rather than minting a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelSignal {
    /// The operator pressed `Ctrl-C` (`SIGINT` on Unix, the console handler on
    /// Windows).
    CtrlC,
    /// Unix `SIGTERM`: the standard *external* stop — `kill <pid>`, `systemctl stop`,
    /// a cancelled CI job, a supervisor's shutdown timeout. Not an interactive
    /// interrupt, and the most common way a runner is asked to go away.
    #[cfg(unix)]
    Term,
    /// Unix `SIGHUP`: the controlling terminal went away (a closed terminal, a dropped
    /// SSH session). Treated as a stop, not as the daemon "reload your config"
    /// convention — this runner supervises exactly one child and has nothing to reload,
    /// and the default disposition would kill it outright anyway.
    #[cfg(unix)]
    Hup,
    /// Windows `CTRL_BREAK_EVENT`: the operator (or a script) sent a break to the
    /// console process group. Unlike the other three Windows events below, this one
    /// carries no OS-imposed termination deadline — a process that ignores it simply
    /// keeps running — so it needs no grace clamp.
    #[cfg(windows)]
    CtrlBreak,
    /// Windows `CTRL_CLOSE_EVENT`: the console window is being closed (its "X"
    /// button, or an equivalent). The OS gives the handler only a short window
    /// (documented at [`CTRL_CLOSE_WINDOW`]) before terminating the process
    /// regardless — see [`effective_grace_for`] for how that bounds this trigger's
    /// effective `--grace`.
    #[cfg(windows)]
    CtrlClose,
    /// Windows `CTRL_LOGOFF_EVENT`: the user is logging off. Not delivered to a
    /// process outside the logging-off user's own session.
    #[cfg(windows)]
    CtrlLogoff,
    /// Windows `CTRL_SHUTDOWN_EVENT`: the system is shutting down.
    #[cfg(windows)]
    CtrlShutdown,
}

impl CancelSignal {
    /// The `cancelled` event's `source` value for this trigger (`docs/schema.md`,
    /// "cancelled").
    fn source(self) -> &'static str {
        match self {
            CancelSignal::CtrlC => "ctrl_c",
            #[cfg(unix)]
            CancelSignal::Term => "sigterm",
            #[cfg(unix)]
            CancelSignal::Hup => "sighup",
            #[cfg(windows)]
            CancelSignal::CtrlBreak => "ctrl_break",
            #[cfg(windows)]
            CancelSignal::CtrlClose => "ctrl_close",
            #[cfg(windows)]
            CancelSignal::CtrlLogoff => "ctrl_logoff",
            #[cfg(windows)]
            CancelSignal::CtrlShutdown => "ctrl_shutdown",
        }
    }

    /// How the stderr line names this trigger to a human.
    fn phrase(self) -> &'static str {
        match self {
            CancelSignal::CtrlC => "Ctrl-C",
            #[cfg(unix)]
            CancelSignal::Term => "SIGTERM",
            #[cfg(unix)]
            CancelSignal::Hup => "SIGHUP",
            #[cfg(windows)]
            CancelSignal::CtrlBreak => "Ctrl-Break",
            #[cfg(windows)]
            CancelSignal::CtrlClose => "console close",
            #[cfg(windows)]
            CancelSignal::CtrlLogoff => "logoff",
            #[cfg(windows)]
            CancelSignal::CtrlShutdown => "system shutdown",
        }
    }
}

/// How a run ended — the decision the race in [`run_async`] resolves to.
enum Ending {
    /// The child exited on its own; carries the raw wait result.
    Exited(processkit::Result<Outcome>),
    /// A runner deadline elapsed while the child was still running: the whole-run
    /// `--timeout` ([`TimeoutTrigger::Overall`]) or the `--idle-timeout`
    /// ([`TimeoutTrigger::Idle`]). Both take the same teardown and terminal code.
    TimedOut(TimeoutTrigger),
    /// A local stop signal reached the runner — `Ctrl-C`, (Unix) `SIGTERM` /
    /// `SIGHUP`, or (Windows) `Ctrl-Break`/console close/logoff/system shutdown.
    /// All take the same teardown and terminal code; the carried
    /// [`CancelSignal`] is what tells them apart on the wire.
    Cancelled(CancelSignal),
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
    /// A runner deadline (the elapsed `limit`) was exceeded: `trigger` names which —
    /// the whole-run `--timeout` or the `--idle-timeout`.
    Timeout {
        limit: Duration,
        trigger: TimeoutTrigger,
    },
    /// The run was cancelled by a local stop signal: `Ctrl-C`, (Unix) `SIGTERM` /
    /// `SIGHUP`, or (Windows) `Ctrl-Break`/console close/logoff/system shutdown.
    /// The carried [`CancelSignal`] names which, so the message stays honest.
    Cancelled(CancelSignal),
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
            // `Error::ResourceLimit`, *pre-spawn* (no child ran). Emit the
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
    // `--env-clear`/`--env-remove`/`--env` map onto processkit's own environment
    // builder, applied in that exact call order: clear first (a clean slate
    // instead of the runner's own inherited environment), then removals of
    // specific inherited vars, then explicit sets. `env`/`env_remove` accumulate
    // into one ordered list where a later entry wins on a duplicated key, so this
    // order makes an explicit `--env` always win over an `--env-remove` of the
    // same key — the order documented in README.md, "Environment".
    if args.env_clear {
        command = command.env_clear();
    }
    for key in &args.env_remove {
        command = command.env_remove(key);
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
    // Otherwise pipe + echo remains the compatibility default: ProcessKit's pump
    // reads each stream and tees it to the corresponding runner stream. With
    // `--capture-dir` that same tee also mirrors into bounded files. When
    // `--idle-timeout` is armed, an [`IdleClock`] tee wraps the *outermost* sink on
    // whichever of those two paths is active, so every observed chunk re-arms the
    // idle window regardless of capture mode; without it the sinks are exactly as
    // before. `--no-echo` swaps only the innermost echo sink
    // (`tokio::io::stdout()`/`stderr()`) for a discarding [`tokio::io::sink()`] —
    // the pipe, the pump, the `--capture-dir` tee, and the `IdleClock` re-arm all
    // stay wired exactly as without it (K-050, K-007). Every branch below that does
    // *not* test `args.no_echo` is byte-for-byte the pre-`--no-echo` code.
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
                    .stdout_tee(idle_clock.tee(capture.stdout_tee(tokio::io::sink())))
                    .stderr_tee(idle_clock.tee(capture.stderr_tee(tokio::io::sink())))
            } else {
                command
                    .stdout_tee(idle_clock.tee(capture.stdout_tee(tokio::io::stdout())))
                    .stderr_tee(idle_clock.tee(capture.stderr_tee(tokio::io::stderr())))
            }
        } else if args.no_echo {
            command
                .stdout_tee(capture.stdout_tee(tokio::io::sink()))
                .stderr_tee(capture.stderr_tee(tokio::io::sink()))
        } else {
            command
                .stdout_tee(capture.stdout_tee(tokio::io::stdout()))
                .stderr_tee(capture.stderr_tee(tokio::io::stderr()))
        }
    } else if idle_timeout.is_some() {
        if args.no_echo {
            command
                .stdout_tee(idle_clock.tee(tokio::io::sink()))
                .stderr_tee(idle_clock.tee(tokio::io::sink()))
        } else {
            command
                .stdout_tee(idle_clock.tee(tokio::io::stdout()))
                .stderr_tee(idle_clock.tee(tokio::io::stderr()))
        }
    } else if args.no_echo {
        command
            .stdout_tee(tokio::io::sink())
            .stderr_tee(tokio::io::sink())
    } else {
        command
            .stdout_tee(tokio::io::stdout())
            .stderr_tee(tokio::io::stderr())
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
    });
    emit_members_snapshot(&mut emitter, &group);

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
    // transport down) when another branch wins.
    let capturing = capture.is_some();
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
            let soft = soft_terminate_then_grace(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(soft_terminate_label(soft)));
            // A forced ending still reports whatever was captured before teardown.
            emit_output_captured(&mut emitter, &capture);
            // The registry entry is removed on every decided ending, not just the
            // happy path: a timeout tears the run down cleanly too.
            clear_registration(&registration);
            let error = termination_error(Termination::Timeout { limit, trigger }, soft, grace);
            Err(finish(&mut emitter, "timeout", None, error))
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
            let soft = soft_terminate_then_grace(&group, grace).await;
            emit_cleanup_finished(&mut emitter, &group, Some(soft_terminate_label(soft)));
            // A forced ending still reports whatever was captured before teardown.
            emit_output_captured(&mut emitter, &capture);
            // A signal cancel tears the run down cleanly too — its entry goes with it.
            clear_registration(&registration);
            let error = termination_error(Termination::Cancelled(signal), soft, grace);
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
/// cannot apply a requested cap it returns [`PkError::ResourceLimit`], and it does
/// so **pre-spawn** — the child never started, so no child code is ever at risk.
/// Because the CLI parsers (`src/cli.rs`) already reject every nonsensical value as
/// a `USAGE` (100) form error *before* we reach here, the only reasons that survive
/// to this point are the "could not be applied" ones — `Unsupported` (macOS/BSD and
/// the Linux process-group fallback have no whole-tree container at all) and
/// `Unenforceable` (a Linux cgroup v2 whose controllers can't be enabled — under
/// systemd, an ordinary container, or typical CI). That is the **same class** of
/// failure as the existing `ProcessGroup::new()` container-creation error ("a
/// whole-tree container capable of what was asked could not be established here"),
/// so the caller **deliberately reuses** the existing [`exit::BACKEND`] (102) code
/// and the `container_error` `runner_exit` `source` rather than minting a new code
/// in the free `112`–`119` slots. The distinguishing signal is the dedicated
/// `limit_hit` event emitted immediately before the shared tail — the authoritative,
/// machine-readable channel (`docs/exit-codes.md`, "Why a band is not enough on its
/// own"; the numeric exit code is only a best-effort hint). Codes `112`–`119` stay
/// reserved.
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
/// validated for form by the CLI parsers (`src/cli.rs` is the single source of
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

/// Report a failed interactive terminal-foreground handoff to the JSONL stream and
/// end the run — the shared tail both terminal-handoff failure paths in
/// [`run_async`] take (a failed [`TerminalForegroundGuard::acquire`], and the
/// failed post-handoff [`ProcessGroup::resume`]).
///
/// **Why a new `phase`.** Both paths sit *after* the child has spawned (the
/// container exists and may hold live members) but *before* the `run_started` event
/// is written. Neither existing `phase` describes them: `create` is "the container
/// could not be created" and `attach` is "the launch into it failed" — here both
/// already succeeded, and it is the *interactive terminal handoff* that failed. So
/// this emits `container_failed` with the additive `phase: "foreground"`, an
/// additive value in the v1 schema's `phase` enum (no `schema_version` bump, per the
/// schema's versioning policy: adding an enum value only widens what a reader may
/// see, it does not change any existing shape). Emitting it here restores the stream
/// invariant that a terminal `runner_exit` with `source: "container_error"` is
/// always preceded by a describing `container_failed`, which these two paths
/// previously broke by leaving the reason on stderr alone.
///
/// **Order.** `container_failed` first, then the shared [`emit_hard_teardown`] tail
/// (the child was spawned, so the container must be torn down), then the terminal
/// [`finish`] `runner_exit` — mirroring the pre-spawn `container_failed` paths
/// ([`ProcessGroup::new`]/[`ProcessGroup::start`] failures), which likewise emit the
/// event before ending. Routing both paths through this one site keeps them from
/// drifting apart, exactly as [`emit_hard_teardown`] does for the hard-teardown
/// callers. `message` carries the underlying error verbatim (the sibling
/// `container_failed` paths use the same raw-error convention); the runner's own
/// contextual framing rides on `error` to stderr.
fn finish_foreground_failure(
    emitter: &mut Emitter,
    group: &ProcessGroup,
    capture: &Option<Capture>,
    registration: &Option<registry::Registration>,
    error: RunnerError,
    message: String,
) -> RunnerError {
    emitter.emit(&Event::ContainerFailed {
        phase: "foreground",
        code: error.code(),
        message,
    });
    emit_hard_teardown(emitter, group, capture, registration);
    finish(emitter, "container_error", None, error)
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

/// Snapshot the container's members — enriched with `ppid`/executable
/// `name`/`start_time` via [`ProcessGroup::members_info`] wherever the platform
/// can report them (`events::Member::from_info`) — and emit `members_snapshot`. A
/// read failure is a diagnostics gap, not a run failure, so it warns and skips the
/// event; it shares the same error contract as the bare-PID `members()` this
/// replaced (`Error::Io` only — a single vanished member is skipped, not an
/// error).
fn emit_members_snapshot(emitter: &mut Emitter, group: &ProcessGroup) {
    match group.members_info() {
        Ok(infos) => emitter.emit(&Event::MembersSnapshot {
            members: infos.into_iter().map(Member::from_info).collect(),
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

/// The runner-imposed whole-run deadline: sleep `limit`, or (with no `--timeout`)
/// never resolve, so the race falls through to the other arms.
async fn deadline(limit: Option<Duration>) {
    match limit {
        Some(limit) => tokio::time::sleep(limit).await,
        None => std::future::pending::<()>().await,
    }
}

/// The runner-imposed **idle** deadline: resolve once the child has produced no
/// observed output for a full `idle` window. Unlike [`deadline`] this timer is
/// *re-armed* on every chunk of the child's output (the output sinks touch `clock`;
/// see [`IdleClock`]/[`crate::capture::IdleActivityTee`]) — it repeatedly sleeps the
/// idle time still remaining, and only resolves when that remaining reaches zero
/// with no fresh output having pushed it back out. With no `--idle-timeout` it never
/// resolves, so the race falls through to the other arms.
///
/// The loop is not a busy-poll: each iteration sleeps the *whole* remaining window
/// (`clock.remaining`), so a quiet run wakes exactly once, at the deadline; only a
/// run that keeps producing output loops, and then only once per output-driven
/// re-arm, never faster than the child speaks.
async fn idle_deadline(idle: Option<Duration>, clock: &IdleClock) {
    let Some(idle) = idle else {
        // No idle deadline armed: park forever so this arm never wins the race.
        std::future::pending::<()>().await;
        return;
    };
    loop {
        let remaining = clock.remaining(idle);
        if remaining.is_zero() {
            // A full idle window has elapsed with no output since the last re-arm.
            return;
        }
        tokio::time::sleep(remaining).await;
    }
}

/// Resolve when a **local stop signal** asks the runner to end the run, naming which
/// one arrived. This is the single cancel arm of [`run_async`]'s race, so every signal
/// it listens for takes the very same teardown (soft stop → `--grace` → hard kill) and
/// the same reserved [`exit::CANCELLED`] code.
///
/// On Unix that is three signals, not one: `SIGINT` (the interactive `Ctrl-C`),
/// `SIGTERM` (the standard external stop — `kill`, `systemctl stop`, a cancelled CI
/// job), and `SIGHUP` (the controlling terminal went away). Their **default**
/// dispositions all terminate the runner outright, which would skip teardown entirely:
/// no terminal JSONL events, a registry entry left behind, and — the guarantee that
/// actually matters — no explicit kill of the container, whose abrupt-owner-death reap
/// covers only the direct child on Linux and nothing at all on macOS/BSD (see
/// [`crate::events::abrupt_cleanup_str`], K-005). Catching them turns the most common
/// way a supervisor stops this runner into the same clean, fully-reported teardown a
/// `Ctrl-C` already got.
///
/// On Windows that is four events, not one: `Ctrl-Break` (the console break, no
/// termination deadline), and the three the console sends when it is about to end
/// the process regardless of what the runner does — console close, logoff, and
/// shutdown (`CTRL_CLOSE_EVENT`/`CTRL_LOGOFF_EVENT`/`CTRL_SHUTDOWN_EVENT`, delivered
/// via `SetConsoleCtrlHandler`, the same mechanism `Ctrl-C` already used). Their
/// default handling likewise terminates the runner outright, skipping teardown —
/// the terminal JSONL events, the registry-entry removal — for exactly the reasons
/// above, even though the tree itself is not left orphaned: on Windows the
/// abrupt-owner-death reap covers the *whole* tree (K-005; closing the runner's
/// last Job Object handle), unlike Linux's direct-child-only reap. The value of
/// catching these events is turning that invisible-but-contained ending into a
/// reported, ordinary one. `CtrlClose` carries an OS-imposed deadline (`--grace`'s
/// effective value is bounded by [`effective_grace_for`], see [`CTRL_CLOSE_WINDOW`]);
/// `CtrlLogoff`/`CtrlShutdown` are deliberately left uncapped (see that function's
/// doc for why).
///
/// A handler that cannot be installed degrades to "this signal is not handled" — that
/// arm never resolves, after an honest warning — rather than aborting an otherwise
/// healthy run; the remaining arms keep working. A signal the environment has already
/// neutralized (`SIG_IGN`, as `nohup` does for `SIGHUP`) is left alone rather than
/// un-ignored behind the operator's back — see [`wait_for_unix_signal`].
///
/// **Decision (T-195): a repeat console-control event mid-teardown is *not* absorbed
/// on Windows, unlike a repeat Unix signal.** This future's listeners are dropped the
/// instant the race resolves (teardown begins), same as every other arm. On Unix that
/// is harmless — the signal disposition stays installed at the OS level for the rest
/// of the process regardless of listener lifetime, so a second signal is silently
/// absorbed. On Windows the console-control handler routes through a per-listener
/// channel; once this future's receivers are gone, a repeat event is reported
/// *unhandled* and the OS falls through to its default disposition, which terminates
/// the process outright — mid-teardown, before the terminal JSONL events are written.
/// Keeping listeners alive for the whole teardown (not just the race) was considered
/// and rejected: it would mean threading persistent listener state through
/// `run_async` well past this function's boundary for the sake of an operator
/// double-press edge case. Documented here, and in `README.md`/`docs/schema.md`
/// ("Timeouts, cancel, and grace"), as an accepted trade-off, not a silent bug — see
/// the `#[cfg(windows)]` arm below for the full reasoning.
async fn wait_for_cancel_signal() -> CancelSignal {
    #[cfg(unix)]
    {
        // The handlers are installed on first poll of this future — i.e. once the race
        // begins — and stay installed for the rest of the process: tokio never restores
        // a default disposition, so a *second* signal arriving mid-teardown is absorbed
        // rather than killing the runner half-way through the cleanup it is running.
        // That is deliberate and already the behavior of the existing `Ctrl-C` arm:
        // teardown is bounded (`--grace` is an upper bound, cut short by
        // `wait_grace_or_empty`), and finishing it is the whole point of catching the
        // signal.
        tokio::select! {
            biased;
            () = wait_for_ctrl_c() => CancelSignal::CtrlC,
            () = wait_for_unix_signal(libc::SIGTERM, "SIGTERM") => CancelSignal::Term,
            () = wait_for_unix_signal(libc::SIGHUP, "SIGHUP") => CancelSignal::Hup,
        }
    }
    #[cfg(windows)]
    {
        // **Decision (T-195): documented asymmetry, not the Unix arm's "absorb a
        // repeat" guarantee.** On Unix, tokio installs the `sigaction` once, globally,
        // for the life of the process — dropping this future's listeners after the
        // race resolves only stops *this* future from being notified, it does not
        // restore the default disposition, so a second signal mid-teardown is
        // silently absorbed at the OS level (see the Unix arm above). Windows'
        // `SetConsoleCtrlHandler` model is different: tokio's handler routes each
        // event to a `watch::Sender`, and once every receiver for that signal has
        // been dropped (which happens here, together with this whole future, the
        // instant the outer `select!` in `run_async` resolves to *any* winning arm)
        // `Sender::send` returns `Err`, the handler reports the event as
        // *unhandled*, and the OS falls through to the next handler and ultimately
        // its own default disposition — which **terminates the process**. So: a
        // second console-control event that arrives after this race has already
        // resolved (i.e. during the soft-stop/`--grace`/hard-kill teardown below,
        // not during this race itself) is not absorbed — it kills the runner
        // mid-teardown, before `cleanup_finished`/`runner_exit` are written, the
        // exact invisible ending this feature exists to prevent for the *first*
        // event. This is a known, accepted trade-off (re-installing and holding
        // listeners alive for the whole teardown was rejected as unwarranted
        // complexity for an operator-repeat-keypress edge case), documented here and
        // in `README.md`/`docs/schema.md`, "Timeouts, cancel, and grace" — not a
        // silent bug.
        tokio::select! {
            biased;
            () = wait_for_ctrl_c() => CancelSignal::CtrlC,
            () = wait_for_windows_ctrl_event(tokio::signal::windows::ctrl_break, "Ctrl-Break") => {
                CancelSignal::CtrlBreak
            }
            () = wait_for_windows_ctrl_event(tokio::signal::windows::ctrl_close, "console close") => {
                CancelSignal::CtrlClose
            }
            () = wait_for_windows_ctrl_event(tokio::signal::windows::ctrl_logoff, "logoff") => {
                CancelSignal::CtrlLogoff
            }
            () = wait_for_windows_ctrl_event(tokio::signal::windows::ctrl_shutdown, "system shutdown") => {
                CancelSignal::CtrlShutdown
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        wait_for_ctrl_c().await;
        CancelSignal::CtrlC
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

/// Resolve when one delivery of the Unix signal `number` arrives. Degrades exactly
/// like [`wait_for_ctrl_c`]: a handler that cannot be installed warns once and then
/// parks forever, so this arm simply never wins the race and the run continues
/// unaffected.
///
/// **Never overrides an inherited `SIG_IGN`.** Installing a handler replaces the
/// disposition unconditionally, including a deliberate "ignore this signal" the
/// environment set before exec'ing us — `nohup` does exactly that for `SIGHUP`, and a
/// supervisor may do it for `SIGTERM`. Silently un-ignoring the signal would turn
/// `nohup processkit-cli run …` from "survives the hangup" into "stops on it", so the
/// disposition is checked first ([`signal_is_ignored`]) and this arm simply parks
/// instead. Nothing is lost by doing so: an ignored signal would not have terminated
/// the runner either, so there is no teardown to rescue — the run continues exactly
/// as it did before this listener existed. No warning is printed, because this is a
/// policy the environment chose, not a failure.
#[cfg(unix)]
async fn wait_for_unix_signal(number: libc::c_int, name: &str) {
    if signal_is_ignored(number) {
        return std::future::pending().await;
    }
    let kind = tokio::signal::unix::SignalKind::from_raw(number);
    let mut signal = match tokio::signal::unix::signal(kind) {
        Ok(signal) => signal,
        Err(err) => {
            eprintln!("processkit-cli: warning: {name} handling is unavailable: {err}");
            return std::future::pending().await;
        }
    };
    // `recv()` yields `None` only once the underlying handler is torn down, which
    // cannot happen while this future owns the stream. Park rather than report a
    // cancel that no signal actually triggered.
    if signal.recv().await.is_none() {
        std::future::pending::<()>().await;
    }
}

/// Is this signal's current disposition `SIG_IGN` — i.e. did whoever launched the
/// runner deliberately neutralize it (the classic case being `nohup`, which ignores
/// `SIGHUP` before exec)? A disposition *query* only: nothing is installed or
/// changed here. A failed query reads as "not ignored", so the caller falls back to
/// its ordinary listener rather than silently dropping a signal it could have caught.
///
/// Applied to the **new** `SIGTERM`/`SIGHUP` listeners only, not to the pre-existing
/// `Ctrl-C` one: this guard exists to avoid *changing* how an already-neutralized
/// signal behaves, and `Ctrl-C` has always installed its handler unconditionally.
/// Reworking that would be a behavior change of its own, not part of this addition.
#[cfg(unix)]
fn signal_is_ignored(number: libc::c_int) -> bool {
    // SAFETY: `sigaction` with a null `act` only reads the current disposition and
    // leaves it untouched; `current` is a valid, writable, zero-initialized value for
    // the duration of the call (the same plain-C-value pattern as
    // `ScopedSignalIgnore::acquire`).
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(number, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

/// Adapts the four distinctly-typed Windows console-control listeners
/// (`tokio::signal::windows::{CtrlBreak,CtrlClose,CtrlLogoff,CtrlShutdown}`) to one
/// shape so [`wait_for_windows_ctrl_event`] can drive any of them generically. They
/// are otherwise unrelated structs (tokio gives each its own type, with no shared
/// public trait) even though every one wraps the identical
/// `SetConsoleCtrlHandler`-backed listener and exposes the same `recv` shape.
#[cfg(windows)]
trait WindowsCtrlListener {
    async fn wait_one(&mut self) -> Option<()>;
}

#[cfg(windows)]
impl WindowsCtrlListener for tokio::signal::windows::CtrlBreak {
    async fn wait_one(&mut self) -> Option<()> {
        self.recv().await
    }
}

#[cfg(windows)]
impl WindowsCtrlListener for tokio::signal::windows::CtrlClose {
    async fn wait_one(&mut self) -> Option<()> {
        self.recv().await
    }
}

#[cfg(windows)]
impl WindowsCtrlListener for tokio::signal::windows::CtrlLogoff {
    async fn wait_one(&mut self) -> Option<()> {
        self.recv().await
    }
}

#[cfg(windows)]
impl WindowsCtrlListener for tokio::signal::windows::CtrlShutdown {
    async fn wait_one(&mut self) -> Option<()> {
        self.recv().await
    }
}

/// Resolve when one delivery of a Windows console-control event arrives — `make`
/// installs the listener (e.g. [`tokio::signal::windows::ctrl_break`]), `name` is
/// only for the degradation warning below. Degrades exactly like
/// [`wait_for_unix_signal`]: a handler that cannot be installed warns once and then
/// parks forever, so this arm simply never wins the race and the run continues
/// unaffected — installing a console-control handler is a lightweight, ordinary
/// operation (unlike `SIGHUP`'s inherited-`SIG_IGN` case on Unix), so there is no
/// disposition to preserve here.
#[cfg(windows)]
async fn wait_for_windows_ctrl_event<T, F>(make: F, name: &str)
where
    F: FnOnce() -> std::io::Result<T>,
    T: WindowsCtrlListener,
{
    let mut listener = match make() {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("processkit-cli: warning: {name} handling is unavailable: {err}");
            return std::future::pending().await;
        }
    };
    // `recv()` on every one of these listeners never actually yields `None` (see
    // their doc comments in `tokio::signal::windows`) — this mirrors the honest,
    // never-report-a-cancel-nothing-triggered shape of `wait_for_unix_signal` rather
    // than assume that guarantee holds forever.
    if listener.wait_one().await.is_none() {
        std::future::pending::<()>().await;
    }
}

/// The approximate window Windows gives a process that caught `CTRL_CLOSE_EVENT`
/// (the console window's close button) to clean up before terminating it
/// regardless of what the handler is doing — see [`effective_grace_for`].
#[cfg(windows)]
const CTRL_CLOSE_WINDOW: Duration = Duration::from_secs(5);

/// Headroom subtracted from [`CTRL_CLOSE_WINDOW`] to get [`CTRL_CLOSE_GRACE_BUDGET`]:
/// the trivial JSONL-event-write/hard-kill overhead that already shares the OS's
/// window, plus scheduling jitter under load.
#[cfg(windows)]
const CTRL_CLOSE_SAFETY_MARGIN: Duration = Duration::from_secs(2);

/// The effective upper bound this runner allows `--grace` to reach for a
/// [`CancelSignal::CtrlClose`] ending: [`CTRL_CLOSE_WINDOW`] minus
/// [`CTRL_CLOSE_SAFETY_MARGIN`], computed rather than an independent constant so
/// the two can never silently drift apart.
#[cfg(windows)]
const CTRL_CLOSE_GRACE_BUDGET: Duration =
    Duration::from_secs(CTRL_CLOSE_WINDOW.as_secs() - CTRL_CLOSE_SAFETY_MARGIN.as_secs());

/// **Decision (T-195): the `CTRL_CLOSE` OS deadline caps the *effective* grace for
/// that one trigger only.** Windows gives a process that caught `CTRL_CLOSE_EVENT`
/// only [`CTRL_CLOSE_WINDOW`] (about 5 seconds) to clean up before terminating it
/// regardless — a stricter deadline than the operator's own `--grace` was ever
/// assumed to fit inside. If a requested `--grace` — plus the (normally trivial)
/// event-write and hard-kill overhead that already shares that window — did not
/// fit, the OS could kill the runner *mid-wait*, before `cleanup_finished`/
/// `runner_exit` are even written: the worst possible outcome for this feature, an
/// *invisible* teardown, exactly what catching the event exists to prevent. So for
/// `CtrlClose` specifically the effective grace is capped to
/// [`CTRL_CLOSE_GRACE_BUDGET`]: a `--grace` that does not fit degrades to the
/// shorter, honest wait rather than risking the OS's own unreported kill. The
/// *reported* `grace_ms` (the `cancelled` event, and the stderr headline) is this
/// same effective value, never the raw request, so the stream never claims a wait
/// that could not actually happen.
///
/// `CtrlBreak` needs no cap: it carries no forced-termination deadline (a process
/// that ignores it simply keeps running).
///
/// `CtrlLogoff` and `CtrlShutdown` are deliberately left **uncapped**: their real
/// deadline is the system-wide `WaitToKillAppTimeout` shutdown policy (itself
/// further extendable per-process via `ShutdownBlockReasonCreate`, which this
/// runner does not call) — neither a fixed constant nor reliably discoverable at
/// run time, unlike `CTRL_CLOSE_EVENT`'s well-documented ~5s window. Hardcoding a
/// matching cap here would be guessing, not honesty. A long `--grace` combined with
/// an imminent forced logoff/shutdown can still lose the terminal events; that is a
/// known, documented trade-off for those two triggers, not a silent bug.
///
/// Every other [`CancelSignal`] (`Ctrl-C`, the Unix signals, `CtrlBreak`,
/// `CtrlLogoff`, `CtrlShutdown`) passes `grace` through unchanged.
///
/// Split by `#[cfg(windows)]` rather than a single `match` with a `CtrlClose`
/// arm gated behind `#[cfg(windows)]` and a catch-all `_`: on a non-Windows
/// target that single arm vanishes before the linter ever sees it, leaving a
/// match with exactly one reachable arm — `clippy::match_single_binding`, which
/// this crate's CI runs with `-D warnings`. The non-Windows body below never even
/// mentions `signal`, so a leading `let _ = signal;` keeps it from tripping
/// `unused_variables` instead.
#[cfg(windows)]
fn effective_grace_for(signal: CancelSignal, grace: Option<Duration>) -> Option<Duration> {
    match signal {
        CancelSignal::CtrlClose => grace.map(|grace| grace.min(CTRL_CLOSE_GRACE_BUDGET)),
        _ => grace,
    }
}

#[cfg(not(windows))]
fn effective_grace_for(signal: CancelSignal, grace: Option<Duration>) -> Option<Duration> {
    let _ = signal;
    grace
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
/// own `Ctrl-C` on Windows — a chance to exit first) before the atomic kill — but
/// only as an *upper bound*: [`wait_grace_or_empty`] cuts the wait short the
/// moment the tree is observed empty, rather than always sleeping the whole
/// window.
async fn soft_terminate_then_grace(group: &ProcessGroup, grace: Option<Duration>) -> SoftTerminate {
    let soft = match group.signal(Signal::Term) {
        Ok(()) => SoftTerminate::Signalled,
        Err(PkError::Unsupported { .. }) => SoftTerminate::Unsupported,
        // Best-effort: a delivery failure does not stop teardown — the group's
        // kill-on-drop still reaps the tree — but it is reported honestly.
        Err(_) => SoftTerminate::Failed,
    };
    if let Some(grace) = grace {
        wait_grace_or_empty(group, grace).await;
    }
    soft
}

/// The polling step used by [`wait_grace_or_empty`] to check for an early-empty
/// container: short enough that a promptly-exiting tree does not hold the runner
/// (and an already-acked control client) for a meaningfully longer tail than
/// necessary, long enough not to turn teardown into a busy-poll.
const GRACE_POLL_STEP: Duration = Duration::from_millis(25);

/// Wait out `grace`, but return as soon as the container's tree is observed
/// empty — `--grace` is an *upper bound* on how long we wait for a voluntary
/// exit after the soft stop, not a mandatory delay. Polls
/// [`ProcessGroup::members`] on [`GRACE_POLL_STEP`] instead of a single
/// `sleep(grace)`, so a tree that dies well inside the window releases the
/// runner (and any control-plane caller already ack'd) promptly instead of
/// idling for the rest of `grace`.
///
/// POSIX caveat, deliberately accepted rather than worked around: a member that
/// *just* exited can still be reported by `members()` until something reaps it
/// (an un-reaped process is still "there" to a liveness check) — this is the
/// same abrupt-cleanup tri-state documented at [`crate::events::abrupt_cleanup_str`]
/// (K-005), not a Windows-only concern. The effect here is bounded and one-sided:
/// it can make the empty-tree check lag the real exit by at most one poll step
/// (negligible next to the grace window it shortens), and it can never
/// *under*-report a live tree as empty — a still-running member is always seen —
/// so an early return is always honest, only possibly a poll step late. A read
/// failure is treated the same as "not yet empty": teardown falls back to the
/// unmodified full-grace wait rather than guessing.
async fn wait_grace_or_empty(group: &ProcessGroup, grace: Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        tokio::time::sleep(remaining.min(GRACE_POLL_STEP)).await;
        if matches!(group.members(), Ok(members) if members.is_empty()) {
            return;
        }
    }
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
        // Both timeout triggers surface the same reserved code; only the headline
        // differs, naming which deadline elapsed so the stderr line is honest.
        Termination::Timeout {
            limit,
            trigger: TimeoutTrigger::Overall,
        } => (
            exit::TIMEOUT,
            format!("run timed out after {}", format_duration(limit)),
        ),
        Termination::Timeout {
            limit,
            trigger: TimeoutTrigger::Idle,
        } => (
            exit::TIMEOUT,
            format!(
                "run idle-timed out after {} with no output",
                format_duration(limit)
            ),
        ),
        // Every local-signal cancel surfaces the same reserved code; only the headline
        // differs, naming the signal that actually arrived (`Ctrl-C`, `SIGTERM`,
        // `SIGHUP`) so the stderr line is honest about who stopped the run.
        Termination::Cancelled(signal) => (
            exit::CANCELLED,
            format!("run cancelled ({})", signal.phrase()),
        ),
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
            Termination::Timeout {
                limit: Duration::from_secs(5),
                trigger: TimeoutTrigger::Overall,
            },
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let cancelled = termination_error(
            Termination::Cancelled(CancelSignal::CtrlC),
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
            Termination::Timeout {
                limit: Duration::from_secs(5),
                trigger: TimeoutTrigger::Overall,
            },
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

    /// Both timeout triggers surface the reserved `TIMEOUT` code (an idle expiry is
    /// the *same class* of ending as an overall one — a deadline the runner enforced,
    /// per K-047/the task's exit-code decision), but their stderr headlines differ so
    /// an operator can tell "ran too long overall" from "went silent". `reason` on
    /// the JSONL `timeout` event is the machine-readable counterpart.
    #[test]
    fn idle_timeout_reuses_the_timeout_code_with_its_own_message() {
        let idle = termination_error(
            Termination::Timeout {
                limit: Duration::from_secs(3),
                trigger: TimeoutTrigger::Idle,
            },
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        let overall = termination_error(
            Termination::Timeout {
                limit: Duration::from_secs(3),
                trigger: TimeoutTrigger::Overall,
            },
            SoftTerminate::Signalled,
            Some(Duration::from_secs(2)),
        );
        // Same reserved class of ending, same code.
        assert_eq!(idle.code(), exit::TIMEOUT);
        assert_eq!(overall.code(), exit::TIMEOUT);

        let idle_msg = idle.to_string();
        assert!(
            idle_msg.contains("idle-timed out"),
            "an idle expiry names itself as an idle timeout: {idle_msg}"
        );
        assert!(
            idle_msg.contains("no output"),
            "the idle message states why (no output): {idle_msg}"
        );
        assert!(
            idle_msg.contains("3s"),
            "the idle window is echoed: {idle_msg}"
        );
        assert_ne!(
            idle_msg,
            overall.to_string(),
            "the idle and overall headlines must read differently"
        );

        // And the reason strings the JSONL event carries stay distinct.
        assert_eq!(TimeoutTrigger::Overall.reason(), "overall");
        assert_eq!(TimeoutTrigger::Idle.reason(), "idle");
    }

    #[test]
    fn cancel_message_names_ctrl_c() {
        let err = termination_error(
            Termination::Cancelled(CancelSignal::CtrlC),
            SoftTerminate::Signalled,
            None,
        );
        let msg = err.to_string();
        assert!(
            msg.contains("cancelled"),
            "message should say cancelled: {msg}"
        );
        assert!(msg.contains("Ctrl-C"), "message should name Ctrl-C: {msg}");
    }

    /// Every local stop signal is the *same class* of ending — a signal ended the run —
    /// so all of them keep the reserved `CANCELLED` code (K-047: an earlier, more
    /// specific record already disambiguates, here the `cancelled` event's `source`).
    /// What must **not** collapse is the reporting: the stderr headline names the
    /// signal that actually arrived, and the wire `source` values stay distinct, so a
    /// `systemctl stop` is never reported as a keyboard interrupt.
    #[cfg(unix)]
    #[test]
    fn unix_stop_signals_share_the_cancel_code_but_report_themselves_honestly() {
        let for_signal = |signal| {
            termination_error(
                Termination::Cancelled(signal),
                SoftTerminate::Signalled,
                Some(Duration::from_secs(2)),
            )
        };
        let ctrl_c = for_signal(CancelSignal::CtrlC);
        let sigterm = for_signal(CancelSignal::Term);
        let sighup = for_signal(CancelSignal::Hup);

        // One class of ending, one reserved code.
        for err in [&ctrl_c, &sigterm, &sighup] {
            assert_eq!(err.code(), exit::CANCELLED);
        }

        // Distinct, honest headlines.
        let sigterm_msg = sigterm.to_string();
        assert!(
            sigterm_msg.contains("run cancelled (SIGTERM)"),
            "a SIGTERM cancel must name SIGTERM: {sigterm_msg}"
        );
        assert!(
            !sigterm_msg.contains("Ctrl-C"),
            "a SIGTERM is not a Ctrl-C: {sigterm_msg}"
        );
        let sighup_msg = sighup.to_string();
        assert!(
            sighup_msg.contains("run cancelled (SIGHUP)"),
            "a SIGHUP cancel must name SIGHUP: {sighup_msg}"
        );
        assert_ne!(ctrl_c.to_string(), sigterm_msg);
        assert_ne!(sigterm_msg, sighup_msg);

        // And the machine-readable `source` values a consumer switches on.
        assert_eq!(CancelSignal::CtrlC.source(), "ctrl_c");
        assert_eq!(CancelSignal::Term.source(), "sigterm");
        assert_eq!(CancelSignal::Hup.source(), "sighup");
    }

    /// The Windows sibling of the Unix proof above: every console-control event
    /// shares the reserved `CANCELLED` code (the same class of ending) but keeps a
    /// distinct, honest `source`/stderr headline — a console close is never
    /// reported as a keyboard interrupt, a logoff, or a shutdown.
    #[cfg(windows)]
    #[test]
    fn windows_ctrl_events_share_the_cancel_code_but_report_themselves_honestly() {
        let for_signal = |signal| {
            termination_error(
                Termination::Cancelled(signal),
                SoftTerminate::Unsupported,
                Some(Duration::from_secs(2)),
            )
        };
        let ctrl_c = for_signal(CancelSignal::CtrlC);
        let ctrl_break = for_signal(CancelSignal::CtrlBreak);
        let ctrl_close = for_signal(CancelSignal::CtrlClose);
        let ctrl_logoff = for_signal(CancelSignal::CtrlLogoff);
        let ctrl_shutdown = for_signal(CancelSignal::CtrlShutdown);

        // One class of ending, one reserved code.
        for err in [
            &ctrl_c,
            &ctrl_break,
            &ctrl_close,
            &ctrl_logoff,
            &ctrl_shutdown,
        ] {
            assert_eq!(err.code(), exit::CANCELLED);
        }

        // Distinct, honest headlines.
        let ctrl_break_msg = ctrl_break.to_string();
        let ctrl_close_msg = ctrl_close.to_string();
        let ctrl_logoff_msg = ctrl_logoff.to_string();
        let ctrl_shutdown_msg = ctrl_shutdown.to_string();
        assert!(
            ctrl_break_msg.contains("run cancelled (Ctrl-Break)"),
            "the message must name Ctrl-Break: {ctrl_break_msg}"
        );
        assert!(
            ctrl_close_msg.contains("run cancelled (console close)"),
            "the message must name console close: {ctrl_close_msg}"
        );
        assert!(
            ctrl_logoff_msg.contains("run cancelled (logoff)"),
            "the message must name logoff: {ctrl_logoff_msg}"
        );
        assert!(
            ctrl_shutdown_msg.contains("run cancelled (system shutdown)"),
            "the message must name system shutdown: {ctrl_shutdown_msg}"
        );
        let messages = [
            ctrl_c.to_string(),
            ctrl_break_msg,
            ctrl_close_msg,
            ctrl_logoff_msg,
            ctrl_shutdown_msg,
        ];
        for (i, a) in messages.iter().enumerate() {
            for b in &messages[i + 1..] {
                assert_ne!(a, b, "two distinct triggers produced the same message");
            }
        }

        // And the machine-readable `source`/human `phrase` values.
        assert_eq!(CancelSignal::CtrlBreak.source(), "ctrl_break");
        assert_eq!(CancelSignal::CtrlBreak.phrase(), "Ctrl-Break");
        assert_eq!(CancelSignal::CtrlClose.source(), "ctrl_close");
        assert_eq!(CancelSignal::CtrlClose.phrase(), "console close");
        assert_eq!(CancelSignal::CtrlLogoff.source(), "ctrl_logoff");
        assert_eq!(CancelSignal::CtrlLogoff.phrase(), "logoff");
        assert_eq!(CancelSignal::CtrlShutdown.source(), "ctrl_shutdown");
        assert_eq!(CancelSignal::CtrlShutdown.phrase(), "system shutdown");
    }

    /// `effective_grace_for` is the identity for every ordinary trigger — proved here
    /// with the always-present `CtrlC` so the passthrough path is covered on every
    /// platform, not only Windows (where [`CancelSignal::CtrlClose`] is the one
    /// exception, proved separately below).
    #[test]
    fn effective_grace_passes_through_unchanged_for_ordinary_triggers() {
        assert_eq!(
            effective_grace_for(CancelSignal::CtrlC, Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(effective_grace_for(CancelSignal::CtrlC, None), None);
    }

    /// **T-195's CTRL_CLOSE decision, proved directly**: a `--grace` that would not
    /// fit inside the OS's own termination window is clamped down to
    /// [`CTRL_CLOSE_GRACE_BUDGET`] for `CtrlClose` alone — a request that already
    /// fits, or no `--grace` at all, is left unchanged — while the sibling Windows
    /// triggers (`CtrlBreak`/`CtrlLogoff`/`CtrlShutdown`), which carry no matching
    /// OS deadline this runner can honestly bound, are deliberately left uncapped.
    #[cfg(windows)]
    #[test]
    fn ctrl_close_grace_is_clamped_to_the_os_window_budget_but_sibling_triggers_are_not() {
        assert_eq!(
            effective_grace_for(CancelSignal::CtrlClose, Some(Duration::from_secs(30))),
            Some(CTRL_CLOSE_GRACE_BUDGET),
            "a --grace that does not fit the OS window must degrade to the budget"
        );
        assert_eq!(
            effective_grace_for(CancelSignal::CtrlClose, Some(Duration::from_secs(1))),
            Some(Duration::from_secs(1)),
            "a --grace that already fits must pass through unchanged"
        );
        assert_eq!(
            effective_grace_for(CancelSignal::CtrlClose, None),
            None,
            "no --grace at all stays unset (no wait is attempted either way)"
        );
        for signal in [
            CancelSignal::CtrlBreak,
            CancelSignal::CtrlLogoff,
            CancelSignal::CtrlShutdown,
        ] {
            assert_eq!(
                effective_grace_for(signal, Some(Duration::from_secs(30))),
                Some(Duration::from_secs(30)),
                "{signal:?} must not be clamped like CtrlClose"
            );
        }
    }

    /// A sanity check on the constants themselves: the budget this runner allows
    /// must actually leave headroom under the OS's own window, else the whole
    /// decision above would be a no-op.
    #[cfg(windows)]
    #[test]
    fn ctrl_close_grace_budget_leaves_headroom_under_the_os_window() {
        assert!(
            CTRL_CLOSE_GRACE_BUDGET < CTRL_CLOSE_WINDOW,
            "the grace budget must leave headroom under the OS's own termination window"
        );
    }

    #[test]
    fn the_four_runner_imposed_endings_carry_distinct_codes() {
        // Every runner-imposed ending must be tellable apart by exit code: a timeout,
        // a Ctrl-C, a control-plane cancel, and a control-plane kill.
        let timeout = termination_error(
            Termination::Timeout {
                limit: Duration::from_secs(5),
                trigger: TimeoutTrigger::Overall,
            },
            SoftTerminate::Signalled,
            None,
        );
        let ctrl_c = termination_error(
            Termination::Cancelled(CancelSignal::CtrlC),
            SoftTerminate::Signalled,
            None,
        );
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
        use clap::Parser;

        let base = |argv: &[&str]| -> RunArgs {
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
        };

        assert!(
            build_limit_options(&base(&[])).is_none(),
            "no limit flag ⇒ the unchanged ProcessGroup::new() path"
        );
        for flag in [
            vec!["--max-memory", "256m"],
            vec!["--max-processes", "8"],
            vec!["--cpu-quota", "1.5"],
        ] {
            assert!(
                build_limit_options(&base(&flag)).is_some(),
                "the {flag:?} flag must request ProcessGroupOptions"
            );
        }
    }

    /// Forcing a real wait *failure* through the child's actual OS-level wait
    /// call is practically unreachable from a test (`RunningProcess::wait`'s own
    /// `Err` path is backend-internal plumbing, not something a spawned test
    /// child can be made to trigger deterministically) — and the same is true
    /// of forcing `exit_code_for` into its own `Err` arm (an untimed
    /// `Outcome::TimedOut` or an unrecognized `#[non_exhaustive]` variant is not
    /// producible by this crate's real backend from the test arsenal either).
    /// So this proves the thing that *is* reachable and is the actual fix:
    /// [`emit_hard_teardown`], the exact shared tail both of those `Ending::Exited`
    /// error arms now run (see the `Err(err)` arm on the wait itself, and the
    /// `Err(error)` arm on `exit_code_for(outcome)`, in `run_async`), fires
    /// `cleanup_started` → the hard kill via `cleanup_finished` (with no
    /// soft-terminate tier) → `output_captured` → nothing else, in that order,
    /// for *any* caller — natural exit, control-kill, and both decode-failure
    /// paths alike. A future edit that special-cases one of those callers back
    /// out of this shared function (as the wait-failure path used to be) has
    /// nowhere to silently diverge: it would have to stop calling this helper,
    /// which is visible on review.
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
        let capture = Some(
            Capture::create(&dir.join("capture"), CAPTURE_MAX_BYTES)
                .expect("create the capture dir"),
        );

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

    /// Both interactive terminal-handoff failure paths — a failed
    /// `TerminalForegroundGuard::acquire` and the failed post-handoff
    /// `group.resume()`, both in `run_async` — now route through
    /// [`finish_foreground_failure`], which restores the stream invariant that a
    /// terminal `runner_exit` with `source: "container_error"` is always preceded by
    /// a describing `container_failed` (previously these two paths emitted only the
    /// teardown pair and the terminal exit, leaving the reason on stderr alone).
    ///
    /// Driving `run_async` itself into those branches needs a real controlling
    /// terminal plus a `tcsetpgrp`/`resume` that fails on demand — not reachable
    /// deterministically from a test. So, like the sibling
    /// `hard_teardown_tail_emits_the_shared_sequence_in_order`, this exercises the
    /// exact shared site both paths take, with a real `ProcessGroup`/`Emitter`
    /// (K-015, no mocks), and pins the emitted sequence and the ordering invariant.
    #[tokio::test]
    async fn foreground_failure_emits_container_failed_before_the_terminal_exit() {
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
            "processkit-cli-run-unit-foreground-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let jsonl = dir.join("events.jsonl");
        let mut emitter = Emitter::create(&jsonl).expect("create the events file");

        // The `RunnerError` carries the runner's contextual framing (→ stderr); the
        // separate `message` is the underlying error the `container_failed` records.
        let error = RunnerError::new(
            exit::BACKEND,
            "could not give the interactive child terminal control: simulated".to_string(),
        );
        let returned = finish_foreground_failure(
            &mut emitter,
            &group,
            &None,
            &None,
            error,
            "simulated terminal-handoff failure".to_string(),
        );
        // Like `finish`, the error is returned unchanged (the reserved BACKEND code).
        assert_eq!(returned.code(), exit::BACKEND);

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
            vec![
                "container_failed",
                "cleanup_started",
                "cleanup_finished",
                "runner_exit",
            ],
            "a terminal-handoff failure emits the describing container_failed first, \
             then the hard-teardown pair, then the terminal runner_exit"
        );

        // The describing event carries the new `foreground` phase, the BACKEND code,
        // and the raw underlying message.
        assert_eq!(lines[0]["phase"], "foreground");
        assert_eq!(lines[0]["code"], exit::BACKEND);
        assert_eq!(lines[0]["message"], "simulated terminal-handoff failure");

        // The invariant this task restores: the terminal `container_error`
        // `runner_exit` is preceded by a `container_failed` — it is no longer the
        // lone record of the failure.
        let runner_exit = lines.last().expect("a terminal event");
        assert_eq!(runner_exit["event"], "runner_exit");
        assert_eq!(runner_exit["source"], "container_error");
        assert_eq!(runner_exit["code"], i32::from(exit::BACKEND));
        assert!(
            runner_exit["child_code"].is_null(),
            "a runner-own failure forwards no child code: {runner_exit}"
        );
        let container_failed_at = kinds
            .iter()
            .position(|k| *k == "container_failed")
            .expect("container_failed present");
        let runner_exit_at = kinds
            .iter()
            .position(|k| *k == "runner_exit")
            .expect("runner_exit present");
        assert!(
            container_failed_at < runner_exit_at,
            "container_failed must precede the terminal runner_exit: {kinds:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
