//! Runner-own exit codes — the exit-code half of processkit-cli's public
//! compatibility surface.
//!
//! `AGENTS.md` fixes that surface as *CLI flags + exit-code ranges +
//! `schema_version`* and requires the runner's own failures to use a distinct,
//! documented range so a child's exit code is never lost or aliased. This module
//! is the in-code mirror of that contract; `docs/exit-codes.md` is the normative
//! document consumers (Orchestra, adapters) pin against.
//!
//! A successful run forwards the *child's* exit code verbatim (implemented in a
//! later task). The codes below are minted only when the runner itself fails
//! before or around the child. Because a child could, in principle, also exit
//! with a number inside this band, the authoritative disambiguator is always the
//! `runner_exit` JSONL event — the numeric code is the best-effort signal for
//! shells that cannot read the event stream.

/// Inclusive lower bound of the runner-own exit-code band. Read by the preflight
/// probe (`src/probe.rs`) so a consumer can pin the reserved band before a launch.
pub const RUNNER_RANGE_START: u8 = 100;
/// Inclusive upper bound of the runner-own exit-code band. Codes above the ones
/// assigned below are reserved for future runner-own conditions. Read by the
/// preflight probe (`src/probe.rs`) so a consumer can pin the reserved band.
pub const RUNNER_RANGE_END: u8 = 119;

/// Invalid command line: unknown flag, missing required option, malformed value,
/// or an unrecognized subcommand form.
pub const USAGE: u8 = 100;
/// The target program could not be started (not found, not executable, bad cwd,
/// permission denied) — a runner failure that happened before the child ran.
#[allow(dead_code)] // Minted once `run` spawns a child (T-002+).
pub const SPAWN: u8 = 101;
/// ProcessKit backend/containment failure: the kernel container, job object, IPC
/// endpoint, or run registry could not be established.
#[allow(dead_code)] // Minted once the runner talks to the ProcessKit backend (T-002+).
pub const BACKEND: u8 = 102;
/// A control-plane command (`inspect`/`cancel`/`kill`) could not reach its target
/// run: no such run id, a stale/dead registry entry, or an IPC failure.
#[allow(dead_code)] // Minted once the control plane is wired (T-004+).
pub const CONTROL: u8 = 103;
/// Unexpected runner fault — an invariant was violated. Reported with this code
/// instead of panicking, so callers still observe a runner-range exit.
#[allow(dead_code)] // Minted as the runner grows fallible paths (T-002+).
pub const INTERNAL: u8 = 104;
/// Retired. Formerly minted for a defined-but-not-yet-built code path while the
/// runner was under construction; every subcommand is now implemented, so no
/// active path mints this code anymore. Per the stability contract
/// (`docs/exit-codes.md`, "Stability"), a retired code is never reused for a
/// different meaning — this number stays permanently reserved.
#[allow(dead_code)] // Retired: kept only so the number stays reserved, never reused.
pub const NOT_IMPLEMENTED: u8 = 105;
/// The run exceeded its `--timeout`: the runner enforced the deadline and tore
/// the process tree down. A **runner-imposed outcome**, not a child exit — the
/// child did not choose to stop — so it takes a reserved-band code rather than a
/// forwarded child code. Distinct from [`CANCELLED`] and from any child result.
pub const TIMEOUT: u8 = 106;
/// The run was cancelled interactively (`Ctrl-C`): the runner tore the process
/// tree down. Like [`TIMEOUT`] this is a runner-imposed outcome, kept in the
/// reserved band so it is never mistaken for a child's own exit — and distinct
/// from a timeout, so a caller can tell "I interrupted it" from "it ran too long".
pub const CANCELLED: u8 = 107;
/// The run was cancelled by a control-plane `cancel` command: a client reached the
/// live runner over its local control channel and asked it to end the run, so the
/// runner ran the **same** soft-stop → grace → hard-kill teardown as a `Ctrl-C`
/// cancel — only triggered over the network instead of by a local signal. A
/// runner-imposed outcome, distinct from [`CANCELLED`] (a local `Ctrl-C`) so a
/// caller can tell "a control client cancelled it" apart from "the operator pressed
/// Ctrl-C", and distinct from [`TIMEOUT`] and from any child result.
pub const CONTROL_CANCELLED: u8 = 108;
/// The run was killed by a control-plane `kill` command: a client asked the live
/// runner to hard-kill the whole tree **immediately** — no soft stop and no grace.
/// Kept in the reserved band and distinct from every other runner-imposed ending
/// ([`TIMEOUT`], [`CANCELLED`], [`CONTROL_CANCELLED`]) so "it was force-killed by
/// command" is never confused with a graceful cancel or a child's own exit.
pub const CONTROL_KILLED: u8 = 109;
/// The **preflight probe** (`processkit-cli probe`) found this binary's
/// compatibility surface does not satisfy the requirements a consumer asked it to
/// verify (a `--require-*` check failed). This is **not** a run outcome — no child
/// is ever spawned by a probe — but a *pre-launch* verdict: the launcher contract
/// (`docs/env-launch.md`) is fail-closed, so an incompatible binary must be
/// reported with a distinct, reserved code rather than silently used. It takes the
/// next free code after the control-plane endings so no existing assignment shifts
/// or changes meaning. See [`crate::probe`].
pub const PROBE_INCOMPATIBLE: u8 = 110;

/// A runner-own failure carrying the exit code it should surface and a
/// human-readable message. Distinct from a child's exit — a child's code is
/// forwarded verbatim and never wrapped in this type.
#[derive(Debug)]
pub struct RunnerError {
    code: u8,
    message: String,
}

impl RunnerError {
    /// Construct a runner error with an explicit code from the runner-own band.
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The runner-own exit code this error should surface to the process's caller.
    pub fn code(&self) -> u8 {
        self.code
    }
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RunnerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigned_codes_stay_within_the_runner_band() {
        for code in [
            USAGE,
            SPAWN,
            BACKEND,
            CONTROL,
            INTERNAL,
            NOT_IMPLEMENTED,
            TIMEOUT,
            CANCELLED,
            CONTROL_CANCELLED,
            CONTROL_KILLED,
            PROBE_INCOMPATIBLE,
        ] {
            assert!(
                (RUNNER_RANGE_START..=RUNNER_RANGE_END).contains(&code),
                "exit code {code} escaped the runner band {RUNNER_RANGE_START}..={RUNNER_RANGE_END}"
            );
        }
    }

    #[test]
    fn timeout_and_cancelled_are_distinct_and_distinct_from_the_other_codes() {
        // The whole point of the runner-imposed outcomes is that a caller can tell
        // them apart — from each other and from every other runner-own code.
        let all = [
            USAGE,
            SPAWN,
            BACKEND,
            CONTROL,
            INTERNAL,
            NOT_IMPLEMENTED,
            TIMEOUT,
            CANCELLED,
            CONTROL_CANCELLED,
            CONTROL_KILLED,
            PROBE_INCOMPATIBLE,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two runner-own codes collided on {a}");
            }
        }
    }

    #[test]
    fn the_four_runner_imposed_endings_are_all_distinct() {
        // A caller must be able to tell every runner-imposed ending apart by code:
        // a --timeout, a Ctrl-C, a control-plane cancel, and a control-plane kill.
        let endings = [TIMEOUT, CANCELLED, CONTROL_CANCELLED, CONTROL_KILLED];
        for (i, a) in endings.iter().enumerate() {
            for b in &endings[i + 1..] {
                assert_ne!(a, b, "two runner-imposed endings collided on {a}");
            }
        }
    }
}
