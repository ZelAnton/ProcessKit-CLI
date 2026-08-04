//! Arguments for the `attest` control-plane client — the containment-membership
//! question a caller can only ask *about itself* (the client itself lives in
//! [`crate::control`]).

use clap::Args;

use super::parse::parse_run_id;

/// `attest --run-id <id> [--json]`
///
/// **There is deliberately no `--pid` (or any other way to name a process), and no
/// `--all`.** Both absences are the feature, not a gap:
///
/// - a caller-supplied pid would make the command answer "is *that* process a member
///   of run X" — a fact about some other process, which proves nothing about the
///   caller and would let any process launder a membership claim about a pid it
///   picked. The identity is taken from the control connection this very process
///   opens (`src/control/mod.rs`, `PeerIdentity`), so the only question this command
///   can pose is "am I inside this run?";
/// - `--all` would answer "am I inside *any* live run", which is a different question
///   with a different failure mode (it invites a caller to accept membership in a run
///   it never named), and every other aggregate form here exists to act on a set of
///   runs rather than to widen a single verdict. A caller that genuinely wants to
///   test several runs asks about each one, and gets a separate, attributable answer.
#[derive(Debug, Args)]
pub struct AttestArgs {
    /// The run to be attested against. Required and single: the command answers
    /// about the calling process and this one run, never about a process the caller
    /// names or a set of runs.
    #[arg(long, value_name = "id", value_parser = parse_run_id)]
    pub run_id: String,

    /// Emit the attestation as one JSON line instead of a human-readable rendering.
    /// Optional and off by default, mirroring `inspect`/`list`/`prune`. The verdict
    /// and the exit code are identical either way — only the rendering changes.
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Command};

    #[test]
    fn attest_requires_a_run_id_and_json_is_optional() {
        let cli = Cli::try_parse_from(["processkit-cli", "attest", "--run-id", "r1"])
            .expect("--json is optional and defaults to off for attest");
        let Command::Attest(args) = cli.command else {
            panic!("expected the attest subcommand");
        };
        assert_eq!(args.run_id, "r1");
        assert!(!args.json, "--json defaults to off");

        let cli = Cli::try_parse_from(["processkit-cli", "attest", "--run-id", "r1", "--json"])
            .expect("attest --json parses");
        let Command::Attest(args) = cli.command else {
            panic!("expected the attest subcommand");
        };
        assert!(args.json);

        assert!(
            Cli::try_parse_from(["processkit-cli", "attest"]).is_err(),
            "--run-id is required: there is no default target"
        );
    }

    /// The absence of a caller-supplied process identity is a security property, so
    /// it is asserted rather than left to inspection: a future flag that let a caller
    /// name *which* process to attest would silently turn "the caller is contained"
    /// into "some pid is contained" (see [`super::AttestArgs`]).
    #[test]
    fn attest_offers_no_way_to_name_another_process_or_every_run() {
        for argv in [
            vec!["processkit-cli", "attest", "--pid", "1234"],
            vec![
                "processkit-cli",
                "attest",
                "--run-id",
                "r1",
                "--pid",
                "1234",
            ],
            vec![
                "processkit-cli",
                "attest",
                "--run-id",
                "r1",
                "--process",
                "1234",
            ],
            vec!["processkit-cli", "attest", "--all"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?} must be rejected: attest answers only about the calling process, \
                 and only about one named run"
            );
        }
    }

    /// `attest` is a by-`run-id` command, so it inherits the same terminal-safety bar
    /// at parse time every other one enforces (`cli::parse_run_id`).
    #[test]
    fn attest_rejects_unsafe_run_ids_at_parse_time() {
        let too_long = "x".repeat(257);
        for bad in ["", "line\nbreak", "bidi\u{202e}override", &too_long] {
            assert!(
                Cli::try_parse_from(["processkit-cli", "attest", "--run-id", bad]).is_err(),
                "attest must reject {bad:?}"
            );
        }
    }
}
