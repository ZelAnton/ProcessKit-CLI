//! Arguments for the `probe` subcommand (the side-effect-free preflight itself
//! lives in [`crate::probe`]).

use clap::Args;

use super::parse::parse_exit_code_band;

/// `probe --json [--require-schema-version <N>] [--require-exit-code-band <s>-<e>]
/// [--require-surface <token>]... [--print-schema]`
///
/// The **preflight** reports — and, when asked, *verifies* — this binary's
/// compatibility surface (the JSONL `schema_version`, the reserved exit-code band,
/// and the CLI surface tokens) so a consumer can confirm a candidate **before**
/// launching any payload. It spawns nothing and touches no registry or container:
/// it is a pure self-report, so running it has no side effects. The `--require-*`
/// flags are the machine-checkable half — each one a consumer expectation; any that
/// this binary cannot meet makes `probe` fail closed with
/// [`crate::exit::PROBE_INCOMPATIBLE`] (110) instead of a false "ok". `--print-schema`
/// is a separate, simpler contract on the same subcommand: it prints the embedded
/// schema document instead of the report, and clap rejects it outright (`USAGE`,
/// 100) if combined with any `--require-*` flag rather than silently skipping
/// their evaluation (see its own doc comment below).
#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// Emit the report as JSON. Required because `probe` is a machine-readable
    /// preflight contract with a single fixed output shape — unlike `inspect`, whose
    /// `--json` is optional (T-214) since it also has a human-readable form. clap
    /// enforces this flag's presence and `probe` always prints JSON.
    #[allow(dead_code)] // Part of the fixed CLI form; enforced by clap, never read.
    #[arg(long, required = true)]
    pub json: bool,

    /// Require the binary's JSONL event `schema_version` to equal `<N>` exactly
    /// (adapters pin an exact version). A mismatch is a fail-closed incompatibility.
    #[arg(long, value_name = "N")]
    pub require_schema_version: Option<u32>,

    /// Require the reserved runner exit-code band to be exactly `<start>-<end>`
    /// (e.g. `100-119`). A mismatch is a fail-closed incompatibility. A malformed
    /// value is a usage error (100), like any other bad flag.
    #[arg(long, value_name = "start-end", value_parser = parse_exit_code_band)]
    pub require_exit_code_band: Option<(u8, u8)>,

    /// Require a CLI **surface token** to be present (repeatable). A token is either
    /// a subcommand name (`run`, `probe`) or a subcommand long flag
    /// (`run:--capture-dir`, `inspect:--json`). An absent token is a fail-closed
    /// incompatibility, so a consumer can assert the exact flags it will use exist.
    #[arg(long = "require-surface", value_name = "token")]
    pub require_surface: Vec<String>,

    /// Print this binary's embedded JSONL event-schema document
    /// (`fixtures/schema/v1/schema.json`, embedded at build time via
    /// `include_str!`, see [`crate::probe::SCHEMA_JSON`]) to stdout, byte-for-byte
    /// identical to that fixture file, and exit successfully — **instead of**
    /// evaluating or printing the usual probe report. **Cannot be combined with
    /// any `--require-*` flag**: clap rejects that combination as an ordinary
    /// usage error (exit `100`), the same code any other malformed `probe`
    /// invocation gets, rather than silently skipping the requested checks and
    /// exiting `0` — a probe that were asked to verify expectations must never
    /// report a false "ok" (see the module-level fail-closed contract in
    /// `src/probe.rs`). This lets a consumer holding only an installed binary or
    /// an unpacked release archive (no git checkout, no tag to match) fetch the
    /// exact machine-readable schema its own version emits, entirely offline
    /// (see `docs/schema.md` and README's "JSONL event schema").
    #[arg(
        long,
        conflicts_with_all = [
            "require_schema_version",
            "require_exit_code_band",
            "require_surface"
        ]
    )]
    pub print_schema: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Command};

    #[test]
    fn probe_requires_json_and_accepts_the_require_flags() {
        // `probe` keeps a fixed form where `--json` is mandatory, unlike `inspect`,
        // where `--json` became optional (T-214).
        assert!(
            Cli::try_parse_from(["processkit-cli", "probe", "--json"]).is_ok(),
            "a bare `probe --json` is the minimal valid form"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "probe"]).is_err(),
            "--json is part of the fixed probe form"
        );

        // The requirement flags parse and are captured, `--require-surface` repeats.
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "probe",
            "--json",
            "--require-schema-version",
            "1",
            "--require-exit-code-band",
            "100-119",
            "--require-surface",
            "probe",
            "--require-surface",
            "run:--jsonl",
        ])
        .expect("a valid probe invocation");
        let Command::Probe(args) = cli.command else {
            panic!("expected the probe subcommand");
        };
        assert_eq!(args.require_schema_version, Some(1));
        assert_eq!(args.require_exit_code_band, Some((100, 119)));
        assert_eq!(args.require_surface, vec!["probe", "run:--jsonl"]);
        assert!(
            !args.print_schema,
            "--print-schema is opt-in, off by default"
        );

        // `--print-schema` parses on its own (still under the fixed `--json` form).
        let cli = Cli::try_parse_from(["processkit-cli", "probe", "--json", "--print-schema"])
            .expect("a valid probe --print-schema invocation");
        let Command::Probe(args) = cli.command else {
            panic!("expected the probe subcommand");
        };
        assert!(args.print_schema);
    }

    /// `--print-schema` conflicts with every `--require-*` flag at the clap level:
    /// combining them is an ordinary usage error, never a silent skip of the
    /// requested checks (R-01 — a probe that were asked to verify expectations
    /// must never report a false "ok").
    #[test]
    fn print_schema_conflicts_with_every_require_flag() {
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "probe",
                "--json",
                "--print-schema",
                "--require-schema-version",
                "1",
            ])
            .is_err(),
            "--print-schema + --require-schema-version must be rejected, not silently accepted"
        );
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "probe",
                "--json",
                "--print-schema",
                "--require-exit-code-band",
                "100-119",
            ])
            .is_err(),
            "--print-schema + --require-exit-code-band must be rejected"
        );
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "probe",
                "--json",
                "--print-schema",
                "--require-surface",
                "probe",
            ])
            .is_err(),
            "--print-schema + --require-surface must be rejected"
        );
    }

    #[test]
    fn probe_rejects_a_malformed_exit_code_band() {
        // A bad band is a form error, so parsing fails (mapped to USAGE) rather than
        // reaching the probe handler.
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "probe",
                "--json",
                "--require-exit-code-band",
                "not-a-band",
            ])
            .is_err(),
            "a malformed --require-exit-code-band must fail at parse time"
        );
    }
}
