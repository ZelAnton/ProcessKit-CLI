//! The runner-imposed deadlines and the local stop-signal listeners: every arm of
//! [`super::launch::run_async`]'s race except the child's own exit and the
//! control-plane command.
//!
//! Two deadlines ([`deadline`], [`idle_deadline`]) and one cancel arm
//! ([`wait_for_cancel_signal`]) that fans out, per platform, over `Ctrl-C`, the
//! Unix `SIGTERM`/`SIGHUP` signals, and the Windows console-control events —
//! plus [`effective_grace_for`], the one place a platform's own termination
//! deadline is allowed to bound the operator's `--grace`. Every listener here
//! degrades to "this trigger is not handled" (an arm that never resolves) rather
//! than failing an otherwise-healthy run, and none of them installs a disposition
//! the environment deliberately neutralized.

use std::time::Duration;

use crate::capture::IdleClock;

use super::CancelSignal;

/// The runner-imposed whole-run deadline: sleep `limit`, or (with no `--timeout`)
/// never resolve, so the race falls through to the other arms.
pub(super) async fn deadline(limit: Option<Duration>) {
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
pub(super) async fn idle_deadline(idle: Option<Duration>, clock: &IdleClock) {
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
/// one arrived. This is the single cancel arm of [`super::launch::run_async`]'s race, so every signal
/// it listens for takes the very same teardown (soft stop → `--grace` → hard kill) and
/// the same reserved [`crate::exit::CANCELLED`] code.
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
pub(super) async fn wait_for_cancel_signal() -> CancelSignal {
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
    #[cfg(unix)]
    if signal_is_ignored(libc::SIGINT) {
        return std::future::pending().await;
    }

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
/// Applied to every Unix stop-signal listener, including `SIGINT`: the guard exists
/// to avoid changing how an already-neutralized signal behaves when a shell or
/// supervisor deliberately launched the runner with that disposition.
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
pub(super) fn effective_grace_for(
    signal: CancelSignal,
    grace: Option<Duration>,
) -> Option<Duration> {
    match signal {
        CancelSignal::CtrlClose => grace.map(|grace| grace.min(CTRL_CLOSE_GRACE_BUDGET)),
        _ => grace,
    }
}

#[cfg(not(windows))]
pub(super) fn effective_grace_for(
    signal: CancelSignal,
    grace: Option<Duration>,
) -> Option<Duration> {
    let _ = signal;
    grace
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
