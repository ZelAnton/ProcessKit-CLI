//! Arguments for the `doctor` subcommand (the side-effecting runtime
//! qualification itself lives in [`crate::doctor`]).

use std::time::Duration;

use clap::Args;

use super::parse::parse_positive_duration;

/// `doctor [--json] [--timeout <duration>] [--check-resource-controller]
/// [--require-mechanism <name>] [--require-abrupt-cleanup <level>]
/// [--require-resource-controller] | doctor --scratch-child <duration>`
///
/// The **runtime qualification** of the host this binary is installed on: unlike
/// [`ProbeArgs`](super::ProbeArgs), which reports what this *binary* is without
/// touching anything, `doctor` actually performs a bounded scratch run — it creates
/// the per-user registry, stands up a container, binds the local control transport,
/// drives an `inspect`/`cancel` round-trip against its own scratch run, waits for it
/// to end, and confirms every artifact is gone. The report is a list of the facts it
/// observed, never one boolean.
///
/// The `--require-*` flags gate the **exit code** only: the report always carries the
/// facts as observed, whether or not a requirement was asked for, so a caller that
/// pins one host property still learns everything about the rest (see
/// [`crate::doctor`]).
///
/// `--scratch-child` is a separate, report-replacing contract on the same
/// subcommand — it is what `doctor` launches *inside* its scratch container — and
/// clap rejects it outright (`USAGE`, 100) in combination with any other flag here,
/// rather than silently skipping the qualification a caller asked for (see its own
/// doc comment below, and `src/cli/probe.rs`'s `--print-schema` for the precedent).
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit the report as one JSON line
    /// (`fixtures/schema/cli/doctor.schema.json`) instead of the human-readable
    /// rendering. Optional, unlike `probe --json`: `doctor` is run by an operator
    /// diagnosing a host at least as often as by an adapter qualifying one, so it
    /// has a readable form too — and both carry the same facts.
    #[arg(long)]
    pub json: bool,

    /// Give the whole qualification at most this long (default `30s`). The scratch
    /// run and its scratch child are bounded by the same budget, so a `doctor` that
    /// is itself killed still leaves nothing behind. A host that cannot finish
    /// inside the budget is reported as unqualified, with the per-phase timings
    /// naming which phase ran out of time — never a generic hang.
    #[arg(
        long,
        value_name = "duration",
        default_value = "30s",
        value_parser = parse_positive_duration
    )]
    pub timeout: Duration,

    /// Also qualify the **resource controller**: run a second, separate scratch run
    /// that asks for a whole-tree process cap, and report whether this host's
    /// containment mechanism could enforce it. Off by default — a host that never
    /// uses `run --max-memory`/`--max-processes`/`--cpu-quota` does not need the
    /// controller, and probing for it costs a second scratch run.
    ///
    /// Isolated on purpose: its failure never masks the mandatory phases, which have
    /// already completed against their own scratch run by the time this one starts.
    #[arg(long)]
    pub check_resource_controller: bool,

    /// Require the containment mechanism this host selects to be exactly `<name>`
    /// (`job_object` / `cgroup_v2` / `process_group` / `process_reaper` / `unknown`,
    /// the same vocabulary the JSONL `run_started` event publishes —
    /// `docs/schema.md`). Compared for exact equality; a host that reports anything else is unqualified
    /// (`HOST_UNQUALIFIED`, 116) with a mismatch naming both the required and the
    /// observed value. The report still carries the observed mechanism either way.
    #[arg(long, value_name = "name")]
    pub require_mechanism: Option<String>,

    /// Require the abrupt-cleanup guarantee this host offers to be exactly `<level>`
    /// (`whole_tree` / `direct_child_only` / `none`, the same vocabulary the JSONL
    /// `run_started` event publishes — `docs/schema.md`). This is what survives a
    /// runner killed without destructors, so an adapter that depends on whole-tree
    /// reaping pins it here.
    ///
    /// Exact equality, deliberately **not** an "at least this strong" comparison:
    /// the three values are platform facts (`docs/platform-support.md`), and this
    /// project publishes no ordering between them that a caller could rely on.
    #[arg(long, value_name = "level")]
    pub require_abrupt_cleanup: Option<String>,

    /// Require the resource-controller check to have found the controller
    /// available. Requires `--check-resource-controller`: a requirement about a fact
    /// this invocation never observed could only ever be answered by guessing, so
    /// clap refuses the pair as a usage error (100) instead — the same fail-closed
    /// stance every other requirement here takes.
    #[arg(long, requires = "check_resource_controller")]
    pub require_resource_controller: bool,

    /// **Be** the harmless scratch child instead of qualifying anything: sleep for at
    /// most `<duration>`, write nothing, read nothing, contact nothing, and exit `0`.
    ///
    /// This is the process `doctor` launches inside its scratch container — the
    /// pattern `src/bin/e2e_helper.rs` established for the end-to-end tier, applied
    /// to the shipped binary, which is what lets `doctor` contain *this binary's own
    /// code* rather than some program it found on the host. It is published rather
    /// than hidden precisely because that claim is worth being able to check: a
    /// reader can run it and see for themselves that it does nothing.
    ///
    /// Bounded like every `e2e_helper` mode, so a `doctor` that is killed mid-run
    /// leaves a child that self-terminates rather than one that lingers.
    ///
    /// **Cannot be combined with any other `doctor` flag**: clap rejects that
    /// combination as an ordinary usage error (exit `100`) rather than letting a
    /// requested qualification be silently replaced by a sleep, the same structural
    /// refusal `probe --print-schema` uses for the same reason (`src/cli/probe.rs`).
    #[arg(
        long,
        value_name = "duration",
        value_parser = parse_positive_duration,
        conflicts_with_all = [
            "json",
            "timeout",
            "check_resource_controller",
            "require_mechanism",
            "require_abrupt_cleanup",
            "require_resource_controller"
        ]
    )]
    pub scratch_child: Option<Duration>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser;

    use crate::cli::{Cli, Command};

    fn doctor_args(argv: &[&str]) -> crate::cli::DoctorArgs {
        let mut full = vec!["processkit-cli", "doctor"];
        full.extend_from_slice(argv);
        let cli = Cli::try_parse_from(&full).unwrap_or_else(|err| panic!("{full:?}: {err}"));
        match cli.command {
            Command::Doctor(args) => args,
            _ => panic!("expected the doctor subcommand"),
        }
    }

    #[test]
    fn a_bare_doctor_is_valid_and_carries_the_default_budget() {
        let args = doctor_args(&[]);
        assert!(!args.json, "the human rendering is the default");
        assert_eq!(
            args.timeout,
            Duration::from_secs(30),
            "the whole qualification is bounded even when nothing was asked for"
        );
        assert!(!args.check_resource_controller);
        assert!(args.require_mechanism.is_none());
        assert!(args.require_abrupt_cleanup.is_none());
        assert!(!args.require_resource_controller);
        assert!(args.scratch_child.is_none());
    }

    #[test]
    fn the_requirement_flags_parse_and_are_captured() {
        let args = doctor_args(&[
            "--json",
            "--timeout",
            "5s",
            "--check-resource-controller",
            "--require-mechanism",
            "job_object",
            "--require-abrupt-cleanup",
            "whole_tree",
            "--require-resource-controller",
        ]);
        assert!(args.json);
        assert_eq!(args.timeout, Duration::from_secs(5));
        assert!(args.check_resource_controller);
        assert_eq!(args.require_mechanism.as_deref(), Some("job_object"));
        assert_eq!(args.require_abrupt_cleanup.as_deref(), Some("whole_tree"));
        assert!(args.require_resource_controller);
    }

    /// A requirement about the optional check cannot be asked for without the check
    /// itself: answering it would mean guessing at a fact this invocation never
    /// observed, so the pair is refused at parse time.
    #[test]
    fn requiring_the_resource_controller_requires_checking_it() {
        assert!(
            Cli::try_parse_from(["processkit-cli", "doctor", "--require-resource-controller"])
                .is_err(),
            "the requirement needs the check that produces the fact it is about"
        );
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "doctor",
                "--check-resource-controller",
                "--require-resource-controller",
            ])
            .is_ok(),
            "with the check present the pair is the intended form"
        );
    }

    /// `--scratch-child` conflicts with every other flag at the clap level:
    /// combining them is an ordinary usage error, never a qualification silently
    /// replaced by a sleep (the `probe --print-schema` precedent, K-076).
    #[test]
    fn scratch_child_conflicts_with_every_other_doctor_flag() {
        for other in [
            vec!["--json"],
            vec!["--timeout", "5s"],
            vec!["--check-resource-controller"],
            vec!["--require-mechanism", "job_object"],
            vec!["--require-abrupt-cleanup", "whole_tree"],
            vec![
                "--check-resource-controller",
                "--require-resource-controller",
            ],
        ] {
            let mut argv = vec!["processkit-cli", "doctor", "--scratch-child", "1s"];
            argv.extend_from_slice(&other);
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?} must be refused rather than silently accepted"
            );
        }
        assert!(
            Cli::try_parse_from(["processkit-cli", "doctor", "--scratch-child", "1s"]).is_ok(),
            "on its own it is the intended form"
        );
    }

    #[test]
    fn doctor_rejects_a_malformed_or_zero_budget() {
        for bad in ["not-a-duration", "0s", "-1s"] {
            assert!(
                Cli::try_parse_from(["processkit-cli", "doctor", "--timeout", bad]).is_err(),
                "`--timeout {bad}` must fail at parse time"
            );
            assert!(
                Cli::try_parse_from(["processkit-cli", "doctor", "--scratch-child", bad]).is_err(),
                "`--scratch-child {bad}` must fail at parse time"
            );
        }
    }
}
