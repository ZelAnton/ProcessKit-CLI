//! Command-line surface for processkit-cli.
//!
//! This is the *CLI flags* half of the compatibility surface fixed by
//! `AGENTS.md`; the shapes here are normative and mirror README's "Command
//! interface".

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

/// Top-level parser: one required subcommand, no global options.
#[derive(Debug, Parser)]
#[command(
    name = "processkit-cli",
    version,
    about = "Run one shell-free command inside ProcessKit's containment boundary.",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The commands that make up the runner's control surface.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a program inside a ProcessKit container and report its lifecycle.
    Run(Box<RunArgs>),
    /// Query a live run over local IPC.
    Inspect(InspectArgs),
    /// Ask a live run to cancel (graceful where supported, then a hard kill).
    Cancel(TargetArgs),
    /// Hard-kill a live run's container immediately.
    Kill(TargetArgs),
    /// List every run recorded in the per-user registry, live and stale alike.
    List(ListArgs),
    /// Reap the registry's confirmed-stale entries — the leftover records of runners
    /// that died abruptly — while never touching a live run's entry.
    Prune(PruneArgs),
    /// Report this binary's compatibility surface for a consumer's fail-closed
    /// compatibility preflight — no run, no child, no side effects.
    Probe(ProbeArgs),
}

/// `run [--run-id <id>] [--cwd <dir>] --jsonl <events.jsonl> [--create-no-window]
/// [--timeout <duration>] [--idle-timeout <duration>] [--grace <duration>]
/// [--max-memory <size>] [--max-processes <n>] [--cpu-quota <cores>]
/// [--capture-dir <dir>] [--capture-max-bytes <size>] [--argv-raw]
/// [--inherit-stdio | --inherit-stdin | --stdin-file <file>]
/// -- <program> <args...>`
//
// `run` consumes every field: `cwd`, `create_no_window`, `timeout`,
// `idle_timeout`, `grace`, `max_memory`, `max_processes`, `cpu_quota` — the
// whole-tree ProcessKit resource caps (see `src/run.rs`) — `command`, `jsonl`,
// `run_id`, `argv_raw`, `capture_dir`/`capture_max_bytes` — bounded stdout/stderr
// capture to files (see `src/capture.rs`) — `env_clear`, `env_remove`, `env`, and
// `inherit_stdio`/`inherit_stdin`/`stdin_file`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Identifier for this run; a value is generated when omitted.
    #[arg(long, value_name = "id")]
    pub run_id: Option<String>,

    /// Working directory for the child process.
    #[arg(long, value_name = "dir")]
    pub cwd: Option<PathBuf>,

    /// File to receive the versioned JSONL lifecycle events (never stdout).
    #[arg(long, value_name = "events.jsonl")]
    pub jsonl: PathBuf,

    /// Windows: create the child with CREATE_NO_WINDOW.
    #[arg(long)]
    pub create_no_window: bool,

    /// Hard deadline for the whole run; the tree is torn down when it elapses.
    // Parsed and validated *here*, at the CLI layer, rather than deferred to the
    // runner: a malformed duration is a form error like any other bad flag, so it
    // belongs with the parsing surface (this module) and surfaces as the same
    // documented `USAGE` (100) exit, not a mid-run failure. `run` then receives an
    // already-validated `Duration` and never re-parses a string. See
    // [`parse_duration`] for the accepted grammar.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    pub timeout: Option<Duration>,

    /// Deadline on child **silence**: the tree is torn down when the child produces
    /// no observed output for this long. Unlike `--timeout` (a ceiling on the whole
    /// run), this deadline is *re-armed* on every chunk of the child's output, so a
    /// child that keeps talking is never reaped no matter how long it runs — only
    /// one that goes quiet past the window is (the classic "stuck build worker").
    /// An idle expiry reuses the same reserved `TIMEOUT` (106) exit and the same
    /// soft-stop → grace → hard-kill teardown as `--timeout`, told apart only by the
    /// `timeout` event's `reason` field (`idle` vs `overall`; see `docs/schema.md`).
    /// Same grammar and parse-time validation as `--timeout` (see [`parse_duration`]).
    /// Cannot be combined with `--inherit-stdio`: under direct inheritance the runner
    /// runs no output pump, so there is no point at which to observe the child and
    /// re-arm the deadline — the flags conflict at parse time, like `--capture-dir`.
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    pub idle_timeout: Option<Duration>,

    /// Grace period between a cancel/timeout and the hard kill. Same grammar and
    /// parse-time validation as `--timeout` (see [`parse_duration`]).
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    pub grace: Option<Duration>,

    /// Cap the run's **whole process tree** total memory. Accepts a byte count
    /// with an optional binary unit — `1048576`, `512k`, `256m`, `2g` (see
    /// [`parse_size`] for the grammar). Enforcement needs a real whole-tree
    /// container (Windows Job Object or Linux cgroup v2); where none exists the
    /// run fails fast with a `limit_hit` rather than silently running unbounded
    /// (see `README.md`, "Resource limits"). Omit to leave memory unbounded.
    #[arg(long, value_name = "size", value_parser = parse_size)]
    pub max_memory: Option<u64>,

    /// Cap the number of live processes in the run's **whole tree**. A positive
    /// integer (`0` is rejected at parse time). Same whole-tree-container
    /// requirement and fail-fast `limit_hit` as `--max-memory`; note the Linux
    /// asymmetry documented in `README.md` ("Resource limits"): there it bounds a
    /// contained child's *descendants*, not the number of top-level launches into
    /// the group. Omit to leave the count unbounded.
    #[arg(long, value_name = "n", value_parser = parse_max_processes)]
    pub max_processes: Option<u32>,

    /// Cap the run's **whole-tree** CPU as a fraction of a single core: `0.5` is
    /// half a core, `2` is two cores' worth. A finite value greater than `0`
    /// (`0`, negatives, `NaN`, and infinities are rejected at parse time — see
    /// [`parse_cpu_quota`]). Same whole-tree-container requirement and fail-fast
    /// `limit_hit` as `--max-memory`. Omit to leave CPU unbounded.
    #[arg(long, value_name = "cores", value_parser = parse_cpu_quota)]
    pub cpu_quota: Option<f64>,

    /// Directory for bounded stdout/stderr capture files (`stdout.log`,
    /// `stderr.log`). When set, the child's output is teed into these files
    /// alongside the live echo; each stream's byte count, content hash, and
    /// truncation flag are reported in the `output_captured` JSONL event.
    #[arg(long, value_name = "dir")]
    pub capture_dir: Option<PathBuf>,

    /// Per-**stream** ceiling on bytes written to a `--capture-dir` file — the
    /// same value that otherwise defaults to `crate::capture::CAPTURE_MAX_BYTES`
    /// (8 MiB). Same grammar as `--max-memory` (see [`parse_size`]): a byte count
    /// with an optional binary unit — `1048576`, `512k`, `256m`, `2g`. Only takes
    /// effect together with `--capture-dir`; omitting it leaves the default
    /// ceiling in place, so a bare `run` (with or without `--capture-dir`) stays
    /// byte-for-byte unchanged. Does not change the `output_captured` event's
    /// shape or the meaning of its `truncated` flag, which still just means "the
    /// stream outran whatever per-stream ceiling was in effect" (see
    /// `src/capture.rs` and `README.md`, "Bounded output capture").
    #[arg(long, value_name = "size", value_parser = parse_size)]
    pub capture_max_bytes: Option<u64>,

    /// Give the child the runner's stdin, stdout, and stderr handles directly.
    /// This preserves terminal status and cannot be combined with mediated I/O
    /// or Windows' no-console mode.
    #[arg(
        long,
        conflicts_with_all = [
            "capture_dir",
            "create_no_window",
            "idle_timeout",
            "inherit_stdin",
            "stdin_file"
        ]
    )]
    pub inherit_stdio: bool,

    /// Give the child the runner's own stdin (terminal, file, or pipe). This does
    /// not create a PTY and cannot be combined with `--stdin-file`.
    #[arg(long, conflicts_with = "stdin_file")]
    pub inherit_stdin: bool,

    /// Stream this file to the child's stdin, then close it at EOF. The file's
    /// bytes stay out of argv and cannot be combined with `--inherit-stdin`.
    #[arg(long, value_name = "file", conflicts_with = "inherit_stdin")]
    pub stdin_file: Option<PathBuf>,

    /// Record the raw argv in diagnostics instead of the redacted hash + hint.
    #[arg(long)]
    pub argv_raw: bool,

    /// Clear the child's entire inherited environment before any
    /// `--env-remove`/`--env` is applied (repeatable flag has no effect beyond
    /// the first). Maps onto `processkit::Command::env_clear()`. See
    /// `README.md`, "Environment", for the full applied order.
    #[arg(long)]
    pub env_clear: bool,

    /// Remove an inherited environment variable by name (repeatable). Applied
    /// after `--env-clear` and before `--env`, so an explicit `--env` for the
    /// same key still wins. Maps onto `processkit::Command::env_remove()`.
    #[arg(long = "env-remove", value_name = "KEY")]
    pub env_remove: Vec<String>,

    /// Set an environment variable for the child as `KEY=VALUE` (repeatable).
    /// Applied last — after `--env-clear` and `--env-remove` — so it always wins
    /// on a duplicated key. Maps onto `processkit::Command::env()`.
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env_kv)]
    pub env: Vec<(String, String)>,

    /// The program to run followed by its arguments. Everything after `--` is
    /// taken verbatim — there is no shell mode, so nothing here is expanded or
    /// re-interpreted. Kept as `OsString`s to preserve bytes exactly.
    #[arg(last = true, required = true, num_args = 1.., value_name = "program")]
    pub command: Vec<OsString>,
}

/// `inspect --run-id <id> --json`
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// The run to inspect.
    #[arg(long, value_name = "id")]
    pub run_id: String,

    /// Emit the snapshot as JSON. Required to match the fixed form; JSON is
    /// currently the only supported output format, so the flag is not branched on —
    /// clap enforces its presence and `inspect` always prints JSON.
    #[allow(dead_code)] // Part of the fixed CLI form; enforced by clap, never read.
    #[arg(long, required = true)]
    pub json: bool,
}

/// Shared argument for the by-run-id control commands (`cancel`, `kill`).
#[derive(Debug, Args)]
pub struct TargetArgs {
    /// The run to act on.
    #[arg(long, value_name = "id")]
    pub run_id: String,
}

/// `list [--json]`
///
/// Scans the per-user registry ([`crate::registry::Registry::entries`]) and prints
/// every entry it finds, live or stale — the discovery counterpart to the
/// by-`run-id` commands above, for an operator or orchestrator that has lost (or
/// never had) a `run_id`. An empty registry is not an error: it prints an empty
/// result and exits `0`, and a single unreadable/corrupt record never blinds the
/// command to the healthy entries (the same per-record degradation
/// `Registry::entries` already applies).
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit one JSON object per entry (one per line) instead of a human-readable
    /// table. Unlike `inspect`/`probe`, this flag is optional — `list` has a
    /// human-readable form of its own.
    #[arg(long)]
    pub json: bool,
}

/// `prune [--json]`
///
/// Scans the per-user registry ([`crate::registry::Registry::prune`]) and reaps every
/// entry it can **confirm** is stale — a leftover `.json`/`.lock` pair from a runner
/// that died abruptly without running its clean-exit removal — while leaving every
/// live entry, and every entry whose liveness it could not probe, untouched. Unlike
/// `list`, prune *mutates* the registry (it deletes files), but like `list` it opens
/// the registry read-only: a missing registry has nothing to prune, so prune never
/// creates the directory or touches its permissions just to look. An empty (or
/// missing) registry is not an error — prune reports a zero tally and exits `0`.
#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Emit the prune tally as a single JSON object instead of a human-readable
    /// summary line. Optional, mirroring `list` — prune has a human-readable form of
    /// its own.
    #[arg(long)]
    pub json: bool,
}

/// `probe --json [--require-schema-version <N>] [--require-exit-code-band <s>-<e>]
/// [--require-surface <token>]...`
///
/// The **preflight** reports — and, when asked, *verifies* — this binary's
/// compatibility surface (the JSONL `schema_version`, the reserved exit-code band,
/// and the CLI surface tokens) so a consumer can confirm a candidate **before**
/// launching any payload. It spawns nothing and touches no registry or container:
/// it is a pure self-report, so running it has no side effects. The `--require-*`
/// flags are the machine-checkable half — each one a consumer expectation; any that
/// this binary cannot meet makes `probe` fail closed with
/// [`crate::exit::PROBE_INCOMPATIBLE`] (110) instead of a false "ok".
#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// Emit the report as JSON. Required to match the fixed form (JSON is the only
    /// supported output format, as for `inspect`); clap enforces its presence and
    /// `probe` always prints JSON.
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
}

/// Parse a human duration for `--timeout` / `--grace`.
///
/// Grammar: a base-10, non-negative integer with an optional unit suffix — `ms`,
/// `s` (the default when the suffix is omitted), `m`, or `h`. Examples: `30`
/// (= 30 seconds), `500ms`, `5s`, `2m`, `1h`. Deliberately strict — a sign, a
/// fraction, surrounding whitespace, or an unknown unit is rejected rather than
/// silently reinterpreted, so a typo fails loudly at parse time instead of arming
/// a surprising deadline. The value is capped only by `u64` milliseconds; an
/// overflow is reported, not wrapped.
///
/// Returns the message that clap renders on failure (which the binary maps to the
/// `USAGE` exit code); on success it hands `run` a ready `Duration`.
///
/// `pub`/`#[doc(hidden)]` (not exported clap surface) so the CLI-parsers fuzz
/// target can drive it directly with arbitrary text (`fuzz/fuzz_targets/cli_parsers.rs`, T-186).
#[doc(hidden)]
pub fn parse_duration(raw: &str) -> Result<Duration, String> {
    if raw.is_empty() {
        return Err("empty duration; expected e.g. `30`, `500ms`, `5s`, `2m`, or `1h`".to_string());
    }

    // Split the leading digit run from the unit suffix. A value that does not
    // start with a digit (a sign, a bare unit, letters) leaves `number` empty.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    if number.is_empty() {
        return Err(format!(
            "duration `{raw}` must start with a non-negative number; \
             expected e.g. `30`, `500ms`, `5s`, `2m`, or `1h`"
        ));
    }

    let value: u64 = number
        .parse()
        .map_err(|_| format!("duration `{raw}` is out of range for a 64-bit millisecond count"))?;

    let millis = match unit {
        "" | "s" => value.checked_mul(1_000),
        "ms" => Some(value),
        "m" => value.checked_mul(60_000),
        "h" => value.checked_mul(3_600_000),
        other => {
            return Err(format!(
                "duration `{raw}` has an unknown unit `{other}`; use ms, s, m, or h"
            ));
        }
    };

    let millis = millis
        .ok_or_else(|| format!("duration `{raw}` is too large to represent in milliseconds"))?;
    Ok(Duration::from_millis(millis))
}

/// Parse a `--max-memory` byte size.
///
/// Grammar: a base-10, positive integer with an optional **binary** unit suffix —
/// `b` (bytes, the default when omitted), `k` (KiB, ×1024), `m` (MiB, ×1024²), or
/// `g` (GiB, ×1024³); the suffix is case-insensitive (`512K` == `512k`). Examples:
/// `1048576`, `512k`, `256m`, `2g`. Deliberately strict — a sign, a fraction,
/// surrounding whitespace, an unknown unit, or a total of `0` bytes is rejected
/// rather than silently reinterpreted, so a typo fails loudly at parse time
/// (mapped to the `USAGE` exit) instead of arming a surprising or degenerate cap.
///
/// **This parser is the single source of truth for the flag's *form*** — it
/// rejects the same nonsense (`0`, non-numeric) that ProcessKit's own
/// `ResourceLimits` validation would (`LimitReason::Invalid`), so a malformed
/// `--max-memory` surfaces as the documented `USAGE` (100) like any other bad
/// flag, never reaching `ProcessGroup::with_options` (where it would otherwise be
/// a mid-run `limit_hit`). The value is capped only by `u64` bytes; an overflow is
/// reported, not wrapped.
///
/// `pub`/`#[doc(hidden)]` (not exported clap surface) so the CLI-parsers fuzz
/// target can drive it directly (`fuzz/fuzz_targets/cli_parsers.rs`, T-186).
#[doc(hidden)]
pub fn parse_size(raw: &str) -> Result<u64, String> {
    if raw.is_empty() {
        return Err("empty size; expected e.g. `1048576`, `512k`, `256m`, or `2g`".to_string());
    }

    // Split the leading digit run from the unit suffix, exactly like
    // `parse_duration`: a value that does not start with a digit (a sign, a bare
    // unit, letters) leaves `number` empty.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    if number.is_empty() {
        return Err(format!(
            "size `{raw}` must start with a non-negative number; \
             expected e.g. `1048576`, `512k`, `256m`, or `2g`"
        ));
    }

    let value: u64 = number
        .parse()
        .map_err(|_| format!("size `{raw}` is out of range for a 64-bit byte count"))?;

    // Case-insensitive so `512K` and `512k` are the same; binary (1024-based)
    // units, documented above so the wire meaning is never ambiguous.
    let bytes = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => Some(value),
        "k" => value.checked_mul(1024),
        "m" => value.checked_mul(1024 * 1024),
        "g" => value.checked_mul(1024 * 1024 * 1024),
        other => {
            return Err(format!(
                "size `{raw}` has an unknown unit `{other}`; use b, k, m, or g"
            ));
        }
    };

    let bytes = bytes.ok_or_else(|| format!("size `{raw}` is too large to represent in bytes"))?;
    if bytes == 0 {
        // `0` bytes is a degenerate cap ProcessKit itself rejects
        // (`LimitReason::Invalid`); catch it here so it is a form error, not a
        // mid-run `limit_hit`.
        return Err(format!("size `{raw}` must be greater than 0 bytes"));
    }
    Ok(bytes)
}

/// Parse a `--max-processes` count: a base-10 integer strictly greater than `0`
/// and within `u32`. Deliberately strict — a sign, a fraction, whitespace, `0`, or
/// an out-of-range value fails loudly at parse time (mapped to the `USAGE` exit)
/// rather than reaching `ProcessGroup::with_options` as a mid-run `limit_hit`.
/// Like [`parse_size`], this parser is the single source of truth for the flag's
/// form, mirroring ProcessKit's `max_processes(0)` → `LimitReason::Invalid`
/// rejection at the CLI boundary instead of duplicating it downstream.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_max_processes(raw: &str) -> Result<u32, String> {
    let n: u32 = raw.parse().map_err(|_| {
        format!("`--max-processes` value `{raw}` must be an integer in 1..=4294967295")
    })?;
    if n == 0 {
        return Err("`--max-processes` must be greater than 0".to_string());
    }
    Ok(n)
}

/// Parse a `--cpu-quota` value: a finite `f64` strictly greater than `0` (a
/// fraction of a single core — `0.5` is half a core, `2` is two cores). `0`,
/// negatives, `NaN`, and the infinities are rejected at parse time (mapped to the
/// `USAGE` exit), mirroring ProcessKit's own `cpu_quota` validity rule
/// (`LimitReason::Invalid`) at the CLI boundary so a nonsense quota is a loud form
/// error, never a mid-run `limit_hit`. Like [`parse_size`]/[`parse_max_processes`]
/// this parser is the single source of truth for the flag's form.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_cpu_quota(raw: &str) -> Result<f64, String> {
    let cores: f64 = raw.parse().map_err(|_| {
        format!("`--cpu-quota` value `{raw}` must be a number, e.g. `0.5`, `1`, `2`")
    })?;
    if !(cores.is_finite() && cores > 0.0) {
        return Err(format!(
            "`--cpu-quota` value `{raw}` must be a finite value greater than 0"
        ));
    }
    Ok(cores)
}

/// Parse a `--require-exit-code-band` value: two `u8`s as `start-end` (e.g.
/// `100-119`). Deliberately strict — exactly one `-` separating two base-10
/// integers, with `start <= end` — so a typo fails loudly at parse time (mapped to
/// the `USAGE` exit) rather than being reinterpreted into a band the consumer did
/// not mean. Returns the message clap renders on failure; on success it hands the
/// probe a ready `(start, end)` pair to compare against the reserved band.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_exit_code_band(raw: &str) -> Result<(u8, u8), String> {
    let (start, end) = raw.split_once('-').ok_or_else(|| {
        format!("exit-code band `{raw}` must be two numbers as `start-end`, e.g. `100-119`")
    })?;
    let start: u8 = start
        .parse()
        .map_err(|_| format!("exit-code band `{raw}` has a non-`u8` start `{start}`"))?;
    let end: u8 = end
        .parse()
        .map_err(|_| format!("exit-code band `{raw}` has a non-`u8` end `{end}`"))?;
    if start > end {
        return Err(format!(
            "exit-code band `{raw}` is inverted: start {start} is above end {end}"
        ));
    }
    Ok((start, end))
}

/// Parse a `--env` value: `KEY=VALUE`, split on the **first** `=` (so a value
/// containing `=` is preserved verbatim rather than truncated). A missing `=` or
/// an empty `KEY` is rejected at parse time — mapped to the `USAGE` exit — rather
/// than silently accepted as a malformed environment variable name.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_env_kv(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw.split_once('=').ok_or_else(|| {
        format!(
            "`--env` value `{raw}` must be `KEY=VALUE` (a literal `=` separating name and value)"
        )
    })?;
    if key.is_empty() {
        return Err(format!("`--env` value `{raw}` has an empty KEY before `=`"));
    }
    Ok((key.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches misconfigured derive attributes (conflicting names, bad
        // num_args, etc.) that would otherwise only surface at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn run_captures_the_command_verbatim_after_double_dash() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "cmd",
            "/c",
            "--not-a-runner-flag",
            "echo hi",
        ])
        .expect("a valid run invocation");

        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.jsonl, PathBuf::from("events.jsonl"));
        assert!(args.run_id.is_none(), "--run-id is optional for run");
        // Flags after `--` must survive as literal argv, not be parsed as runner
        // options.
        assert_eq!(
            args.command,
            vec![
                OsString::from("cmd"),
                OsString::from("/c"),
                OsString::from("--not-a-runner-flag"),
                OsString::from("echo hi"),
            ]
        );
    }

    #[test]
    fn run_requires_jsonl_and_a_command() {
        assert!(
            Cli::try_parse_from(["processkit-cli", "run", "--", "cmd"]).is_err(),
            "--jsonl is required"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "run", "--jsonl", "e.jsonl"]).is_err(),
            "a command after `--` is required"
        );
    }

    #[test]
    fn inspect_requires_run_id_and_json() {
        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--run-id", "r1", "--json"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--run-id", "r1"]).is_err(),
            "--json is part of the fixed form"
        );
        assert!(
            Cli::try_parse_from(["processkit-cli", "inspect", "--json"]).is_err(),
            "--run-id is required"
        );
    }

    #[test]
    fn list_defaults_to_no_json_and_accepts_the_flag() {
        let cli = Cli::try_parse_from(["processkit-cli", "list"]).expect("a bare list");
        let Command::List(args) = cli.command else {
            panic!("expected the list subcommand");
        };
        assert!(
            !args.json,
            "--json is optional and defaults to off for list"
        );

        let cli = Cli::try_parse_from(["processkit-cli", "list", "--json"]).expect("list --json");
        let Command::List(args) = cli.command else {
            panic!("expected the list subcommand");
        };
        assert!(args.json);
    }

    #[test]
    fn prune_defaults_to_no_json_and_accepts_the_flag() {
        let cli = Cli::try_parse_from(["processkit-cli", "prune"]).expect("a bare prune");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert!(
            !args.json,
            "--json is optional and defaults to off for prune"
        );

        let cli = Cli::try_parse_from(["processkit-cli", "prune", "--json"]).expect("prune --json");
        let Command::Prune(args) = cli.command else {
            panic!("expected the prune subcommand");
        };
        assert!(args.json);
    }

    #[test]
    fn cancel_and_kill_require_a_run_id() {
        assert!(Cli::try_parse_from(["processkit-cli", "cancel", "--run-id", "r1"]).is_ok());
        assert!(Cli::try_parse_from(["processkit-cli", "kill", "--run-id", "r1"]).is_ok());
        assert!(Cli::try_parse_from(["processkit-cli", "cancel"]).is_err());
        assert!(Cli::try_parse_from(["processkit-cli", "kill"]).is_err());
    }

    #[test]
    fn parse_duration_accepts_the_documented_grammar() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_duration_rejects_malformed_values() {
        // Empty, non-numeric, signed, fractional, unknown unit, and whitespace all
        // fail loudly rather than being silently reinterpreted.
        for bad in ["", "abc", "-5", "5x", "1.5s", "s", "5 s", " 5s", "ms"] {
            assert!(
                parse_duration(bad).is_err(),
                "expected `{bad}` to be rejected as a duration"
            );
        }
    }

    #[test]
    fn parse_duration_reports_overflow_instead_of_wrapping() {
        // A value that would overflow the millisecond count is an error, never a
        // wrapped-around tiny duration.
        assert!(parse_duration("99999999999999999999h").is_err());
        assert!(parse_duration(&format!("{}h", u64::MAX)).is_err());
    }

    // Property-based tier (T-167). Placed in this same `#[cfg(test)]` module
    // rather than a new `tests/properties.rs`: `parse_duration` is private to
    // this module, so an integration test under `tests/` — which links only
    // against the crate's public surface — cannot reach it; only an in-crate
    // unit test can. These run under the library unit-test tier (a plain
    // `cargo test`, i.e. `--lib`).
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            /// Unit equivalence across the documented grammar: `s` is 1000x `ms`,
            /// `m` is 60x `s`, `h` is 60x `m`, and a bare number defaults to `s` —
            /// for any value small enough that none of the multiplications
            /// overflow `u64` milliseconds.
            #[test]
            fn unit_equivalence_holds(value in 0u64..1_000_000) {
                let bare = parse_duration(&value.to_string()).unwrap();
                let secs = parse_duration(&format!("{value}s")).unwrap();
                let millis = parse_duration(&format!("{}ms", value * 1_000)).unwrap();
                let mins = parse_duration(&format!("{value}m")).unwrap();
                let mins_as_secs = parse_duration(&format!("{}s", value * 60)).unwrap();
                let hours = parse_duration(&format!("{value}h")).unwrap();
                let hours_as_mins = parse_duration(&format!("{}m", value * 60)).unwrap();

                prop_assert_eq!(bare, secs, "a bare number must default to seconds");
                prop_assert_eq!(secs, millis, "`Ns` must equal `(N*1000)ms`");
                prop_assert_eq!(mins, mins_as_secs, "`Nm` must equal `(N*60)s`");
                prop_assert_eq!(hours, hours_as_mins, "`Nh` must equal `(N*60)m`");
            }

            /// Any string that does not start with an ASCII digit is rejected: the
            /// grammar requires a leading digit run, so `raw.find` locating the
            /// first non-digit at index 0 always leaves `number` empty.
            #[test]
            fn non_digit_leading_input_is_rejected(raw in "[^0-9]{0,32}") {
                prop_assert!(parse_duration(&raw).is_err());
            }

            /// A digit run followed by any suffix outside the four documented
            /// units is rejected rather than silently reinterpreted.
            #[test]
            fn digits_with_unknown_unit_are_rejected(
                value in 0u64..1_000_000,
                unit in "[a-zA-Z]{1,8}",
            ) {
                prop_assume!(!matches!(unit.as_str(), "ms" | "s" | "m" | "h"));
                let raw = format!("{value}{unit}");
                prop_assert!(parse_duration(&raw).is_err());
            }

            /// No input — arbitrary, not just grammar-shaped — ever makes the
            /// parser panic; it always returns `Ok` or `Err`.
            #[test]
            fn never_panics_on_arbitrary_input(raw in ".{0,64}") {
                let _ = parse_duration(&raw);
            }
        }
    }

    #[test]
    fn run_parses_timeout_and_grace_into_durations() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--timeout",
            "5s",
            "--grace",
            "500ms",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.timeout, Some(Duration::from_secs(5)));
        assert_eq!(args.grace, Some(Duration::from_millis(500)));
    }

    #[test]
    fn run_parses_idle_timeout_into_a_duration_and_defaults_to_absent() {
        // `--idle-timeout` reuses `parse_duration`, so it accepts the same grammar
        // as `--timeout`/`--grace` and lands as a ready `Duration`.
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--idle-timeout",
            "2m",
            "--",
            "true",
        ])
        .expect("a valid run invocation with --idle-timeout");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.idle_timeout, Some(Duration::from_secs(120)));

        // Omitting it leaves it absent, so no idle deadline is armed.
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert!(
            args.idle_timeout.is_none(),
            "omitting --idle-timeout arms no idle deadline"
        );
    }

    #[test]
    fn run_parses_each_stdio_mode_and_rejects_incompatible_combinations() {
        let inherited_stdio = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--inherit-stdio",
            "--",
            "true",
        ])
        .expect("--inherit-stdio is a valid opt-in");
        let Command::Run(args) = inherited_stdio.command else {
            panic!("expected the run subcommand");
        };
        assert!(args.inherit_stdio);
        assert!(!args.inherit_stdin);
        assert!(args.stdin_file.is_none());

        let inherited = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--inherit-stdin",
            "--",
            "true",
        ])
        .expect("--inherit-stdin is a valid opt-in");
        let Command::Run(args) = inherited.command else {
            panic!("expected the run subcommand");
        };
        assert!(!args.inherit_stdio);
        assert!(args.inherit_stdin);
        assert!(args.stdin_file.is_none());

        let file = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--stdin-file",
            "input.txt",
            "--",
            "true",
        ])
        .expect("--stdin-file is a valid opt-in");
        let Command::Run(args) = file.command else {
            panic!("expected the run subcommand");
        };
        assert!(!args.inherit_stdio);
        assert!(!args.inherit_stdin);
        assert_eq!(args.stdin_file, Some(PathBuf::from("input.txt")));

        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--inherit-stdin",
                "--stdin-file",
                "input.txt",
                "--",
                "true",
            ])
            .is_err(),
            "the two stdin modes are contradictory and must fail at parse time"
        );

        for incompatible in [
            "--inherit-stdin",
            "--stdin-file",
            "--capture-dir",
            "--idle-timeout",
        ] {
            let mut argv = vec![
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--inherit-stdio",
                incompatible,
            ];
            // Give the value-taking flags a *valid* value so the only reason parsing
            // fails is the conflict itself, not a malformed value: a path for the
            // file/dir flags, a well-formed duration for `--idle-timeout`.
            if matches!(incompatible, "--stdin-file" | "--capture-dir") {
                argv.push("path");
            } else if incompatible == "--idle-timeout" {
                argv.push("5s");
            }
            argv.extend(["--", "true"]);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "--inherit-stdio must reject {incompatible}"
            );
        }

        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--inherit-stdio",
                "--create-no-window",
                "--",
                "true",
            ])
            .is_err(),
            "--inherit-stdio requires a usable Windows console when one exists"
        );
    }

    #[test]
    fn probe_requires_json_and_accepts_the_require_flags() {
        // The fixed form mirrors `inspect`: `--json` is mandatory.
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
    }

    #[test]
    fn parse_exit_code_band_accepts_and_rejects() {
        assert_eq!(parse_exit_code_band("100-119").unwrap(), (100, 119));
        assert_eq!(parse_exit_code_band("0-255").unwrap(), (0, 255));
        assert_eq!(parse_exit_code_band("110-110").unwrap(), (110, 110));
        // Missing separator, non-numeric, out-of-u8-range, and an inverted band all
        // fail loudly rather than being reinterpreted.
        for bad in ["100", "100+119", "a-119", "100-b", "100-999", "119-100"] {
            assert!(
                parse_exit_code_band(bad).is_err(),
                "expected `{bad}` to be rejected as an exit-code band"
            );
        }
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

    #[test]
    fn run_parses_env_flags_in_the_order_given() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--env-clear",
            "--env-remove",
            "FOO",
            "--env",
            "BAR=1",
            "--env",
            "BAZ=with=equals",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert!(args.env_clear);
        assert_eq!(args.env_remove, vec!["FOO".to_string()]);
        assert_eq!(
            args.env,
            vec![
                ("BAR".to_string(), "1".to_string()),
                ("BAZ".to_string(), "with=equals".to_string()),
            ]
        );
    }

    #[test]
    fn run_env_flags_default_to_absent() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert!(!args.env_clear);
        assert!(args.env_remove.is_empty());
        assert!(args.env.is_empty());
    }

    #[test]
    fn parse_env_kv_splits_on_the_first_equals() {
        assert_eq!(
            parse_env_kv("FOO=bar").unwrap(),
            ("FOO".to_string(), "bar".to_string())
        );
        assert_eq!(
            parse_env_kv("FOO=").unwrap(),
            ("FOO".to_string(), String::new())
        );
        assert_eq!(
            parse_env_kv("FOO=a=b=c").unwrap(),
            ("FOO".to_string(), "a=b=c".to_string())
        );
    }

    #[test]
    fn parse_env_kv_rejects_a_missing_separator_or_empty_key() {
        for bad in ["FOO", "", "=novalue"] {
            assert!(
                parse_env_kv(bad).is_err(),
                "expected `{bad}` to be rejected as a KEY=VALUE pair"
            );
        }
    }

    #[test]
    fn run_rejects_a_malformed_timeout() {
        // A bad duration is a form error, so parsing fails (mapped to USAGE by the
        // binary) rather than reaching the runner.
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--timeout",
                "soon",
                "--",
                "true",
            ])
            .is_err(),
            "a malformed --timeout must fail at parse time"
        );
    }

    #[test]
    fn run_rejects_a_malformed_idle_timeout() {
        // `--idle-timeout` shares `--timeout`'s parser, so a malformed value is the
        // same `USAGE` form error, rejected at parse time rather than mid-run.
        for bad in ["soon", "-5", "1.5s", "5x"] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    "--idle-timeout",
                    bad,
                    "--",
                    "true",
                ])
                .is_err(),
                "a malformed `--idle-timeout {bad}` must fail at parse time"
            );
        }
    }

    #[test]
    fn parse_size_accepts_bytes_and_binary_units() {
        assert_eq!(parse_size("1").unwrap(), 1);
        assert_eq!(parse_size("1048576").unwrap(), 1_048_576);
        assert_eq!(parse_size("512b").unwrap(), 512);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("2m").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("3g").unwrap(), 3 * 1024 * 1024 * 1024);
        // The unit is case-insensitive.
        assert_eq!(parse_size("512K").unwrap(), 512 * 1024);
    }

    #[test]
    fn parse_size_rejects_malformed_or_degenerate_values() {
        // Empty, non-numeric, signed, fractional, whitespace, an unknown unit, and a
        // bare unit all fail loudly. `0` (in any unit) is a degenerate cap ProcessKit
        // itself rejects, so the parser catches it as a form error before with_options.
        for bad in [
            "", "abc", "-5", "1.5m", "5 m", " 5m", "5x", "m", "0", "0k", "0m", "0g",
        ] {
            assert!(
                parse_size(bad).is_err(),
                "expected `{bad}` to be rejected as a size"
            );
        }
    }

    #[test]
    fn parse_size_reports_overflow_instead_of_wrapping() {
        assert!(parse_size(&format!("{}g", u64::MAX)).is_err());
        assert!(parse_size("99999999999999999999").is_err());
    }

    #[test]
    fn parse_max_processes_requires_a_positive_integer() {
        assert_eq!(parse_max_processes("1").unwrap(), 1);
        assert_eq!(parse_max_processes("64").unwrap(), 64);
        // `0`, negatives, fractions, and non-numbers are rejected at parse time.
        for bad in ["0", "-1", "1.5", "abc", "", "99999999999"] {
            assert!(
                parse_max_processes(bad).is_err(),
                "expected `{bad}` to be rejected as a process count"
            );
        }
    }

    #[test]
    fn parse_cpu_quota_requires_a_finite_positive() {
        assert_eq!(parse_cpu_quota("0.5").unwrap(), 0.5);
        assert_eq!(parse_cpu_quota("1").unwrap(), 1.0);
        assert_eq!(parse_cpu_quota("2").unwrap(), 2.0);
        // `0`, negatives, NaN, the infinities, and non-numbers are rejected — the
        // same values ProcessKit's `cpu_quota` validity rule treats as `Invalid`,
        // caught here at the CLI boundary instead.
        for bad in [
            "0", "0.0", "-1", "-0.5", "nan", "NaN", "inf", "infinity", "abc", "",
        ] {
            assert!(
                parse_cpu_quota(bad).is_err(),
                "expected `{bad}` to be rejected as a CPU quota"
            );
        }
    }

    #[test]
    fn run_parses_the_resource_limit_flags() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--max-memory",
            "256m",
            "--max-processes",
            "64",
            "--cpu-quota",
            "1.5",
            "--",
            "true",
        ])
        .expect("a valid run invocation with resource limits");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.max_memory, Some(256 * 1024 * 1024));
        assert_eq!(args.max_processes, Some(64));
        assert_eq!(args.cpu_quota, Some(1.5));
    }

    #[test]
    fn run_resource_limit_flags_default_to_absent() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert!(args.max_memory.is_none());
        assert!(args.max_processes.is_none());
        assert!(args.cpu_quota.is_none());
    }

    #[test]
    fn run_parses_capture_max_bytes_and_defaults_to_absent() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--capture-dir",
            "cap",
            "--capture-max-bytes",
            "64k",
            "--",
            "true",
        ])
        .expect("a valid run invocation with a custom capture ceiling");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.capture_max_bytes, Some(64 * 1024));

        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert!(
            args.capture_max_bytes.is_none(),
            "omitting --capture-max-bytes leaves it absent so the default ceiling applies"
        );
    }

    #[test]
    fn run_rejects_a_malformed_capture_max_bytes() {
        // Same grammar (and rejection) as `--max-memory` (`parse_size`), so a
        // malformed value fails loudly at parse time (mapped to USAGE) rather than
        // silently falling back to the default ceiling.
        for bad in ["0", "lots", "-5", "1.5m"] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    "--capture-max-bytes",
                    bad,
                    "--",
                    "true",
                ])
                .is_err(),
                "a malformed `--capture-max-bytes {bad}` must fail at parse time"
            );
        }
    }

    #[test]
    fn run_rejects_malformed_resource_limit_flags() {
        // Each nonsense value is a form error mapped to USAGE, never reaching the
        // runner as a mid-run limit failure.
        for (flag, value) in [
            ("--max-memory", "0"),
            ("--max-memory", "lots"),
            ("--max-processes", "0"),
            ("--max-processes", "-1"),
            ("--cpu-quota", "0"),
            ("--cpu-quota", "-1"),
            ("--cpu-quota", "nan"),
            ("--cpu-quota", "inf"),
        ] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    flag,
                    value,
                    "--",
                    "true",
                ])
                .is_err(),
                "a malformed `{flag} {value}` must fail at parse time"
            );
        }
    }
}
