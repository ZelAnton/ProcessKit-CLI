//! The **preflight probe** (`processkit-cli probe`): the in-binary half of the
//! fail-closed launcher preflight a consumer drives through the CLI.
//!
//! A consumer that discovers a `processkit-cli` binary must confirm the file is
//! *suitable* — that it exists, is executable, and is **version-compatible on the confirmed surface** (the CLI
//! flags, the reserved exit-code band, and the JSONL `schema_version`) — **before**
//! it launches any payload through it. Silently falling back to an uncontained
//! launch when the candidate is unusable would reintroduce exactly the
//! process-leak hazard this project exists to prevent, so the contract is
//! *fail-closed*: an unusable candidate must be reported, never worked around.
//!
//! This module is the tool the consumer runs on the candidate to make that
//! decision: it prints the binary's compatibility surface as one machine-readable
//! JSON line and, when the consumer passes `--require-*` expectations, *verifies*
//! them and fails closed with [`exit::PROBE_INCOMPATIBLE`] (110) on any mismatch.
//!
//! ## No side effects — never a real contained process
//!
//! A probe is pure: it reads compile-time constants ([`events::SCHEMA_VERSION`],
//! the [`exit`] band) and introspects the clap surface in memory, then prints. It
//! spawns no child, opens no run registry, binds no control endpoint, and creates
//! no container. Running `probe` on a candidate is therefore safe to do as a
//! preflight, with none of the effects a real `run` would have.
//!
//! ## The three consumer-side outcomes it grounds
//!
//! The consumer runs `<path> probe --json [--require-...]`; the outcomes this
//! binary is responsible for are:
//!
//! - **path missing / not executable** — the *spawn* fails before this process ever
//!   starts; the consumer distinguishes them by the OS error (a missing path is
//!   `NotFound`, a present-but-unspawnable file is a different error). Fail closed.
//! - **present, executable, but incompatible** — this process runs but its surface
//!   does not satisfy the consumer's `--require-*` expectations, so it prints
//!   `compatible: false` with the concrete `mismatches` and exits `110`. A consumer
//!   that instead reads the report and compares fields itself reaches the same
//!   verdict. Either way the result is distinct and parseable — never a silent
//!   "ok" and never a generic error.
//! - **ok** — a compatible `processkit-cli`: `compatible: true`, exit `0`.

use serde::{Deserialize, Serialize};

use crate::cli::ProbeArgs;
use crate::events::SCHEMA_VERSION;
use crate::exit::{self, RUNNER_RANGE_END, RUNNER_RANGE_START, RunnerError};

/// The probe report's own format version. Independent of the JSONL event
/// [`schema_version`](crate::events::SCHEMA_VERSION), the control-plane
/// [`snapshot_version`](crate::control::SNAPSHOT_VERSION), and the
/// [`registry_version`](crate::registry::REGISTRY_VERSION): the probe report is the
/// launcher contract's own client/runner surface, so it versions on its own axis.
/// Bump it only on a breaking change to the report's shape.
pub const PROBE_VERSION: u32 = 1;

/// The reserved runner exit-code band, as reported by a probe. A consumer pins this
/// so a child exit that happens to fall in the band is never confused with a runner
/// failure (see `docs/exit-codes.md`).
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExitCodeBand {
    /// Inclusive lower bound (`100`).
    pub start: u8,
    /// Inclusive upper bound (`119`).
    pub end: u8,
}

/// The machine-readable preflight report `probe` prints: everything a consumer
/// needs to decide whether this binary is a compatible launch target. `Serialize`
/// to render it, `Deserialize` so a consumer (and the tests) parse it back and
/// check fields rather than scraping text.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeReport {
    /// This report's format version ([`PROBE_VERSION`]).
    pub probe_version: u32,
    /// The binary's package name (`processkit-cli`), so a consumer can confirm the
    /// candidate really is this runner and not some other program that happens to
    /// accept a `probe` argument.
    pub binary: String,
    /// The binary's semantic version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The JSONL event `schema_version` this binary emits — the value adapters pin.
    pub schema_version: u32,
    /// The reserved runner exit-code band this binary uses.
    pub exit_code_band: ExitCodeBand,
    /// The CLI **surface tokens** this binary exposes: every subcommand name and
    /// every subcommand long flag (`run:--capture-dir`, `inspect:--json`, …),
    /// sorted. Derived from the live clap definition so it never drifts from the
    /// real parser. A consumer reads this to confirm the exact flags it will use
    /// exist.
    pub surface: Vec<String>,
    /// Whether every `--require-*` expectation the consumer asked for is satisfied.
    /// `true` when no expectation was requested (a healthy self-report).
    pub compatible: bool,
    /// The concrete, human-readable reasons `compatible` is `false` — one per unmet
    /// expectation. Always present; empty when `compatible` is `true`. This is the
    /// parseable "why", so an incompatibility is never a generic error.
    pub mismatches: Vec<String>,
}

/// Run the preflight probe: build the report, verify any `--require-*` expectations,
/// print the report as one JSON line to **stdout** (in *both* the compatible and the
/// incompatible case, so the consumer always has a parseable result), and return the
/// fail-closed verdict. A satisfied (or unrequested) surface is `Ok(())` (exit `0`);
/// any unmet expectation is [`exit::PROBE_INCOMPATIBLE`] (110) with the mismatches
/// echoed on stderr by the caller.
pub fn run(args: &ProbeArgs) -> Result<(), RunnerError> {
    let mismatches = evaluate(args);
    let compatible = mismatches.is_empty();
    let report = ProbeReport {
        probe_version: PROBE_VERSION,
        binary: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version: SCHEMA_VERSION,
        exit_code_band: ExitCodeBand {
            start: RUNNER_RANGE_START,
            end: RUNNER_RANGE_END,
        },
        surface: surface_tokens(),
        compatible,
        mismatches,
    };

    let json = serde_json::to_string(&report).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not render the probe report: {err}"),
        )
    })?;
    println!("{json}");

    if compatible {
        Ok(())
    } else {
        Err(RunnerError::new(
            exit::PROBE_INCOMPATIBLE,
            format!(
                "this binary is incompatible with the requested launch surface: {}",
                report.mismatches.join("; ")
            ),
        ))
    }
}

/// Check every `--require-*` expectation against this binary's real surface and
/// return the unmet ones (empty ⇒ compatible). Each comparison is against a
/// compile-time or clap-derived fact, so the verdict is deterministic.
fn evaluate(args: &ProbeArgs) -> Vec<String> {
    let mut mismatches = Vec::new();

    // schema_version: an exact match — adapters pin one version, and a different one
    // is a breaking (major) change they do not understand. `filter` keeps only a
    // requested-and-mismatching value, so the binding is the offending version.
    if let Some(want) = args
        .require_schema_version
        .filter(|&want| want != SCHEMA_VERSION)
    {
        mismatches.push(format!(
            "requires JSONL schema_version {want}, but this binary emits {SCHEMA_VERSION}"
        ));
    }

    // exit-code band: an exact match against the reserved band.
    if let Some((start, end)) = args
        .require_exit_code_band
        .filter(|&band| band != (RUNNER_RANGE_START, RUNNER_RANGE_END))
    {
        mismatches.push(format!(
            "requires exit-code band {start}-{end}, but this binary reserves \
             {RUNNER_RANGE_START}-{RUNNER_RANGE_END}"
        ));
    }

    // CLI surface tokens: every requested one must be present.
    let surface = surface_tokens();
    for token in &args.require_surface {
        if !surface.iter().any(|present| present == token) {
            mismatches.push(format!(
                "requires CLI surface token `{token}`, which this binary does not expose"
            ));
        }
    }

    mismatches
}

/// The binary's CLI surface tokens, derived from the **live** clap definition so the
/// report can never drift from the real parser: for each subcommand, its name, plus
/// `<name>:--<long>` for each of its long flags. The clap-injected `--help` /
/// `--version` flags are excluded — they are not part of the compatibility surface.
/// Sorted and deduplicated for a deterministic, stable order.
fn surface_tokens() -> Vec<String> {
    use clap::CommandFactory;

    let command = crate::cli::Cli::command();
    let mut tokens = Vec::new();
    for sub in command.get_subcommands() {
        let name = sub.get_name();
        tokens.push(name.to_string());
        for arg in sub.get_arguments() {
            if let Some(long) = arg.get_long() {
                if long == "help" || long == "version" {
                    continue;
                }
                tokens.push(format!("{name}:--{long}"));
            }
        }
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    /// Parse a `probe` invocation and hand back its [`ProbeArgs`] — the tests drive
    /// the real CLI so they exercise the same surface a consumer would.
    fn probe_args(extra: &[&str]) -> ProbeArgs {
        let mut argv = vec!["processkit-cli", "probe", "--json"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("a valid probe invocation");
        match cli.command {
            Command::Probe(args) => args,
            _ => panic!("expected the probe subcommand"),
        }
    }

    /// A bare probe is a healthy self-report: it names the binary, carries the
    /// build's version, the current schema_version, and the reserved band, and is
    /// `compatible` with no mismatches.
    #[test]
    fn bare_probe_is_a_healthy_self_report() {
        let args = probe_args(&[]);
        let mismatches = evaluate(&args);
        assert!(
            mismatches.is_empty(),
            "an unqualified probe requests nothing, so nothing can mismatch: {mismatches:?}"
        );

        // Build the same report `run` would, and check the deterministic fields.
        let report = ProbeReport {
            probe_version: PROBE_VERSION,
            binary: env!("CARGO_PKG_NAME").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version: SCHEMA_VERSION,
            exit_code_band: ExitCodeBand {
                start: RUNNER_RANGE_START,
                end: RUNNER_RANGE_END,
            },
            surface: surface_tokens(),
            compatible: mismatches.is_empty(),
            mismatches,
        };
        assert_eq!(report.binary, "processkit-cli");
        assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(report.schema_version, SCHEMA_VERSION);
        assert_eq!(
            report.exit_code_band,
            ExitCodeBand {
                start: 100,
                end: 119
            }
        );
        assert!(report.compatible && report.mismatches.is_empty());

        // The report round-trips through JSON, so a consumer parses back exactly what
        // was serialized.
        let line = serde_json::to_string(&report).expect("the report serializes");
        let parsed: ProbeReport = serde_json::from_str(&line).expect("the report parses back");
        assert_eq!(parsed.probe_version, PROBE_VERSION);
        assert_eq!(parsed.schema_version, SCHEMA_VERSION);
        assert_eq!(
            parsed.exit_code_band,
            ExitCodeBand {
                start: 100,
                end: 119
            }
        );
        assert!(parsed.compatible);
    }

    /// The surface list is derived from the live parser: it carries every subcommand
    /// and representative long flags, and excludes the clap-injected help/version.
    #[test]
    fn surface_tokens_track_the_real_cli() {
        let surface = surface_tokens();
        for expected in [
            "run",
            "inspect",
            "cancel",
            "kill",
            "probe",
            "run:--jsonl",
            "run:--capture-dir",
            "run:--inherit-stdio",
            "run:--inherit-stdin",
            "run:--stdin-file",
            "run:--timeout",
            "inspect:--json",
            "cancel:--run-id",
            "probe:--json",
            "probe:--require-schema-version",
            "probe:--require-exit-code-band",
            "probe:--require-surface",
        ] {
            assert!(
                surface.iter().any(|t| t == expected),
                "surface must expose `{expected}`: {surface:?}"
            );
        }
        assert!(
            !surface
                .iter()
                .any(|t| t.ends_with(":--help") || t.ends_with(":--version")),
            "clap's help/version flags are not part of the compatibility surface: {surface:?}"
        );
        // Deterministic: sorted and free of duplicates.
        let mut sorted = surface.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(surface, sorted, "the surface is sorted and deduplicated");
    }

    /// A schema_version the binary cannot meet is a fail-closed mismatch that names
    /// both the requested and the actual value — never a silent "ok".
    #[test]
    fn a_schema_version_mismatch_fails_closed() {
        let unsupported = SCHEMA_VERSION + 1;
        let args = probe_args(&["--require-schema-version", &unsupported.to_string()]);
        let mismatches = evaluate(&args);
        assert_eq!(mismatches.len(), 1, "one unmet expectation: {mismatches:?}");
        assert!(
            mismatches[0].contains(&unsupported.to_string())
                && mismatches[0].contains(&SCHEMA_VERSION.to_string()),
            "the mismatch names both the requested and the actual schema: {mismatches:?}"
        );

        // The exact current version is, conversely, compatible.
        let ok = probe_args(&["--require-schema-version", &SCHEMA_VERSION.to_string()]);
        assert!(
            evaluate(&ok).is_empty(),
            "the current schema_version is compatible"
        );
    }

    /// An exit-code band that differs from the reserved one is a fail-closed
    /// mismatch; the exact reserved band is compatible.
    #[test]
    fn an_exit_code_band_mismatch_fails_closed() {
        let args = probe_args(&["--require-exit-code-band", "100-118"]);
        let mismatches = evaluate(&args);
        assert_eq!(
            mismatches.len(),
            1,
            "the narrowed band mismatches: {mismatches:?}"
        );

        let ok = probe_args(&["--require-exit-code-band", "100-119"]);
        assert!(evaluate(&ok).is_empty(), "the reserved band is compatible");
    }

    /// A surface token the binary does not expose is a fail-closed mismatch; present
    /// tokens are compatible, and several requirements can be checked at once.
    #[test]
    fn a_missing_surface_token_fails_closed() {
        let args = probe_args(&["--require-surface", "run:--not-a-real-flag"]);
        let mismatches = evaluate(&args);
        assert_eq!(
            mismatches.len(),
            1,
            "the bogus token mismatches: {mismatches:?}"
        );
        assert!(mismatches[0].contains("run:--not-a-real-flag"));

        let ok = probe_args(&[
            "--require-surface",
            "probe",
            "--require-surface",
            "run:--capture-dir",
            "--require-surface",
            "run:--inherit-stdio",
            "--require-surface",
            "run:--inherit-stdin",
            "--require-surface",
            "run:--stdin-file",
        ]);
        assert!(
            evaluate(&ok).is_empty(),
            "real subcommand/flag tokens are compatible"
        );
    }

    /// Several unmet expectations accumulate — each is reported, so a consumer sees
    /// every reason at once rather than one-at-a-time.
    #[test]
    fn multiple_mismatches_all_report() {
        let unsupported = SCHEMA_VERSION + 7;
        let args = probe_args(&[
            "--require-schema-version",
            &unsupported.to_string(),
            "--require-exit-code-band",
            "1-2",
            "--require-surface",
            "no-such-subcommand",
        ]);
        let mismatches = evaluate(&args);
        assert_eq!(
            mismatches.len(),
            3,
            "all three expectations are unmet: {mismatches:?}"
        );
    }
}
