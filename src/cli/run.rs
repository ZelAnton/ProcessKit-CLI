//! Arguments for the `run` subcommand — the runner's own surface, and by far the
//! largest and fastest-growing of the nine subcommands' argument sets (the
//! subcommand itself lives in [`crate::run`]).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, ValueEnum};

use crate::labels::OperatorLabel;

use super::parse::{
    parse_cpu_quota, parse_duration, parse_env_key, parse_env_kv, parse_max_processes,
    parse_positive_duration, parse_run_id, parse_size,
};

/// `run [--run-id <id>] [--cwd <dir>] --jsonl <events.jsonl> [--create-no-window]
/// [--windows-graceful-ctrl-break]
/// [--timeout <duration>] [--idle-timeout <duration>] [--grace <duration>]
/// [--snapshot-interval <duration>]
/// [--max-memory <size>] [--max-processes <n>] [--cpu-quota <cores>]
/// [--capture-dir <dir>] [--capture-max-bytes <size>]
/// [--capture-overflow <truncate|cancel>] [--no-echo] [--detach]
/// [--argv-raw] [--label <KEY=VALUE>] [--env-clear] [--env-remove <KEY>]
/// [--env-file <file>] [--env <KEY=VALUE>] [--run-id-env <KEY>]
/// [--inherit-stdio | --inherit-stdin | --stdin-file <file>]
/// -- <program> <args...>`
//
// `run` consumes every field: `cwd`, `create_no_window`,
// `windows_graceful_ctrl_break`, `timeout`,
// `idle_timeout`, `grace`, `snapshot_interval` — the opt-in periodic
// `members_snapshot` cadence (see `src/run/launch.rs`'s `snapshot_cadence`) —
// `max_memory`, `max_processes`, `cpu_quota` — the
// whole-tree ProcessKit resource caps (see `src/run/launch.rs`) — `command`, `jsonl`,
// `run_id`, `argv_raw`, `capture_dir`/`capture_max_bytes`/`capture_overflow` —
// bounded stdout/stderr
// capture to files, and what happens at the ceiling (see `src/capture.rs`) —
// `no_echo` — suppress the live echo
// while capture/idle-timeout keep observing the same bytes (see `src/run/launch.rs`) —
// `detach` — hand the whole run to a re-spawned, detached copy of this binary and
// return as soon as it has provably started (see `src/run/detach.rs`) —
// `labels`, `env_clear`, `env_remove`, `env_file`, `env`, `run_id_env` — publish the
// run's final id into the child's environment (see `src/run/launch.rs`) — and
// `inherit_stdio`/`inherit_stdin`/`stdin_file`.
//
// This synopsis and the field list above it are **normative** and have silently
// drifted apart before (K-006): re-derive both against the struct's full field
// list — not just the flags a change happens to touch — whenever a field is added.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Identifier for this run; a value is generated when omitted. Explicit ids
    /// are 1-256 characters and cannot contain terminal control or formatting
    /// characters.
    #[arg(long, value_name = "id", value_parser = parse_run_id)]
    pub run_id: Option<String>,

    /// Attach operator metadata as `KEY=VALUE` (repeatable). Later values replace
    /// earlier values for the same key in the registry and lifecycle event.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = crate::labels::parse)]
    pub labels: Vec<OperatorLabel>,

    /// Working directory for the child process.
    #[arg(long, value_name = "dir")]
    pub cwd: Option<PathBuf>,

    /// File to receive the versioned JSONL lifecycle events (never stdout).
    #[arg(long, value_name = "events.jsonl")]
    pub jsonl: PathBuf,

    /// Windows: create the child with CREATE_NO_WINDOW.
    #[arg(long)]
    pub create_no_window: bool,

    /// Windows: launch the child as a console process-group leader so ProcessKit's
    /// graceful stop can send it `CTRL_BREAK` before the hard Job Object kill. This
    /// is opt-in because it changes Windows console-signal routing. It is a no-op on
    /// other platforms and conflicts with modes that deliberately provide no shared
    /// console (`--create-no-window` and `--detach`).
    #[arg(
        long,
        conflicts_with_all = ["create_no_window", "detach"]
    )]
    pub windows_graceful_ctrl_break: bool,

    /// Hard deadline for the whole run; the tree is torn down when it elapses. A
    /// value of `0` is rejected at parse time (see [`parse_positive_duration`]) —
    /// it would arm a deadline that is already elapsed on the first poll, almost
    /// certainly an operator typo rather than an intentional immediate teardown.
    // Parsed and validated *here*, at the CLI layer, rather than deferred to the
    // runner: a malformed duration is a form error like any other bad flag, so it
    // belongs with the parsing surface (this module) and surfaces as the same
    // documented `USAGE` (100) exit, not a mid-run failure. `run` then receives an
    // already-validated `Duration` and never re-parses a string. See
    // [`parse_positive_duration`] for the accepted grammar.
    #[arg(long, value_name = "duration", value_parser = parse_positive_duration)]
    pub timeout: Option<Duration>,

    /// Deadline on child **silence**: the tree is torn down when the child produces
    /// no observed output for this long. Unlike `--timeout` (a ceiling on the whole
    /// run), this deadline is *re-armed* on every chunk of the child's output, so a
    /// child that keeps talking is never reaped no matter how long it runs — only
    /// one that goes quiet past the window is (the classic "stuck build worker").
    /// An idle expiry reuses the same reserved `TIMEOUT` (106) exit and the same
    /// soft-stop → grace → hard-kill teardown as `--timeout`, told apart only by the
    /// `timeout` event's `reason` field (`idle` vs `overall`; see `docs/schema.md`).
    /// Same grammar and parse-time validation as `--timeout` — including the
    /// rejection of `0`, which would otherwise guarantee an immediate teardown
    /// (`remaining` saturates to zero) — see [`parse_positive_duration`].
    /// Cannot be combined with `--inherit-stdio`: under direct inheritance the runner
    /// runs no output pump, so there is no point at which to observe the child and
    /// re-arm the deadline — the flags conflict at parse time, like `--capture-dir`.
    #[arg(long, value_name = "duration", value_parser = parse_positive_duration)]
    pub idle_timeout: Option<Duration>,

    /// Grace period between a cancel/timeout and the hard kill. Same duration
    /// grammar as `--timeout`/`--idle-timeout`, but — unlike them — `0` stays
    /// legal here: it means "no pause", a real and useful setting, not a
    /// degenerate one, so this flag keeps using the more permissive
    /// [`parse_duration`] rather than [`parse_positive_duration`].
    #[arg(long, value_name = "duration", value_parser = parse_duration)]
    pub grace: Option<Duration>,

    /// Re-emit the `members_snapshot` JSONL event every `<duration>` for as long
    /// as the child runs, instead of only once right after it is spawned. This is
    /// **recorded** observability for the shape of the process tree over time —
    /// when the worker fleet ballooned, whether helpers lingered, what the tree
    /// looked like just before an idle deadline fired — for exactly the runs
    /// (long, detached, quiet) where nobody is likely to have been watching live
    /// with `inspect` at the interesting moment. Omit it and the run emits the
    /// single post-spawn snapshot it always did, at the same point in the stream
    /// and with no event added — but note that the event itself now always
    /// carries `reason` (`spawn`) and `read_error`, on the default path too:
    /// additive fields within schema v1, not an unchanged wire form (see
    /// `docs/schema.md`, "Versioning").
    ///
    /// Each periodic event is the ordinary `members_snapshot`, enriched through
    /// the same `ProcessGroup::members_info()` path as the post-spawn one and
    /// told apart from it only by its `reason` field (`interval` vs `spawn`; see
    /// `docs/schema.md`, "members_snapshot"). The cadence stops the moment the
    /// run's ending is decided, so a periodic snapshot never interleaves into the
    /// teardown events.
    ///
    /// A member read that fails does not invent a tree that was never observed:
    /// it emits that sample with `read_error: true` and an empty `members` list,
    /// the same honest degradation `cleanup_started`/`cleanup_finished` already
    /// apply, and the cadence continues. That flag is the contract, not the
    /// accompanying stderr warning — a `--detach`ed runner's stderr is `null`, so
    /// there the JSONL stream is the only channel that reports the failure at all.
    ///
    /// **The resulting stream is deliberately unbounded**: roughly
    /// `duration / interval` extra lines, each sized by the tree, with no ceiling,
    /// rotation, or sampling. On a day-long run a 1s cadence over a 100-process
    /// tree writes several hundred megabytes; pick the interval accordingly (see
    /// `docs/running-commands.md`, "Recorded tree snapshots", for the arithmetic).
    ///
    /// Composes with **every** I/O mode, `--inherit-stdio` included: a snapshot
    /// is a query of the container's own member list, not an observation of the
    /// output pump, so — unlike `--idle-timeout` — it needs no pipes and
    /// conflicts with nothing. Same duration grammar and parse-time validation as
    /// `--timeout`, including the rejection of `0`, which asks for an unbounded
    /// snapshot storm rather than a cadence — see [`parse_positive_duration`].
    #[arg(long, value_name = "duration", value_parser = parse_positive_duration)]
    pub snapshot_interval: Option<Duration>,

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
    /// with an optional binary unit — `1048576`, `512k`, `256m`, `2g`. Requires
    /// `--capture-dir`; omitting it leaves the default ceiling in place. Does not
    /// change the `output_captured` event's
    /// shape or the meaning of its `truncated` flag, which still just means "the
    /// stream outran whatever per-stream ceiling was in effect" (see
    /// `src/capture.rs` and `README.md`, "Bounded output capture").
    #[arg(
        long,
        value_name = "size",
        value_parser = parse_size,
        requires = "capture_dir"
    )]
    pub capture_max_bytes: Option<u64>,

    /// What to do when either capture stream exceeds `--capture-max-bytes`.
    /// `truncate` (the default when omitted) keeps the run alive and clips only
    /// the transcript; `cancel` ends the whole run through the same graceful
    /// teardown as a timeout. Requires `--capture-dir` because the capture byte
    /// ceiling is the source of this signal.
    #[arg(long, value_name = "policy", value_enum, requires = "capture_dir")]
    pub capture_overflow: Option<CaptureOverflowPolicy>,

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
            "stdin_file",
            "no_echo"
        ]
    )]
    pub inherit_stdio: bool,

    /// Suppress the child's live stdout/stderr echo on the runner's own stdout/
    /// stderr. The pipe + pump stay wired up exactly as without the flag — only
    /// the runner-side echo write is skipped — so `--capture-dir` still receives
    /// the child's bytes in full and `--idle-timeout` still re-arms on every
    /// observed chunk; only the live retransmission to the runner's own stdout/
    /// stderr is dropped. Meant for an embedding orchestrator that reads results
    /// from `--jsonl`/`--capture-dir` and finds the child's output, interleaved
    /// with its own, pure noise. Cannot be combined with `--inherit-stdio`, which
    /// runs no pump to suppress in the first place.
    #[arg(long, conflicts_with = "inherit_stdio")]
    pub no_echo: bool,

    /// Start the run **detached** from this invocation and return as soon as it has
    /// provably started, instead of staying the runner's parent for the whole run.
    /// The command re-spawns this binary — in a new session on Unix, with
    /// `DETACHED_PROCESS` on Windows — to do the actual run, waits until that copy
    /// has registered the run and written its `run_started` event to `--jsonl`, and
    /// then exits. From that point the run is supervised out of band, exactly like
    /// any other run: `--jsonl` for its events, `inspect`/`cancel`/`kill` for its
    /// control plane, `list` for discovery, and `wait` for its end.
    ///
    /// **The exit code changes meaning under this flag**, and only under it: it
    /// reports whether the run *started*, never how the child finished. A successful
    /// start is `0` even for a child that later fails, and a start that failed keeps
    /// the same reserved-band code the run itself would have reported. The child's
    /// real code stays fully available where a detached caller can actually observe
    /// it — the terminal `runner_exit` event in `--jsonl` (see
    /// `docs/exit-codes.md`, "Detached runs").
    ///
    /// Cannot be combined with the interactive stdio modes (`--inherit-stdio`,
    /// `--inherit-stdin`): a detached run has no terminal to hand over and nobody
    /// left to type at it. There is no live echo either — the detached runner runs
    /// with `--no-echo`'s discarding sinks — while `--jsonl` (still required),
    /// `--capture-dir`, and `--idle-timeout` all keep observing the child's output
    /// exactly as they do in the foreground, and `--snapshot-interval` keeps
    /// recording the tree's shape (this is the mode it exists for — nobody is
    /// watching a detached run live with `inspect`).
    ///
    /// **The detached runner's own stdin/stdout/stderr are `null`**, so every
    /// warning the foreground runner would print reaches nobody: `--jsonl` is the
    /// only diagnostic channel a detached run has. That is why a failed member read
    /// is recorded as a `members_snapshot` with `read_error: true` rather than only
    /// warned about — and why a `--jsonl` write failure (a full disk, say) is
    /// invisible: event logging switches itself off after one unseen warning and the
    /// stream simply ends mid-run.
    #[arg(long, conflicts_with_all = ["inherit_stdio", "inherit_stdin"])]
    pub detach: bool,

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
    /// after `--env-clear` and before `--env-file`/`--env`, so either later source
    /// can restore the same key. Maps onto `processkit::Command::env_remove()`.
    #[arg(long = "env-remove", value_name = "KEY")]
    pub env_remove: Vec<String>,

    /// Read child environment entries from a UTF-8 `KEY=VALUE` file (repeatable).
    /// Empty lines and lines whose first non-whitespace character is `#` are
    /// ignored. Files are applied in argument order after removals and before
    /// explicit `--env`, keeping their contents out of the runner's argv.
    #[arg(long = "env-file", value_name = "file")]
    pub env_file: Vec<PathBuf>,

    /// Set an environment variable for the child as `KEY=VALUE` (repeatable).
    /// Applied last — after `--env-clear` and `--env-remove` — so it always wins
    /// on a duplicated key. Maps onto `processkit::Command::env()`.
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env_kv)]
    pub env: Vec<(String, String)>,

    /// Set `<KEY>` in the child's environment to this run's **final** id — the
    /// explicit `--run-id` when one was given, otherwise the id the runner
    /// generated. That is the same value `run_started.run_id`, the registry
    /// record, and every control-plane reply carry, so a child (and its
    /// descendants) can name the run it belongs to without the caller minting an
    /// identity itself and passing it twice as `--run-id <id> --env KEY=<id>` —
    /// two copies that can drift, and which foreclose a runner-generated id
    /// entirely, since a generated id was previously not knowable until the run
    /// had already started.
    ///
    /// **Strictly opt-in.** Omit it and the child's environment is exactly what it
    /// has always been: there is no default key, and nothing is injected
    /// unconditionally. `<KEY>` follows the same rules as an `--env` KEY (see
    /// [`parse_env_key`]): non-empty, no whitespace or control characters, and no
    /// `=` — the value comes from the runner, not the command line.
    ///
    /// **Applied last, after all four `--env-*` flags** (`--env-clear`,
    /// `--env-remove`, `--env-file`, `--env`), so the injected id wins over an
    /// `--env-file` entry or an `--env-remove` naming the same key no matter which
    /// order those flags were written on the command line — see `README.md`,
    /// "Environment". The one collision that is *refused* rather than resolved is
    /// an explicit `--env <KEY>=…` for this very key: two flags asking for two
    /// different values of one variable is a caller mistake, so it fails at parse
    /// time as a `USAGE` (100) error instead of silently discarding the value the
    /// caller typed (see [`RunArgs::validate`]). "The same key" means the same key
    /// *to this platform*: byte-exact off Windows, and case-insensitive on Windows,
    /// where `KEY` and `key` are one child variable rather than two.
    ///
    /// **The value is correlation data, not a credential.** It proves nothing
    /// about who started the run, carries no authority, and is trivially forgeable
    /// by anything that can set an environment variable; the child's own
    /// environment is visible to the child and, on some platforms, to other
    /// processes of the same user. Use it to *label* work, never to authorize it.
    ///
    /// Composes with `--detach`: the detached copy is re-spawned with an explicit
    /// `--run-id` for the very id its caller reported, so the child of a detached
    /// run observes the same value as the one in `--jsonl`.
    #[arg(long = "run-id-env", value_name = "KEY", value_parser = parse_env_key)]
    pub run_id_env: Option<String>,

    /// The program to run followed by its arguments. Everything after `--` is
    /// taken verbatim — there is no shell mode, so nothing here is expanded or
    /// re-interpreted. Kept as `OsString`s to preserve bytes exactly.
    #[arg(last = true, required = true, num_args = 1.., value_name = "program")]
    pub command: Vec<OsString>,
}

impl RunArgs {
    /// The `run`-specific half of [`super::Cli::validate`]: the cross-argument
    /// rules that compare two flags' **values** rather than their presence, which
    /// clap's declarative `conflicts_with` cannot express.
    ///
    /// One rule today. `--run-id-env <KEY>` and an explicit `--env <KEY>=<value>`
    /// for the same key are refused as a pair, because they ask for two different
    /// values of one child variable and only one of them can survive. The
    /// alternative — silently letting the injected run id win, which is what the
    /// builder order in `src/run/launch.rs` would do — is deterministic but
    /// discards, without a word, a value the caller explicitly typed. Refusing is
    /// the more predictable contract for the calling side (an adapter learns it
    /// asked for something impossible *before* a child runs, rather than
    /// discovering a dropped variable in production), and it is the choice
    /// `README.md` and `docs/running-commands.md` document.
    ///
    /// Whether two keys *are* the same key is decided the way the platform decides
    /// it — [`names_one_child_variable`] — because that, not byte equality, is what
    /// settles whether one child variable or two are at stake. Byte equality on
    /// Windows would refuse `--env RID=mine --run-id-env RID` while accepting
    /// `--env RID=mine --run-id-env rid`, and the accepted pair would then resolve
    /// in exactly the way this refusal exists to prevent: one case-insensitive
    /// child variable, the injected id landing last and winning, and `mine` gone
    /// without a word.
    ///
    /// Deliberately **not** refused, because those are resolved rather than
    /// contradictory (`README.md`, "Environment"):
    ///
    /// - `--env-remove <KEY>` with the same key: removals are applied before every
    ///   set, so the injected id lands afterwards and wins — exactly as an explicit
    ///   `--env` already wins over an `--env-remove` of its key.
    /// - an `--env-file` entry for the same key: file contents are read at run
    ///   time, not at parse time, so there is nothing to compare here; the fixed
    ///   builder order settles it (the injection is applied last) regardless of
    ///   argument order.
    ///
    /// Returns the message clap renders on failure; the KEY is escaped and no
    /// environment **value** appears in it.
    pub fn validate(&self) -> Result<(), String> {
        let Some(key) = self.run_id_env.as_deref() else {
            return Ok(());
        };
        let Some((env_key, _)) = self
            .env
            .iter()
            .find(|(env_key, _)| names_one_child_variable(env_key, key))
        else {
            return Ok(());
        };
        // Each flag is quoted back with the spelling the caller actually typed, so
        // the message never invents an `--env` entry they did not write. The two can
        // only differ where the platform folds case (Windows today), and there the
        // difference is precisely what needs explaining.
        let case_note = if env_key == key {
            ""
        } else {
            " (environment variable names are case-insensitive on Windows)"
        };
        Err(format!(
            "`--run-id-env {}` and `--env {}=…` both set the same child variable{case_note}: \
             the run id would always win, whichever order the two flags were given. \
             Drop one of them — `--run-id-env` to keep your own value, or the `--env` \
             entry to let the runner publish this run's id",
            key.escape_debug(),
            env_key.escape_debug()
        ))
    }
}

/// Whether two child-environment variable **names** address one and the same
/// variable *on this platform* — the question [`RunArgs::validate`]'s refusal turns
/// on, and the one place this crate decides it.
///
/// Windows environment blocks are case-insensitive: `PATH`, `Path`, and `path` name
/// one variable there, and the environment map `std::process::Command` builds for a
/// child folds them together the same way. So a pair differing only in case is the
/// same contradiction as an identical spelling, and comparing bytes there would let
/// the injected run id silently replace an explicit `--env` value for every
/// spelling but one — the outcome the refusal exists to prevent.
///
/// Off Windows the comparison stays byte-exact, because names are genuinely
/// case-sensitive there: `PATH` and `Path` are two variables, and refusing that pair
/// would reject a legal invocation over a collision that does not exist.
#[cfg(windows)]
fn names_one_child_variable(left: &str, right: &str) -> bool {
    // Compared per character rather than with `str::eq_ignore_ascii_case`, because
    // the shared key grammar (`super::parse_env_key`) accepts non-ASCII names, and
    // `é`/`É` is one Windows variable exactly as `a`/`A` is — an ASCII-only fold
    // would leave that class of spellings in the silent-substitution hole this
    // function closes.
    left.chars()
        .map(simple_uppercase)
        .eq(right.chars().map(simple_uppercase))
}

/// The **simple** (one-to-one) uppercase form of `character`, matching how Windows
/// folds an environment name per character, rather than Unicode's full mapping:
/// where the full mapping produces more than one character (`ß` → `SS`), the
/// platform's own table leaves the character alone, so this does too.
///
/// The two tables can still disagree on exotic code points; that residue is far
/// narrower than the whole-case axis it replaces, and it cannot reach a name in the
/// conventional `[A-Za-z0-9_]` shape.
#[cfg(windows)]
fn simple_uppercase(character: char) -> char {
    let mut mapped = character.to_uppercase();
    match (mapped.next(), mapped.next()) {
        (Some(single), None) => single,
        _ => character,
    }
}

/// The byte-exact half of the same rule (see the Windows arm above): off Windows,
/// environment variable names are case-sensitive, so `PATH` and `Path` are two
/// child variables and only the identical spelling is a collision.
#[cfg(not(windows))]
fn names_one_child_variable(left: &str, right: &str) -> bool {
    left == right
}

/// Policy applied when a bounded output transcript reaches its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CaptureOverflowPolicy {
    /// Preserve the historical behavior: clip the transcript and keep running.
    Truncate,
    /// Gracefully stop the run, then hard-kill survivors after `--grace`.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::cli::{Cli, Command};

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
    fn run_rejects_zero_timeout_and_idle_timeout_but_accepts_zero_grace() {
        // `--timeout 0`/`--idle-timeout 0` would arm a deadline that expires on the
        // very first poll — almost certainly a typo — and is rejected at parse
        // time (see `parse_positive_duration`).
        let zero_timeout = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--timeout",
            "0",
            "--",
            "true",
        ]);
        assert!(
            zero_timeout.is_err(),
            "--timeout 0 must be rejected as a degenerate deadline"
        );

        let zero_idle_timeout = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--idle-timeout",
            "0",
            "--",
            "true",
        ]);
        assert!(
            zero_idle_timeout.is_err(),
            "--idle-timeout 0 must be rejected as a degenerate deadline"
        );

        // `--grace 0` stays legal — "no pause" between the soft stop and the hard
        // kill is a meaningful setting, not a degenerate one.
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--grace",
            "0",
            "--",
            "true",
        ])
        .expect("--grace 0 is a legal 'no pause' setting");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.grace, Some(Duration::ZERO));
    }

    #[test]
    fn run_parses_idle_timeout_into_a_duration_and_defaults_to_absent() {
        // `--idle-timeout` reuses `parse_positive_duration`, so it accepts the same
        // grammar as `--timeout` (a superset of `--grace`'s, minus `0`) and lands
        // as a ready `Duration`.
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
    fn run_parses_snapshot_interval_into_a_duration_and_defaults_to_absent() {
        // `--snapshot-interval` reuses `parse_positive_duration`, the one duration
        // parser this project has (K-048): same grammar as `--timeout`, landing as
        // a ready `Duration` rather than a string the runner would re-parse.
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--snapshot-interval",
            "30s",
            "--",
            "true",
        ])
        .expect("a valid run invocation with --snapshot-interval");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.snapshot_interval, Some(Duration::from_secs(30)));

        // Omitting it arms no cadence, so the run emits exactly the one post-spawn
        // `members_snapshot` it always did.
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
            args.snapshot_interval.is_none(),
            "omitting --snapshot-interval arms no periodic snapshot cadence"
        );
    }

    #[test]
    fn run_rejects_a_zero_or_malformed_snapshot_interval() {
        // `--snapshot-interval 0` is rejected exactly the way `--timeout 0` is (the
        // shared `parse_positive_duration`): a zero cadence is not a fast cadence,
        // it is an unbounded snapshot storm, and almost certainly a typo.
        for zero in ["0", "0ms", "0s", "0m", "0h"] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    "--snapshot-interval",
                    zero,
                    "--",
                    "true",
                ])
                .is_err(),
                "`--snapshot-interval {zero}` must be rejected as a degenerate cadence"
            );
        }

        // A malformed value is the same `USAGE` form error as for `--timeout`,
        // caught at parse time rather than mid-run.
        for bad in ["soon", "-5", "1.5s", "5x", "5 s"] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    "--snapshot-interval",
                    bad,
                    "--",
                    "true",
                ])
                .is_err(),
                "a malformed `--snapshot-interval {bad}` must fail at parse time"
            );
        }
    }

    /// The cadence samples the *container*, never the output pump, so — unlike
    /// `--idle-timeout`, which conflicts with `--inherit-stdio` because direct
    /// inheritance leaves no point at which to observe the child — it composes
    /// with every I/O mode and declares no conflict at all.
    #[test]
    fn run_snapshot_interval_composes_with_every_io_mode() {
        for io_mode in [
            vec!["--inherit-stdio"],
            vec!["--inherit-stdin"],
            vec!["--no-echo"],
            vec!["--capture-dir", "cap"],
            vec!["--detach"],
        ] {
            let mut argv = vec![
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--snapshot-interval",
                "1s",
            ];
            argv.extend_from_slice(&io_mode);
            argv.extend(["--", "true"]);
            let parsed = Cli::try_parse_from(argv)
                .unwrap_or_else(|err| panic!("--snapshot-interval must accept {io_mode:?}: {err}"));
            let Command::Run(args) = parsed.command else {
                panic!("expected the run subcommand");
            };
            assert_eq!(args.snapshot_interval, Some(Duration::from_secs(1)));
        }
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
            "--no-echo",
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
    fn run_parses_detach_and_rejects_the_interactive_stdio_modes() {
        // The flag is off by default, so a run that does not ask for it keeps the
        // foreground contract (this invocation stays the runner).
        let plain = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "true",
        ])
        .expect("a valid run invocation");
        let Command::Run(args) = plain.command else {
            panic!("expected the run subcommand");
        };
        assert!(!args.detach, "--detach is opt-in, never the default");

        let detached = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--detach",
            "--",
            "true",
        ])
        .expect("--detach is a valid opt-in");
        let Command::Run(args) = detached.command else {
            panic!("expected the run subcommand");
        };
        assert!(args.detach);

        // The interactive stdio modes are contradictory with detaching — there is no
        // terminal to hand over and no interactive caller left — so the conflict is
        // caught at parse time, not discovered mid-run.
        for incompatible in ["--inherit-stdio", "--inherit-stdin"] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    "--detach",
                    incompatible,
                    "--",
                    "true",
                ])
                .is_err(),
                "--detach must reject {incompatible}"
            );
        }

        // Everything a detached run *can* still observe the child with stays legal
        // alongside it — including `--no-echo`, whose sinks the detached runner
        // reuses, so passing it explicitly is never a conflict.
        let combined = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--detach",
            "--no-echo",
            "--capture-dir",
            "capture",
            "--idle-timeout",
            "5s",
            "--stdin-file",
            "input.txt",
            "--",
            "true",
        ])
        .expect("--detach composes with capture, idle-timeout, no-echo, and stdin-file");
        let Command::Run(args) = combined.command else {
            panic!("expected the run subcommand");
        };
        assert!(args.detach && args.no_echo);
        assert_eq!(args.capture_dir, Some(PathBuf::from("capture")));
        assert_eq!(args.idle_timeout, Some(Duration::from_secs(5)));
        assert_eq!(args.stdin_file, Some(PathBuf::from("input.txt")));

        // `--jsonl` stays required under `--detach`: it is the only channel a
        // detached caller has left to observe the run's outcome on.
        assert!(
            Cli::try_parse_from(["processkit-cli", "run", "--detach", "--", "true"]).is_err(),
            "--detach does not make the required --jsonl optional"
        );
    }

    #[test]
    fn run_parses_windows_graceful_ctrl_break_and_rejects_consoleless_modes() {
        let parsed = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--windows-graceful-ctrl-break",
            "--",
            "worker",
        ])
        .expect("the graceful Windows console opt-in parses");
        let Command::Run(args) = parsed.command else {
            panic!("expected run");
        };
        assert!(args.windows_graceful_ctrl_break);

        for incompatible in ["--create-no-window", "--detach"] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    "--windows-graceful-ctrl-break",
                    incompatible,
                    "--",
                    "worker",
                ])
                .is_err(),
                "{incompatible} removes the shared console needed for CTRL_BREAK"
            );
        }
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
            "--env-file",
            "base.env",
            "--env-file",
            "secrets.env",
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
            args.env_file,
            vec![PathBuf::from("base.env"), PathBuf::from("secrets.env")]
        );
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
        assert!(args.env_file.is_empty());
        assert!(args.env.is_empty());
    }

    #[test]
    fn run_rejects_an_environment_key_with_whitespace() {
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--env",
                "BAD KEY=value",
                "--",
                "true",
            ])
            .is_err(),
            "invalid environment keys must fail at the CLI boundary"
        );
    }

    /// Parse a `run` invocation built from the mandatory bits plus `flags`, and
    /// hand back its arguments. Panics on a parse failure, so a test that is about
    /// *accepted* command lines does not have to unwrap at every call.
    fn run_args(flags: &[&str]) -> RunArgs {
        let mut argv = vec!["processkit-cli", "run", "--jsonl", "events.jsonl"];
        argv.extend_from_slice(flags);
        argv.extend(["--", "true"]);
        let cli = Cli::try_parse_from(&argv).unwrap_or_else(|err| panic!("{argv:?}: {err}"));
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        *args
    }

    /// Whether the whole command line — parse **and** the cross-argument rules
    /// `Cli::validate` applies right after it (`src/main.rs`) — is accepted. This is
    /// what a caller actually observes: both refusals are the same `USAGE` (100)
    /// exit with clap's own rendering, so the tests below assert on the pair rather
    /// than on which of the two layers happened to catch the mistake.
    fn run_is_accepted(flags: &[&str]) -> bool {
        let mut argv = vec!["processkit-cli", "run", "--jsonl", "events.jsonl"];
        argv.extend_from_slice(flags);
        argv.extend(["--", "true"]);
        Cli::try_parse_from(&argv).is_ok_and(|cli| cli.validate().is_ok())
    }

    #[test]
    fn run_parses_run_id_env_and_defaults_to_absent() {
        let args = run_args(&["--run-id-env", "PROCESSKIT_RUN_ID"]);
        assert_eq!(args.run_id_env.as_deref(), Some("PROCESSKIT_RUN_ID"));
        assert_eq!(args.validate(), Ok(()));

        // Strictly opt-in: without the flag there is no key at all, so no run
        // publishes its id into a child environment by default.
        let args = run_args(&[]);
        assert!(
            args.run_id_env.is_none(),
            "omitting --run-id-env injects nothing into the child environment"
        );
        assert_eq!(args.validate(), Ok(()));
    }

    #[test]
    fn run_rejects_a_malformed_run_id_env_key() {
        // The same KEY form `--env` demands (the shared `parse_env_key`/
        // `validate_env_key`), plus the `KEY=VALUE` spelling a caller might reach
        // for out of habit: the value is the runner's, so a pair is a
        // misunderstanding rather than a harmless redundancy.
        for bad in ["", "BAD KEY", "TAB\tKEY", "LINE\nKEY", "KEY=VALUE", "KEY="] {
            assert!(
                Cli::try_parse_from([
                    "processkit-cli",
                    "run",
                    "--jsonl",
                    "events.jsonl",
                    "--run-id-env",
                    bad,
                    "--",
                    "true",
                ])
                .is_err(),
                "a malformed `--run-id-env {bad:?}` must fail at parse time"
            );
        }
    }

    /// `--run-id-env` is one more source feeding the same environment builder, not a
    /// mode: it composes with all four `--env-*` flags, and — since the applied order
    /// is fixed in `src/run/launch.rs` rather than read off the command line — the
    /// parsed result cannot depend on where it was written among them.
    ///
    /// This pins the **parse** only: which fields a command line populates. What the
    /// child ends up observing when those flags meet on one key is decided by the
    /// builder chain in `src/run/launch.rs`, out of this module's reach, and is
    /// proven through the built binary in `tests/run.rs` —
    /// `run_id_env_is_set_on_a_slate_env_clear_emptied`,
    /// `run_id_env_outlives_an_env_remove_for_the_same_key`,
    /// `run_id_env_wins_over_an_env_file_entry_for_the_same_key`, and
    /// `run_id_env_wins_over_every_other_environment_flag_at_once`.
    #[test]
    fn run_id_env_composes_with_every_environment_flag_in_any_order() {
        let env_flags: [&[&str]; 2] = [
            &[
                "--env-clear",
                "--env-remove",
                "STALE",
                "--env-file",
                "base.env",
                "--env",
                "MODE=ci",
                "--run-id-env",
                "PROCESSKIT_RUN_ID",
            ],
            // The same invocation with the new flag written *first* instead of last.
            &[
                "--run-id-env",
                "PROCESSKIT_RUN_ID",
                "--env-clear",
                "--env-remove",
                "STALE",
                "--env-file",
                "base.env",
                "--env",
                "MODE=ci",
            ],
        ];
        for flags in env_flags {
            let args = run_args(flags);
            assert_eq!(args.validate(), Ok(()), "{flags:?}");
            assert!(args.env_clear);
            assert_eq!(args.env_remove, vec!["STALE".to_string()]);
            assert_eq!(args.env_file, vec![PathBuf::from("base.env")]);
            assert_eq!(args.env, vec![("MODE".to_string(), "ci".to_string())]);
            assert_eq!(args.run_id_env.as_deref(), Some("PROCESSKIT_RUN_ID"));
        }
    }

    /// The collision decision (T-304): an explicit `--env KEY=…` for the very key
    /// `--run-id-env` targets is **refused at parse time**, in either order, rather
    /// than resolved by silently discarding the caller's value. A removal or a file
    /// naming the same key is *not* refused — those have a documented applied order
    /// that already settles them (the injection lands last and wins).
    #[test]
    fn run_id_env_refuses_a_duplicate_explicit_env_key_whatever_the_flag_order() {
        assert!(
            !run_is_accepted(&["--env", "SHARED=mine", "--run-id-env", "SHARED"]),
            "an explicit --env for the injected key is a contradiction, not a default"
        );
        assert!(
            !run_is_accepted(&["--run-id-env", "SHARED", "--env", "SHARED=mine"]),
            "the refusal cannot depend on which flag was written first"
        );

        // A *different* key is no collision at all, in either order.
        assert!(run_is_accepted(&[
            "--env",
            "OTHER=mine",
            "--run-id-env",
            "SHARED"
        ]));
        assert!(run_is_accepted(&[
            "--run-id-env",
            "SHARED",
            "--env",
            "OTHER=mine"
        ]));

        // Removals and files are resolved by the applied order, not refused.
        assert!(run_is_accepted(&[
            "--env-remove",
            "SHARED",
            "--run-id-env",
            "SHARED"
        ]));
        assert!(run_is_accepted(&[
            "--run-id-env",
            "SHARED",
            "--env-remove",
            "SHARED"
        ]));
        assert!(run_is_accepted(&[
            "--env-file",
            "base.env",
            "--run-id-env",
            "SHARED"
        ]));

        // And with the flag absent, an `--env` for any key stays exactly as legal as
        // it was before this flag existed.
        assert!(run_is_accepted(&["--env", "SHARED=mine"]));
    }

    /// The refusal turns on what the *platform* calls one variable, not on byte
    /// equality (T-304, R-01). Windows environment blocks are case-insensitive, so
    /// `--env RID_TEST=mine --run-id-env rid_test` is the very contradiction the
    /// rule is about: accepting it resolves the pair the one way the refusal exists
    /// to prevent — one child variable, the injected id landing last, and the
    /// caller's own `mine` discarded without a word.
    #[cfg(windows)]
    #[test]
    fn run_id_env_refuses_a_case_variant_env_key_on_windows() {
        for (entry, injected) in [
            ("RID_TEST=mine", "rid_test"),
            ("rid_test=mine", "RID_TEST"),
            ("Rid_Test=mine", "rID_tEST"),
            // The shared key grammar accepts non-ASCII names, and Windows folds
            // their case too, so an ASCII-only comparison would leave this class of
            // spellings in exactly the hole the rule closes.
            ("RID_É=mine", "rid_é"),
        ] {
            assert!(
                !run_is_accepted(&["--env", entry, "--run-id-env", injected]),
                "`--env {entry}` and `--run-id-env {injected}` name one Windows variable"
            );
            assert!(
                !run_is_accepted(&["--run-id-env", injected, "--env", entry]),
                "the refusal cannot depend on which flag was written first"
            );
        }

        // Folding case is not fuzzy matching: names differing by more than case stay
        // two variables, and naming both remains as legal as it ever was.
        assert!(run_is_accepted(&[
            "--env",
            "RID_TEST=mine",
            "--run-id-env",
            "rid_test_2"
        ]));
        assert!(run_is_accepted(&[
            "--env",
            "RID_TEST=mine",
            "--run-id-env",
            "other"
        ]));

        // The diagnostic quotes each flag with the spelling the caller actually
        // typed — it must not render an `--env` entry they never wrote — and still
        // never repeats the value beside it.
        let args = run_args(&["--env", "RID_TEST=mine", "--run-id-env", "rid_test"]);
        let error = args.validate().expect_err("the pair is refused");
        assert!(
            error.contains("`--run-id-env rid_test`") && error.contains("`--env RID_TEST=…`"),
            "each flag is quoted as it was written: {error:?}"
        );
        assert!(
            error.contains("case-insensitive on Windows"),
            "two visibly different keys are one variable, and the message says why: {error:?}"
        );
        assert!(
            !error.contains("mine"),
            "the refusal must not disclose the --env value: {error:?}"
        );
    }

    /// The mirror of the rule above off Windows, where environment names really are
    /// case-sensitive: `RID_TEST` and `rid_test` are two child variables, so the
    /// pair is not a collision at all and refusing it would reject a legal
    /// invocation. The identical spelling is still refused.
    #[cfg(not(windows))]
    #[test]
    fn run_id_env_treats_a_case_variant_env_key_as_a_second_variable_off_windows() {
        assert!(run_is_accepted(&[
            "--env",
            "RID_TEST=mine",
            "--run-id-env",
            "rid_test"
        ]));
        assert!(run_is_accepted(&[
            "--run-id-env",
            "rid_test",
            "--env",
            "RID_TEST=mine"
        ]));
        assert!(!run_is_accepted(&[
            "--env",
            "RID_TEST=mine",
            "--run-id-env",
            "RID_TEST"
        ]));
    }

    #[test]
    fn run_id_env_collision_error_names_the_key_but_never_the_value() {
        // The same secret-safety invariant the environment parsers hold
        // (`parse_env_kv_errors_never_repeat_the_value`): a caller who collided a
        // secret-bearing `--env` with `--run-id-env` must not see that secret echoed
        // back into a terminal, a CI log, or a captured stderr transcript.
        let secret = "do-not-print-this-secret";
        let args = run_args(&[
            "--env",
            &format!("SHARED={secret}"),
            "--run-id-env",
            "SHARED",
        ]);
        let error = args.validate().expect_err("the pair is refused");
        assert!(
            !error.contains(secret),
            "the refusal must not disclose the --env value: {error:?}"
        );
        assert!(
            error.contains("SHARED"),
            "the refusal names the key it is about: {error:?}"
        );
        assert!(
            !error.chars().any(char::is_control),
            "the refusal stays on one terminal line: {error:?}"
        );
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
    fn run_requires_capture_dir_for_capture_max_bytes() {
        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--capture-max-bytes",
                "64k",
                "--",
                "true",
            ])
            .is_err(),
            "a capture ceiling without a capture destination is never silently inert"
        );
    }

    #[test]
    fn run_parses_capture_overflow_policy_and_requires_capture() {
        let cli = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--capture-dir",
            "cap",
            "--capture-overflow",
            "cancel",
            "--",
            "true",
        ])
        .expect("cancel overflow policy with capture is valid");
        let Command::Run(args) = cli.command else {
            panic!("expected the run subcommand");
        };
        assert_eq!(args.capture_overflow, Some(CaptureOverflowPolicy::Cancel));

        assert!(
            Cli::try_parse_from([
                "processkit-cli",
                "run",
                "--jsonl",
                "events.jsonl",
                "--capture-overflow",
                "cancel",
                "--",
                "true",
            ])
            .is_err(),
            "an overflow policy without a capture source must fail at parse time"
        );

        let Command::Run(defaults) = Cli::try_parse_from([
            "processkit-cli",
            "run",
            "--jsonl",
            "events.jsonl",
            "--",
            "true",
        ])
        .expect("a valid default run")
        .command
        else {
            panic!("expected the run subcommand");
        };
        assert!(
            defaults.capture_overflow.is_none(),
            "omission preserves the historical truncate-and-continue behavior"
        );
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
