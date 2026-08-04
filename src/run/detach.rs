//! Detached runs (`--detach`): the wrapper that hands a run to a detached copy of
//! this binary and returns once that copy has provably started.
//!
//! Everything here implements the wrapper described in the parent module's fifth
//! settled decision: spawn a detached copy of this binary, prove it started, and
//! get out of the way. None of it re-implements any part of the run itself — the
//! detached copy re-enters `super::run_inner` through the ordinary
//! [`super::execute`] path, because the argv it receives is this invocation's own
//! with `--detach` removed.
//!
//! **Decision: a detached run is indistinguishable on the wire.** No event, field,
//! `runner_exit.source`, or exit code is minted for detaching, and none is needed:
//! the detached copy *is* an ordinary run — it never learns it was detached (the
//! flag is gone from its argv), so its JSONL stream has exactly the shape, values,
//! and `schema_version` a foreground run's does. What changed is only who waits for
//! it, which is a property of the caller, not of the run, and is reported where it
//! belongs: this process's own exit code (see `docs/exit-codes.md`, "Detached
//! runs"). That keeps the schema, the golden fixture, and every closed enumeration
//! in `src/events.rs` untouched by this feature.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::cli::RunArgs;
use crate::duration_fmt::format_duration;
use crate::error_envelope::ErrorKind;
use crate::events::{self, Emitter};
use crate::exit::{self, RunnerError};

/// The `--detach` token as it appears in this process's own argv. [`detached_argv`]
/// strips exactly this — and only before the `--` separator — when handing the run
/// on, which is what keeps the detached copy from detaching again forever.
const DETACH_FLAG: &str = "--detach";

/// The `--no-echo` token [`detached_argv`] adds when the caller did not: a detached
/// run has no live audience for the child's output, so it runs the *existing*
/// echo-suppression path rather than a second one of its own.
const NO_ECHO_FLAG: &str = "--no-echo";

/// The `--run-id` token [`detached_argv`] adds when the caller did not, so the
/// detached run has an id this process already knows and can watch for.
const RUN_ID_FLAG: &str = "--run-id";

/// How long to wait between checks for the detached runner's `run_started` event.
///
/// Much shorter than [`crate::wait`]'s poll step, and deliberately so: this loop
/// bounds how long a "return immediately" command actually takes, and it runs for
/// startup only (milliseconds), not for the life of a run. Each tick costs one small
/// file read of a stream that is at most a few lines long at this point.
const DETACH_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long to wait for the detached runner to report a started run before giving up.
///
/// A backstop, not a tuning knob: a healthy start takes milliseconds, and the loop
/// returns the moment it sees `run_started`, so this bound is only reached when the
/// detached copy is alive but wedged before it ever started the run. Generous enough
/// that a heavily loaded host is not mistaken for a wedged runner, finite so that
/// `run --detach` cannot hang an orchestrator forever.
const DETACH_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Hand this run to a detached copy of this binary and return once it has provably
/// started — the whole of `--detach`.
///
/// **Why re-spawn instead of daemonizing in place.** The run is already a complete,
/// self-contained path (`run_inner` → [`super::launch::run_async`]); the only thing a caller wants
/// to change is *who waits for it*. Re-spawning this binary on the same argv keeps
/// that path byte-for-byte identical in the detached process — same container, same
/// registry entry, same control transport, same JSONL, same teardown — instead of
/// growing a second, subtly different lifecycle for detached runs. On Unix the copy
/// is put in a **new session** (`setsid`, see [`spawn_detached`]), so a terminal
/// hang-up or a `Ctrl-C` in the caller's session no longer reaches it; on Windows it
/// is created with `DETACHED_PROCESS`, so it holds no console handle.
///
/// **What "started" means, and why it is that.** This returns only after the
/// detached runner's **`run_started` event is readable in `--jsonl`**. That single
/// observation is load-bearing: [`super::launch::run_async`] emits `run_started` *after* it has
/// opened the event stream, created the container, published the registry record
/// ([`super::launch::register_run`]), and spawned the child — so seeing it proves every one of those
/// steps already happened, and it is the only one of them a separate process can
/// confirm without racing the run (a registry entry, by contrast, is *gone* again the
/// moment a fast run ends, so its absence would be ambiguous). The event is flushed
/// per line by [`Emitter`], so it is visible to this process as soon as it is
/// written.
///
/// **Fail-closed.** Three ways a start can fail, none of them silent:
/// - the events file cannot be created — reported here, before anything is spawned,
///   exactly as [`super::launch::run_async`] would have reported it ([`exit::SETUP`]);
/// - the detached copy cannot be spawned at all ([`exit::SETUP`]: a support step
///   failed, and blaming [`exit::SPAWN`] would point the caller at *their* program,
///   which was never reached);
/// - the detached copy started but exited before reporting a started run — its own
///   reserved-band code is forwarded verbatim (see [`detached_start_failure`]), so a
///   missing program still reads as [`exit::SPAWN`] and an unusable container still
///   reads as [`exit::BACKEND`], just as they would have in the foreground.
///
/// Nothing is printed on success: the caller learns the run id from `run_started` in
/// the events file (whose presence this call guarantees), not from a stdout contract
/// this command has never had.
pub(super) fn start_detached(args: &RunArgs) -> Result<(), RunnerError> {
    // Resolve the id *here* so this process can watch for it. When `--run-id` was
    // given this is that exact value (already in the argv below); when it was not,
    // the generated id is passed to the detached copy explicitly, so the run is
    // never nameless — a detached run that generated its own id would be observable
    // only by whoever managed to read the events file first.
    let run_id = events::resolve_run_id(args.run_id.as_deref());

    // Create/truncate the events file before spawning anything, for two reasons.
    // First, fail-closed: an unwritable `--jsonl` is the caller's error and belongs
    // on *this* process's stderr and exit code, not buried in a detached copy that
    // has nowhere to report it. Second, it makes the handshake below unambiguous —
    // the file is empty when the detached runner starts appending to it, so a
    // `run_started` read out of it cannot be a leftover from an earlier run that
    // reused the same `--jsonl` path and `--run-id`. `Emitter::create` is exactly
    // what the run itself opens the stream with (the detached copy truncates it
    // again, harmlessly), so this shares its semantics rather than inventing a
    // second file-creation policy.
    drop(Emitter::create(&args.jsonl).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!(
                "could not open the JSONL events file `{}`: {err}",
                args.jsonl.display()
            ),
        )
    })?);

    let argv = detached_argv(
        std::env::args_os().skip(1),
        &run_id,
        args.run_id.is_none(),
        !args.no_echo,
    );
    let mut detached = spawn_detached(&argv).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not start a detached runner for run `{run_id}`: {err}"),
        )
    })?;

    await_started(&mut detached, &args.jsonl, &run_id)
}

/// Rewrite this invocation's own argv (everything after `argv[0]`) into the argv the
/// detached copy is spawned with.
///
/// Three edits, and deliberately no more:
/// - `--detach` is dropped, so the copy runs the ordinary foreground path;
/// - `--run-id <id>` is added when the caller did not give one (`add_run_id`), so the
///   run carries the id this process is about to watch for;
/// - `--no-echo` is added when the caller did not give one (`add_no_echo`), so the
///   detached runner takes the **existing** discarding-sink path rather than a
///   parallel "suppress output while detached" mechanism. It stays legal alongside
///   everything `--detach` permits (`--inherit-stdio`, the only flag `--no-echo`
///   conflicts with, is itself rejected with `--detach` at parse time).
///
/// **Why rewrite argv rather than re-render the parsed [`RunArgs`].** Re-rendering
/// would mean re-serializing every value the CLI parsed — durations, byte sizes, a
/// float CPU quota, `OsString` paths and environment values — and any drift between
/// what was parsed and what is re-printed would silently change the detached run.
/// Passing the caller's own tokens through verbatim cannot drift: the detached copy
/// parses exactly what this process parsed.
///
/// **Why the `--` scan is enough to find the flag.** Everything after `--` is the
/// child's command line and is never touched, so a program that legitimately takes a
/// literal `--detach` argument keeps it. Before `--`, a bare `--detach` token can only
/// be this flag: clap does not accept `-`-leading values for any of `run`'s options
/// (no `allow_hyphen_values` anywhere in [`crate::cli`]), so it could not be some
/// other flag's value, and `--detach=…` is rejected outright for a boolean flag.
fn detached_argv<I>(argv: I, run_id: &str, add_run_id: bool, add_no_echo: bool) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut rewritten = Vec::new();
    let mut added = false;
    for token in argv {
        if added {
            // Past `--`: the child's own command line, copied through untouched.
            rewritten.push(token);
            continue;
        }
        if token == OsStr::new("--") {
            // The added flags belong before the separator, where runner options live.
            push_added_flags(&mut rewritten, run_id, add_run_id, add_no_echo);
            added = true;
            rewritten.push(token);
            continue;
        }
        if token == OsStr::new(DETACH_FLAG) {
            continue;
        }
        rewritten.push(token);
    }
    if !added {
        // Unreachable through the real CLI: `run`'s command is `last = true`, so clap
        // rejects an invocation with no `--` long before this. Handled anyway, since
        // silently dropping `--run-id` would leave the run unwatchable.
        push_added_flags(&mut rewritten, run_id, add_run_id, add_no_echo);
    }
    rewritten
}

/// Append the flags [`detached_argv`] adds on the caller's behalf, in a fixed order.
fn push_added_flags(argv: &mut Vec<OsString>, run_id: &str, add_run_id: bool, add_no_echo: bool) {
    if add_run_id {
        argv.push(OsString::from(RUN_ID_FLAG));
        argv.push(OsString::from(run_id));
    }
    if add_no_echo {
        argv.push(OsString::from(NO_ECHO_FLAG));
    }
}

/// Spawn this binary again, detached from this process's session/console, on `argv`.
///
/// **Stdio is `null`, not inherited.** A detached runner outlives the caller's
/// terminal, pipe, or console; keeping their handles would leave it writing into
/// something that can vanish (and, on Windows, into a console it is explicitly being
/// created without). The child's *own* output is unaffected by this — it is read
/// through ProcessKit's pipes and discarded by the `--no-echo` sinks
/// [`detached_argv`] arms, while `--capture-dir` and `--idle-timeout` keep seeing
/// every byte.
///
/// **Environment and working directory are inherited**, exactly as they would be for
/// a foreground run: the detached copy must resolve a relative `--jsonl`,
/// `--capture-dir`, or `--cwd` to the same paths this process would have, and must
/// see the same environment the caller set up for the run
/// (`PROCESSKIT_CLI_REGISTRY_DIR` among it, or the run would register somewhere else).
///
/// Platform mechanisms:
/// - **Unix**: `setsid()` in the forked child, before `exec`. The copy becomes a
///   session leader with no controlling terminal, so a `Ctrl-C` or a hang-up in the
///   caller's session is not delivered to it. (It stays this process's direct child
///   until this process exits moments later, which is what makes the handshake below
///   able to observe a failed start; `init` reaps it afterwards.)
/// - **Windows**: `DETACHED_PROCESS`, so the copy is created without a console and
///   receives none of the console-control events (`Ctrl-C`/`Ctrl-Break`/close) the
///   caller's console delivers, plus [`disinherit_std_handles`] so the copy does not
///   keep the caller's own pipes open behind it. Note that a detached process still
///   belongs to any Job Object this process belongs to: a caller inside a
///   kill-on-close job cannot detach out of it (documented in `README.md`, "Detached
///   runs").
fn spawn_detached(argv: &[OsString]) -> std::io::Result<Child> {
    // The path to *this* binary, not `argv[0]`: the caller may have reached us
    // through `PATH` or a relative path, neither of which is guaranteed to still
    // resolve to this executable for the spawn below.
    let program = std::env::current_exe()?;
    let mut command = std::process::Command::new(program);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `pre_exec` runs in the forked child between `fork` and `exec`, where
        // only async-signal-safe work is allowed. This closure calls exactly one such
        // function (`setsid`) and, on failure, builds an `io::Error` from `errno`
        // without allocating. It touches no shared state and takes no locks.
        unsafe {
            command.pre_exec(|| {
                // Cannot fail here in practice: `setsid` only refuses for a process
                // that is already a process-group leader, which a freshly forked
                // child never is (its pid is new, so it leads no group). Checked
                // anyway — a silent failure would leave the "detached" run in the
                // caller's session, still exposed to its terminal signals.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::DETACHED_PROCESS);
        disinherit_std_handles();
    }

    command.spawn()
}

/// Windows: stop this process's own standard handles from being inherited by the
/// detached copy about to be spawned.
///
/// **Why this is required, not a tidy-up.** `CreateProcessW` is called with
/// `bInheritHandles = TRUE` whenever any standard handle is redirected (which
/// [`spawn_detached`] always does, to `null`), and that flag is all-or-nothing: *every*
/// inheritable handle in this process is duplicated into the child, including the
/// caller's stdout/stderr pipes — even though the child's own `STARTUPINFO` points at
/// `NUL`. A caller that captures output (`subprocess.run(capture_output=True)`,
/// `Command::output()`, a shell pipeline) reads until end-of-file, and end-of-file only
/// arrives when the *last* writer closes: a detached runner silently holding that pipe
/// open would make the caller block for the whole run, which is precisely what
/// `--detach` exists to avoid. Clearing `HANDLE_FLAG_INHERIT` on the three standard
/// handles leaves the detached copy with no reference to them, so the caller's capture
/// ends when this process exits, moments later.
///
/// Best-effort, and safe to be: this process is about to exit and spawns nothing else,
/// so the only observable effect of a failure is the blocking described above rather
/// than an incorrect run. A missing or invalid standard handle (a caller that is itself
/// detached) is skipped rather than treated as an error.
#[cfg(windows)]
fn disinherit_std_handles() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` takes a plain constant and returns a borrowed handle
        // this process already owns; it is not closed or otherwise consumed here.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `handle` was just obtained from the OS and checked for the two
        // "no such handle" sentinels; `SetHandleInformation` only edits that handle's
        // inheritance flag and cannot invalidate it.
        unsafe {
            windows_sys::Win32::Foundation::SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0)
        };
    }
}

/// Block until the detached runner has provably started the run — or until it is
/// clear that it never will.
///
/// The loop checks the events file **first** and the process **second**, and that
/// order is what makes the answer honest: a run can start and finish faster than one
/// poll interval, so a runner that has already exited is not evidence of a failed
/// start until the stream has been looked at again. Only when both are true — the
/// process is gone *and* no `run_started` was written — is the start a failure.
///
/// The two paths that give up while the detached copy is still alive (the observation
/// itself failed; [`DETACH_START_TIMEOUT`] elapsed) kill it before returning, via
/// [`abandon`]: reporting a failed start while leaving an unreported run behind would
/// be exactly the silent, unsupervised process this command exists to avoid.
fn await_started(detached: &mut Child, jsonl: &Path, run_id: &str) -> Result<(), RunnerError> {
    let deadline = Instant::now() + DETACH_START_TIMEOUT;
    loop {
        if run_started_recorded(jsonl, run_id) {
            return Ok(());
        }
        match detached.try_wait() {
            // Gone. One last look at the stream settles whether it left because the
            // run failed to start, or because the whole run was already over.
            Ok(Some(status)) => {
                return if run_started_recorded(jsonl, run_id) {
                    Ok(())
                } else {
                    Err(detached_start_failure(status, run_id, jsonl))
                };
            }
            Ok(None) => {}
            Err(err) => {
                // A start event may have landed after this iteration's initial
                // observation. Never abandon a run whose durable start is visible.
                if run_started_recorded(jsonl, run_id) {
                    return Ok(());
                }
                abandon(detached);
                return Err(RunnerError::new(
                    exit::SETUP,
                    format!(
                        "could not observe the detached runner for run `{run_id}`: {err} — \
                         the detached runner was killed rather than left unsupervised"
                    ),
                ));
            }
        }
        if Instant::now() >= deadline {
            // The stream may have gained `run_started` after this iteration's first
            // check. Do not kill a run whose durable start is now observable.
            if run_started_recorded(jsonl, run_id) {
                return Ok(());
            }
            abandon(detached);
            return Err(RunnerError::new(
                exit::SETUP,
                format!(
                    "the detached runner for run `{run_id}` did not report a started run within \
                     {} — it was killed rather than left unsupervised; see `{}` for whatever it \
                     did write",
                    format_duration(DETACH_START_TIMEOUT),
                    jsonl.display()
                ),
            ));
        }
        sleep(DETACH_POLL_INTERVAL);
    }
}

/// Whether the run's `run_started` event is readable in the events file yet.
///
/// Reads the whole (still tiny) file and looks for the one line that names this run:
/// the stream is written line-by-line with a flush per event, so a partially written
/// last line is possible and is simply not valid JSON yet — such a line is skipped and
/// re-read on the next poll rather than treated as an answer. A missing or unreadable
/// file is "not yet", never an error: the detached copy truncates and reopens the same
/// path as it starts.
fn run_started_recorded(jsonl: &Path, run_id: &str) -> bool {
    let Ok(stream) = std::fs::read_to_string(jsonl) else {
        return false;
    };
    stream
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|event| event["event"] == "run_started" && event["run_id"] == run_id)
}

/// The error for a detached copy that exited without ever reporting a started run.
///
/// **The code is forwarded, not minted.** A failure this early is a failure the
/// foreground path already has a code for — a program that could not be spawned is
/// [`exit::SPAWN`], a container that could not be created is [`exit::BACKEND`], an
/// unwritable `--capture-dir` is [`exit::SETUP`] — and the detached copy exited with
/// exactly that code. Passing it through keeps `run --detach`'s failures identical to
/// `run`'s, so a caller needs no second table to read them, and spends no slot from
/// the reserved band's free range on a distinction that does not exist.
///
/// Anything *outside* the reserved band is reported as [`exit::SETUP`] instead of
/// forwarded, and that includes a `0`: no path in [`super::launch::run_async`] can reach a successful
/// exit without having written `run_started` first, so such a status is not a run
/// result to relay but an unexplained death of the runner — and relaying `0` would
/// report a start that provably did not happen.
///
/// **The machine-readable `kind` is *not* forwarded on the same terms.** The code is
/// a number produced by a *different process*, and — as [`spawn_detached`] already
/// acknowledges — possibly by a different build, if the binary on disk was replaced
/// between this process spawning the copy and the copy reaching `exec`. Reading such
/// a number through this build's own table would let a code no `run` can mint arrive
/// dressed as a real verdict about this run (a relayed `112` would claim
/// `kind: "wait_timeout"`, `retryable: true` — "the run is still going, wait again" —
/// for a run that never started, and would contradict
/// `fixtures/schema/cli/error.schema.json`'s own `wait_timeout ⇒ operation: "wait"`
/// conditional, letting this binary print an object its published schema rejects).
/// [`relayed_kind`] therefore names only the codes `run` itself produces and leaves
/// every other reserved-band number as [`ErrorKind::Unknown`], which exists for
/// exactly this "read `code`, not `kind`" case. The exit code is still forwarded
/// verbatim either way: it is the caller's contract, and unlike the kind it claims
/// nothing beyond the number the copy returned.
fn detached_start_failure(status: ExitStatus, run_id: &str, jsonl: &Path) -> RunnerError {
    let band = exit::RUNNER_RANGE_START..=exit::RUNNER_RANGE_END;
    let reserved = status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .filter(|code| band.contains(code));
    match reserved {
        Some(code) => {
            let kind = relayed_kind(code);
            let message = if kind == ErrorKind::Unknown {
                // Deliberately not "the same code it would have reported in the
                // foreground": this build's `run` reports no such code, so saying so
                // would be the same borrowed claim the kind refuses to make.
                format!(
                    "run `{run_id}` did not start: the detached runner exited with the \
                     reserved-band code {code}, which this build's own `run` never reports — the \
                     binary on disk may have been replaced between the spawn and the exec; see \
                     `{}`",
                    jsonl.display()
                )
            } else {
                format!(
                    "run `{run_id}` did not start: the detached runner failed with the same code \
                     it would have reported in the foreground ({code}); see `{}`",
                    jsonl.display()
                )
            };
            RunnerError::new(code, message).with_kind(kind)
        }
        None => RunnerError::new(
            exit::SETUP,
            format!(
                "run `{run_id}` did not start: the detached runner exited ({status}) without \
                 recording a started run; see `{}`",
                jsonl.display()
            ),
        ),
    }
}

/// The [`ErrorKind`] a relayed reserved-band code is allowed to carry.
///
/// A relayed code is only as trustworthy as the process that produced it, and that
/// process is not this one. The codes below are the ones **`run` itself mints** —
/// [`exit::SPAWN`], [`exit::BACKEND`], [`exit::SETUP`], [`exit::INTERNAL`] and the
/// runner-imposed endings ([`exit::TIMEOUT`], [`exit::CANCELLED`],
/// [`exit::CONTROL_CANCELLED`], [`exit::CONTROL_KILLED`], [`exit::OUTPUT_OVERFLOW`]),
/// plus [`exit::USAGE`], which a respawned copy reports when it will not parse the
/// argv this process handed it. For those, this build's own table names the same
/// failure the foreground path would have named, so [`ErrorKind::for_code`] is
/// honest. (The endings imply a `run_started` was already written and so are not
/// expected on this path at all; they are listed because they are `run`'s own codes,
/// not because relaying one is a normal outcome.)
///
/// Every **other** reserved-band number gets [`ErrorKind::Unknown`], and there are
/// two ways to be one: a code no build assigns yet (`105`, `116`-`119` — already
/// [`ErrorKind::for_code`]'s answer), and a code this build assigns to a *different
/// subcommand* — [`exit::PROBE_INCOMPATIBLE`] (110), [`exit::WAIT_TIMEOUT`] (112),
/// [`exit::EVENTS_INVALID`] (114), [`exit::NOT_A_MEMBER`] (115), minted only by
/// `probe`/`wait`/`events --validate`/`attest`
/// and unreachable from `run`. Naming the second group from this build's table would
/// invent a verdict about a run out of a foreign build's number; naming nothing is
/// the honest answer, and the numeric `code` still reaches the caller untouched.
fn relayed_kind(code: u8) -> ErrorKind {
    match code {
        exit::USAGE
        | exit::SPAWN
        | exit::BACKEND
        | exit::INTERNAL
        | exit::TIMEOUT
        | exit::CANCELLED
        | exit::CONTROL_CANCELLED
        | exit::CONTROL_KILLED
        | exit::OUTPUT_OVERFLOW
        | exit::SETUP => ErrorKind::for_code(code),
        _ => ErrorKind::Unknown,
    }
}

/// Kill and reap a detached copy this process is about to stop vouching for.
///
/// Best-effort by necessity, and honest about its reach: this signals the runner
/// process itself, so whether a child it had already spawned goes with it is the
/// platform's abrupt-owner-death contract, not a promise made here — the same
/// `whole_tree` / `direct_child_only` / `none` tri-state every `run_started` event
/// reports as `abrupt_cleanup` (see [`events::abrupt_cleanup_str`]). Killing by handle
/// (never by PID) means this can never hit a recycled PID.
///
/// A copy killed after it had already published its registry record leaves that record
/// behind as an ordinary **stale entry** — detectable as such (its liveness lock dies
/// with it) and reaped by `prune` like any other abrupt death, never mistaken for a live
/// run. Nothing special is needed, or attempted, here.
fn abandon(detached: &mut Child) {
    let _ = detached.kill();
    let _ = detached.wait();
}

#[cfg(test)]
mod tests {
    use crate::events::Event;

    use super::*;

    /// Build an argv vector from string literals, as `std::env::args_os` would
    /// hand it to [`detached_argv`] (everything after `argv[0]`).
    fn argv(tokens: &[&str]) -> Vec<OsString> {
        tokens.iter().map(OsString::from).collect()
    }

    /// Render a rewritten argv for comparison/assertion.
    fn rendered(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|token| token.to_string_lossy().into_owned())
            .collect()
    }

    /// The rewrite is exactly three edits: drop `--detach`, add the resolved
    /// `--run-id`, add `--no-echo` — all *before* the `--` separator, with every
    /// other token of the caller's own command line preserved in order.
    #[test]
    fn detached_argv_drops_the_flag_and_names_the_run() {
        let rewritten = detached_argv(
            argv(&[
                "run",
                "--jsonl",
                "events.jsonl",
                "--detach",
                "--timeout",
                "5s",
                "--",
                "sleep",
                "30",
            ]),
            "run-42",
            true,
            true,
        );
        assert_eq!(
            rendered(&rewritten),
            vec![
                "run",
                "--jsonl",
                "events.jsonl",
                "--timeout",
                "5s",
                "--run-id",
                "run-42",
                "--no-echo",
                "--",
                "sleep",
                "30",
            ],
            "the detached copy runs the caller's own command line, minus --detach \
             and plus the two flags it needs"
        );
    }

    /// What the caller already asked for is never duplicated: an explicit
    /// `--run-id` stays where it was (and is the id the handshake watches for), and
    /// an explicit `--no-echo` is not added a second time.
    #[test]
    fn detached_argv_does_not_duplicate_flags_the_caller_gave() {
        let rewritten = detached_argv(
            argv(&[
                "run",
                "--run-id",
                "build-42",
                "--no-echo",
                "--detach",
                "--jsonl",
                "events.jsonl",
                "--",
                "cargo",
                "build",
            ]),
            "build-42",
            false,
            false,
        );
        let rendered = rendered(&rewritten);
        assert_eq!(
            rendered,
            vec![
                "run",
                "--run-id",
                "build-42",
                "--no-echo",
                "--jsonl",
                "events.jsonl",
                "--",
                "cargo",
                "build",
            ]
        );
        assert_eq!(
            rendered.iter().filter(|t| *t == "--run-id").count(),
            1,
            "an explicit --run-id is not passed twice: {rendered:?}"
        );
        assert_eq!(
            rendered.iter().filter(|t| *t == "--no-echo").count(),
            1,
            "an explicit --no-echo is not passed twice: {rendered:?}"
        );
    }

    /// The child's own command line is off limits: a program that legitimately takes
    /// a literal `--detach` argument keeps it, because only tokens *before* the `--`
    /// separator are rewritten. Without this, detaching would silently change what
    /// the caller asked to run.
    #[test]
    fn detached_argv_never_rewrites_the_child_command_line() {
        let rewritten = detached_argv(
            argv(&[
                "run",
                "--jsonl",
                "events.jsonl",
                "--detach",
                "--",
                "my-tool",
                "--detach",
                "--no-echo",
            ]),
            "run-7",
            true,
            true,
        );
        assert_eq!(
            rendered(&rewritten),
            vec![
                "run",
                "--jsonl",
                "events.jsonl",
                "--run-id",
                "run-7",
                "--no-echo",
                "--",
                "my-tool",
                "--detach",
                "--no-echo",
            ],
            "everything after `--` is the child's, verbatim"
        );
    }

    /// Build an [`ExitStatus`] carrying `code`, the way the platform reports a
    /// process that exited with it.
    fn exited_with(code: i32) -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            // A POSIX wait status: the exit code sits in the high byte.
            ExitStatus::from_raw(code << 8)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code as u32)
        }
    }

    /// A detached copy that died before reporting a started run surfaces **its own**
    /// reserved-band code, so `run --detach`'s start failures read exactly like
    /// `run`'s (K-047: reuse the existing code, do not mint a new one) — and, for a
    /// code `run` genuinely mints, the same machine-readable `kind` the foreground
    /// path would have reported.
    #[test]
    fn a_failed_detached_start_forwards_the_runners_own_reserved_code() {
        let jsonl = Path::new("events.jsonl");
        for code in [
            exit::USAGE,
            exit::SPAWN,
            exit::BACKEND,
            exit::SETUP,
            exit::INTERNAL,
        ] {
            let err = detached_start_failure(exited_with(i32::from(code)), "run-9", jsonl);
            assert_eq!(
                err.code(),
                code,
                "a detached start failure keeps the code the run itself reported"
            );
            assert_eq!(
                err.kind(),
                ErrorKind::for_code(code),
                "a code `run` itself mints keeps the kind the foreground path would report"
            );
            assert!(
                err.to_string().contains("run-9"),
                "the message names the run: {err}"
            );
        }
    }

    /// A relayed code that **`run` cannot produce** is reported as `unknown`, never
    /// as the kind this build happens to give that number for some other subcommand.
    ///
    /// The relayed status comes from a re-exec'd copy that can be a different build
    /// (see `spawn_detached`), so borrowing the meaning would (a) state a materially
    /// false verdict — a relayed `112` would claim `wait_timeout`, the one
    /// `retryable: true` kind, meaning "the run is still going, wait again", for a run
    /// that never started — and (b) print an object
    /// `fixtures/schema/cli/error.schema.json` itself rejects, since it requires
    /// `wait_timeout ⇒ operation: "wait"` while this failure's operation is `run`.
    /// The number is still forwarded: only the *claim about its meaning* is withheld.
    #[test]
    fn a_relayed_code_run_cannot_mint_is_named_unknown_rather_than_borrowed() {
        let jsonl = Path::new("events.jsonl");
        for code in [
            exit::PROBE_INCOMPATIBLE,
            exit::WAIT_TIMEOUT,
            exit::EVENTS_INVALID,
            exit::NOT_IMPLEMENTED,
            exit::RUNNER_RANGE_END,
        ] {
            let err = detached_start_failure(exited_with(i32::from(code)), "run-9", jsonl);
            assert_eq!(
                err.code(),
                code,
                "the relayed code itself is still forwarded verbatim"
            );
            assert_eq!(
                err.kind(),
                ErrorKind::Unknown,
                "code {code} is not one `run` mints, so this build must not name it"
            );
            assert!(
                err.to_string().contains("never reports"),
                "the message says the code is not this build's own: {err}"
            );
        }
    }

    /// The two halves of that rule are exhaustive over the reserved band and agree
    /// with `run`'s own code set: every band member is either a code `run` mints
    /// (named after the foreground path) or `unknown`, and nothing else.
    #[test]
    fn every_reserved_code_is_either_runs_own_or_unknown() {
        let runs_own = [
            exit::USAGE,
            exit::SPAWN,
            exit::BACKEND,
            exit::INTERNAL,
            exit::TIMEOUT,
            exit::CANCELLED,
            exit::CONTROL_CANCELLED,
            exit::CONTROL_KILLED,
            exit::OUTPUT_OVERFLOW,
            exit::SETUP,
        ];
        for code in exit::RUNNER_RANGE_START..=exit::RUNNER_RANGE_END {
            let kind = relayed_kind(code);
            if runs_own.contains(&code) {
                assert_eq!(
                    kind,
                    ErrorKind::for_code(code),
                    "a code `run` mints keeps its own name: {code}"
                );
            } else {
                assert_eq!(
                    kind,
                    ErrorKind::Unknown,
                    "a code `run` cannot mint is unnamed here: {code}"
                );
            }
        }
        // `CONTROL` (103) is the one assigned code deliberately absent from the set
        // above: `run` never speaks to a control plane as a client, so a relayed 103
        // would be a foreign build's verdict about someone else's target.
        assert_eq!(relayed_kind(exit::CONTROL), ErrorKind::Unknown);
    }

    /// A status that is *not* a reserved-band code is not relayed as one — including
    /// a `0`, which would otherwise report a start that provably never happened (no
    /// `run_started` was written). Both become a `SETUP` failure that says so.
    #[test]
    fn an_unexplained_detached_exit_is_never_reported_as_a_started_run() {
        let jsonl = Path::new("events.jsonl");
        for code in [0, 1, 42, 200] {
            let err = detached_start_failure(exited_with(code), "run-9", jsonl);
            assert_eq!(
                err.code(),
                exit::SETUP,
                "an out-of-band status ({code}) is a support failure, not a run result"
            );
            assert!(
                err.code() != 0,
                "a failed start never exits 0, whatever the detached copy did"
            );
            assert!(
                err.to_string().contains("without recording a started run"),
                "the message states what was missing: {err}"
            );
        }
    }

    /// The handshake's single observation: a `run_started` line naming *this* run.
    /// Anything else — no file, an empty file, another run's stream, a half-written
    /// line — reads as "not started yet", never as a start.
    #[test]
    fn run_started_is_recognized_only_for_this_run_and_only_when_complete() {
        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-run-unit-detach-handshake-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let jsonl = dir.join("events.jsonl");

        // Nothing there yet: the detached copy has not even opened the stream.
        assert!(
            !run_started_recorded(&jsonl, "run-1"),
            "a missing events file is not a started run"
        );

        // Opened and truncated, still empty.
        let mut emitter = Emitter::create(&jsonl).expect("create the events file");
        assert!(
            !run_started_recorded(&jsonl, "run-1"),
            "an empty events file is not a started run"
        );

        // A real `run_started`, written by the real emitter — for a *different* run.
        emitter.emit(&Event::RunStarted {
            run_id: "run-2".to_string(),
            labels: std::collections::BTreeMap::new(),
            root_pid: Some(4242),
            mechanism: "job_object",
            abrupt_cleanup: "whole_tree",
            cwd: None,
            command: events::CommandInfo::for_argv(&[OsString::from("true")], false),
        });
        assert!(
            !run_started_recorded(&jsonl, "run-1"),
            "another run's start is not this run's start"
        );
        assert!(
            run_started_recorded(&jsonl, "run-2"),
            "the run named by the event is started"
        );

        // A half-written trailing line (the emitter flushes per event, but a reader
        // can still catch a partial write) is skipped, not misread.
        let partial = dir.join("partial.jsonl");
        std::fs::write(
            &partial,
            "{\"schema_version\":1,\"event\":\"run_started\",\"run_i",
        )
        .expect("write a truncated line");
        assert!(
            !run_started_recorded(&partial, "run-1"),
            "an incomplete line is not an answer"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
