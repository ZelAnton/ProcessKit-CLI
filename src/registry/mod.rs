//! The per-user run registry — the first brick of the control plane.
//!
//! ProcessKit-cli's control plane lives in the *live* `run` process, not in named
//! kernel objects (`AGENTS.md`, "The control plane lives in the live runner
//! process"). Future `inspect` / `cancel` / `kill` clients (T-008/T-009) find a
//! live runner by consulting this registry — a **per-user directory** of one record
//! per in-flight run. Three properties are load-bearing and each is realized here:
//!
//! - **Owner-only access.** The registry directory is created restricted to its
//!   owner — unix mode `0700`, and on Windows a *protected* DACL that grants only
//!   the current user (see [`platform`]). A record names a run's local transport
//!   endpoint, so a world-readable registry would leak a control channel to any
//!   local process. [`Registry::open`] (the mutating path a run about to write a
//!   record uses) re-asserts the restriction on every call so a pre-existing
//!   directory is locked down too; [`Registry::open_read_only`] (the path every
//!   read-only client takes — `list`/`prune`/`wait` and the control clients)
//!   deliberately does neither — a read-only scan must not create the directory or
//!   touch its permissions.
//! - **No PID addressing.** A record is never indexed or identified by a bare PID
//!   (`AGENTS.md`: "Nothing is addressed by PID, which is what makes PID reuse
//!   irrelevant"). Entries are found by scanning records and matching their
//!   `run_id`; the on-disk file name is an opaque, PID-free token. PID reuse
//!   therefore cannot alias one run onto another.
//! - **Detectable staleness — not mere file existence.** If a runner dies abruptly
//!   the kernel container reaps the process tree, but the record file is left
//!   behind. A client must be able to tell that leftover record from a live one
//!   *without* relying on the file merely existing. The signal is an **OS advisory
//!   lock**: the live runner holds an exclusive lock on the record's sibling lock
//!   file for the whole run, and the OS releases that lock automatically when the
//!   process dies — abruptly or not. A client probes liveness by trying to take the
//!   lock: it can only succeed when no live runner holds it, i.e. the entry is
//!   stale (see [`Registry::entries`] and [`Health`]).
//!
//! The connection *endpoint* names the run's local control transport (a unix socket
//! path, or a Windows named-pipe name — see [`crate::control`]). A live runner
//! publishes it here so a client can reach it; it is `None` only when the transport
//! could not be stood up (best-effort degradation, the run still works).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::events;
use crate::exit::{self, RunnerError};

/// Open the registry for a whole-registry CLI operation and map the prerequisite
/// failure onto the shared `SETUP` contract. By-run-id control clients deliberately
/// keep their distinct `CONTROL` mapping in `control`.
pub(crate) fn open_read_only_for_setup() -> Result<Registry, RunnerError> {
    Registry::open_read_only().map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not open the run registry: {err}"),
        )
    })
}

/// Map a whole-registry scan failure onto the shared `SETUP` diagnostic used by
/// `list`, `prune`, `wait`, and aggregate control operations.
pub(crate) fn setup_read_error(err: io::Error) -> RunnerError {
    RunnerError::new(
        exit::SETUP,
        format!("could not read the run registry: {err}"),
    )
}

/// On-disk record format version. Independent of the JSONL event
/// [`schema_version`](crate::events::SCHEMA_VERSION): the registry is a private
/// per-user contract between a runner and its own control-plane clients, not the
/// public event stream, so it versions on its own axis.
///
/// **Why adding [`Record::argv_sha256`]/[`Record::hint`] (T-215) did not bump it.**
/// A version exists to tell a reader "you cannot correctly interpret what follows",
/// and neither direction of this change can mislead a reader:
///
/// - **New record, old reader.** `Record` has no `deny_unknown_fields`, so a reader
///   built before these fields existed ignores them and reads every field it knows
///   exactly as before — including in the mixed registry a mid-upgrade user really
///   has, where an older `list`/`prune` binary and a newer `run` share one directory.
/// - **Old record, new reader.** Both fields are `Option` + `#[serde(default)]`, so
///   a record written before they existed reads back with them `None` — "not
///   reported", the same value a run whose argv matched no hint rule writes today.
///
/// The version and mechanism tags gate every action that probes or reaps a record.
/// An older reader may ignore additive fields, but it must skip a record whose
/// liveness semantics it cannot interpret rather than treating its `lock_file` as an
/// advisory lock and falsely declaring a live run stale. A bump is therefore required
/// whenever a writer changes the meaning of an existing field or liveness mechanism.
pub const REGISTRY_VERSION: u32 = 1;

/// The only liveness mechanism today: an OS advisory lock held for the run's life.
const LIVENESS_ADVISORY_LOCK: &str = "advisory_lock";

/// Environment override for the registry directory. Set it to pin the location —
/// used by the integration tests to isolate a scratch registry, and available to
/// an orchestrator that wants the registry somewhere specific. When unset the
/// platform default ([`platform::default_registry_dir`]) is used.
const REGISTRY_DIR_ENV: &str = "PROCESSKIT_CLI_REGISTRY_DIR";

/// Minimum age (by mtime) a `.lock` file with no `.json` sibling must have before
/// [`Registry::orphaned_lock_paths`] treats it as a candidate for reaping at all.
///
/// Without this floor, a `.lock` file [`Registry::reserve_entry`] `create_new`-d
/// microseconds ago — created, but not yet locked — is indistinguishable from a
/// genuine orphan that has sat unlocked for hours: both are simply an unlocked
/// `.lock` file with no `.json` next to it. A concurrent `prune` probing that fresh
/// file would either win the race for the lock (denying the legitimate reservation
/// underway and, worse, deleting the file it was about to publish a record for) or
/// lose it (correctly reading it as `Live`), purely by scheduling luck. A genuine
/// orphan never ages out of this check — it sits forever until reaped — so paying a
/// few seconds of extra reap latency costs nothing, while the reservation window
/// (two adjacent syscalls) reliably falls inside it. This is one half of [R-01]'s
/// fix; the other half is [`platform::lock_path_still_matches`], the identity check
/// `reserve_entry` performs after taking its lock. See docs/registry.md, "The
/// reaping safety invariant".
const ORPHAN_LOCK_MIN_AGE: Duration = Duration::from_secs(5);

/// Largest registry record accepted from disk. Records are small JSON metadata, so
/// this leaves generous room for legitimate labels while preventing one corrupt file
/// from making every discovery or polling pass allocate without bound.
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// The registry record a runner writes at start and removes on a clean exit.
///
/// `Serialize` + `Deserialize`: the runner writes it, future control-plane clients
/// read it back. Deliberately carries **no PID** — a run is addressed by `run_id`,
/// never by process id.
///
/// # What T-215 deliberately left out
///
/// The record gained the two *redaction-safe* command fields below so `list` can
/// tell several live runs apart ([`Record::argv_sha256`], [`Record::hint`]). Two
/// further candidates were considered and **refused**, as decisions rather than
/// omissions:
///
/// - **`root_pid`.** The registry's second load-bearing property is that nothing
///   here is addressed by, or identified with, a PID (`AGENTS.md`, "Nothing is
///   addressed by PID, which is what makes PID reuse irrelevant"; `docs/registry.md`,
///   "No PID addressing"). Publishing the root PID as a *display* field would not
///   break how entries are found, but it would put a reused-at-any-moment number in
///   front of an operator inside the one artifact whose whole design says "this
///   number cannot identify a run" — an invitation to `kill <pid>` the process that
///   inherited it. The value it would add (telling two runs apart) is exactly what
///   the fingerprint below provides without that hazard.
/// - **`cwd`.** It is a raw, unfingerprinted string with no redaction rule anywhere
///   in this project's redaction contract, which covers argv only (`docs/schema.md`,
///   "Command redaction"). A working directory routinely spells out a customer,
///   ticket, branch, or user name, so persisting it verbatim into a long-lived
///   registry file would put *unredacted* operational text into an artifact whose
///   own module docs justify its owner-only permissions by the sensitivity of what it
///   holds — for a discovery benefit the fingerprint already delivers. It would also
///   add a second untrusted **path** to validate on read, the shape that has already
///   produced one subtle defect here (a `Path::components()` validator silently
///   normalizing `.` segments, [K-074]). The `run_started` JSONL event still carries
///   `cwd`: that stream is written only to a file the caller explicitly asked for
///   with `--jsonl`, which is a different disclosure decision than a per-user
///   registry every local client of this binary reads by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Format version of this record ([`REGISTRY_VERSION`]).
    pub registry_version: u32,
    /// The run's identifier (`--run-id` or a generated value); the key clients match
    /// on. Not a PID.
    pub run_id: String,
    /// The run's local control-transport connection address — a unix socket path, or
    /// a Windows named-pipe name (see [`crate::control`]). A live runner publishes it
    /// so `inspect`/`cancel`/`kill` clients can reach it; `None` only when the
    /// transport could not be stood up (best-effort degradation).
    ///
    /// Untrusted deserialized data on the read side, exactly like
    /// [`Liveness::lock_file`]: [`Registry::prune`] reaps the unix socket directory a
    /// **confirmed-stale** record published, and validates this value by shape before
    /// deleting anything through it (see [`platform::control_socket_dir_to_reap`]).
    pub endpoint: Option<String>,
    /// Run start time, RFC 3339 UTC with millisecond precision (same formatter as the
    /// JSONL events, see [`events::format_rfc3339_utc`]).
    pub started_at: String,
    /// The run's **redaction-safe** command fingerprint: the lowercase-hex SHA-256 of
    /// the canonical argv encoding `docs/schema.md` pins, exactly as the JSONL
    /// `run_started` event's `command.argv_sha256` carries it (one shared
    /// implementation — [`events::CommandFingerprint`]). It is what lets an operator
    /// staring at several live entries tell *which run is which* without the command
    /// line: two runs of the same command share a fingerprint, two different commands
    /// do not.
    ///
    /// **Never argv, in any form.** The hash is one-way and is the only
    /// command-derived value here besides [`Record::hint`]; `--argv-raw` does not
    /// widen it, because raw argv is not an input to `register` at all (see
    /// [`Registry::register`]). Added additively in T-215: `None` on a record written
    /// before that (or by any writer that publishes none), which is why it
    /// deserializes as optional.
    ///
    /// Untrusted deserialized data on the read side, like every other field a
    /// corrupt or hand-edited file can carry — validated by shape in
    /// [`parse_and_validate_record`], which drops a malformed value rather than
    /// discarding the whole record (see [`is_valid_argv_sha256`]).
    #[serde(default)]
    pub argv_sha256: Option<String>,
    /// The run's worker-shape category, from the same classifier the JSONL event uses
    /// ([`events::classify_hint`] / its `HINT_RULES` catalog, mirrored in
    /// `docs/schema.md`) — `None` when the argv matches no known shape, which is the
    /// common case. A fixed category label, never argv content, so publishing it
    /// weakens redaction no more than the fingerprint above does.
    ///
    /// Additive and optional for the same reason as [`Record::argv_sha256`], and
    /// validated on read the same way (see [`is_valid_hint`]).
    #[serde(default)]
    pub hint: Option<String>,
    /// Operator-provided discovery labels. Additive and empty for records written
    /// before labels existed; values are validated on read before display or
    /// aggregate filtering.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Absolute path to the run's JSONL lifecycle stream. Additive and absent on
    /// records written by older versions. Paths are operator-selected observability
    /// locators, not command-line data; the owner-only registry publishes them so a
    /// supervisor that discovers a detached run can also find its artifacts.
    #[serde(default)]
    pub jsonl: Option<String>,
    /// Absolute output-capture directory, when `--capture-dir` was requested.
    /// Additive and optional for both compatibility and runs without capture.
    #[serde(default)]
    pub capture_dir: Option<String>,
    /// How a client decides whether this record is live or stale — never by the file
    /// merely existing.
    pub liveness: Liveness,
}

/// Artifact locations a runner may publish with its registry record. Borrowed so
/// registration never needs an extra clone before constructing the owned record.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactLocators<'a> {
    pub jsonl: Option<&'a str>,
    pub capture_dir: Option<&'a str>,
}

/// The documented liveness signal embedded in a [`Record`]: which sibling file the
/// live runner holds an OS advisory lock on, and by what mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Liveness {
    /// The mechanism tag. `advisory_lock` today; a versioned point of extension.
    pub kind: String,
    /// The sibling lock file (name only, resolved against the registry directory)
    /// the live runner holds an exclusive advisory lock on for the whole run. A
    /// client tests liveness by trying to acquire that lock — see [`Registry::entries`].
    pub lock_file: String,
}

/// The health of a registry entry as probed through its lock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// A live runner holds the entry's advisory lock: the run is running.
    Live,
    /// No process holds the lock (the runner exited abruptly without cleaning up, or
    /// the lock file is gone): the entry is **confirmed** stale and must not be
    /// treated as live.
    Stale,
    /// The liveness probe itself could not be performed — the lock file would not
    /// open (EISDIR, permission denied, a rejected symlink/reparse point) or the
    /// lock call errored. Liveness is **unknown**, not confirmed stale: this is the
    /// same "probe failed" case [`Registry::prune`]'s own [`probe_for_prune`] and
    /// [`Registry::probe_run`]'s own [`probe_health`] call already keep apart from a
    /// confirmed-dead entry (see [K-024]) — [`Registry::entries`] now keeps it apart
    /// too, instead of folding it into [`Health::Stale`]. Every control client
    /// (`inspect`/`cancel`/`kill`) that matches only on [`Health::Live`] *acts* on
    /// this exactly as it does on `Stale` — refusing — so their behavior is
    /// unchanged; what they no longer share is the *wording* of that refusal, which
    /// names an unprobeable entry `unprobed` instead of claiming the runner is gone
    /// (see [`crate::control`], "Dead runner / unreachable entry"). Neither the
    /// discovery surface `list` nor a refusing control client makes the misleadingly
    /// positive claim "stale" (the runner is confirmed dead) about a record the probe
    /// never actually reached.
    Unprobed,
}

/// A scanned registry entry: its parsed [`Record`], its probed [`Health`], and the
/// path of the record file (so a client can act on or reap it). Consumed by the
/// control-plane client ([`crate::control`], `inspect`), which matches on `run_id`
/// and connects only to a [`Health::Live`] entry's endpoint.
#[derive(Debug)]
pub struct Entry {
    pub record: Record,
    pub health: Health,
    /// The record file's path — how a client acts on or reaps the entry (the
    /// reaping clients, `cancel`/`kill`, T-009), and, for `list`, a unique-per-entry
    /// tertiary sort key (two records can otherwise share both `run_id` and
    /// `started_at`); `inspect` matches on `run_id` and health alone, so it does not
    /// touch it.
    pub path: PathBuf,
}

/// A registry record that passed every corruption guard in the scan — readable,
/// parsable JSON, a well-formed `started_at`, and a simple in-directory `lock_file`
/// name — paired with the two on-disk paths it resolves to. The shared product of
/// [`Registry::scan`], consumed by [`Registry::entries`] (which probes each into an
/// [`Entry`]), [`Registry::prune`] (which reaps only the confirmed-stale ones), and
/// [`Registry::probe_run`] (which probes only the ones matching one `run_id`).
struct ScannedRecord {
    record: Record,
    /// The record file (`<stem>.json`) — what [`Entry::path`] carries and what prune
    /// deletes first.
    json_path: PathBuf,
    /// The validated, joined lock file path (`<stem>.lock`) the liveness probe opens.
    lock_path: PathBuf,
}

/// The tally a [`Registry::prune`] pass produces: how many entries it reaped, how
/// many live ones it deliberately left alone, and how many it could not probe (and
/// so also left alone). The counts sum only over records the scan considered — a
/// corrupt/unreadable record is never a prune candidate and is not counted here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneOutcome {
    /// Confirmed-stale entries (`.json`/`.lock` pairs) whose files were reaped.
    pub pruned: usize,
    /// Live entries left untouched (a live runner holds the lock) — counts both
    /// paired records and lone orphaned `.lock` files a live holder still holds.
    pub live: usize,
    /// Entries whose liveness could not be probed (the lock file would not open, or
    /// the lock call errored) and were therefore left in place rather than risked —
    /// counts both paired records and lone orphaned `.lock` files.
    pub unprobed: usize,
    /// Confirmed-stale **orphaned** `.lock` files — ones with no paired `.json` —
    /// that were reaped. Kept as its own field rather than folded into `pruned`
    /// because a pruned entry deletes *two* files (`.json` + `.lock`) while an
    /// orphaned-lock reap deletes only the one `.lock` file: collapsing the two
    /// would make `pruned` an inconsistent unit (sometimes "pairs", sometimes
    /// "files"). An orphan arises when a record's `.json` write never lands (now
    /// backstopped for the fresh-registration case by
    /// [`ReservedEntry`]'s Drop guard, but still possible for, say, a hand-edited or
    /// partially-cleaned-up directory) or when [`Registration::remove`]'s best-effort
    /// `.json` delete succeeds while its `.lock` delete does not.
    pub orphaned_locks: usize,
}

/// The result of a non-destructive [`Registry::preview_prune`] pass (`prune
/// --dry-run`, T-199): the same aggregate tally a following [`Registry::prune`]
/// pass over the identical, untouched registry state would report, plus every
/// confirmed-stale candidate that tally counts — what that following prune would
/// actually reap. [`Registry::preview_prune`] never calls `fs::remove_file`, so
/// producing this costs nothing but the scan and the liveness probes themselves.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PrunePreview {
    /// The same tally shape [`Registry::prune`] returns, computed the identical way
    /// — see [`Registry::preview_prune`] for why the two are guaranteed to agree.
    pub outcome: PruneOutcome,
    /// Every confirmed-stale (`Reapable`) candidate the tally above counts, in scan
    /// order: paired records first (the [`Registry::scan`] pass), then orphaned lock
    /// files (the [`Registry::orphaned_lock_paths`] pass) — the same two passes
    /// [`Registry::prune`] makes, in the same order.
    pub candidates: Vec<PruneCandidate>,
}

/// One confirmed-stale prune candidate a [`Registry::preview_prune`] pass found —
/// identifying the same on-disk entry a following [`Registry::prune`] pass would
/// reap, in the same vocabulary `list`/`prune --json` already use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneCandidate {
    /// A paired `.json`/`.lock` record: the `run_id`/`started_at` fields a caller
    /// would otherwise have to open its [`Record`] to find, plus the private control
    /// socket directory the reap would remove along with them.
    Entry {
        /// The record's `run_id`.
        run_id: String,
        /// The record's `started_at`, RFC 3339 UTC with millisecond precision.
        started_at: String,
        /// The private control-socket directory a real prune would also reap for
        /// this entry (T-207) — the `pkc-…` directory holding the unix socket the
        /// record published in [`Record::endpoint`], as classified by the very same
        /// check the reap itself applies.
        ///
        /// `None` whenever that reap would remove nothing: a record with no endpoint
        /// at all, an endpoint that is not the exact shape this project's control
        /// server publishes (a corrupt or hand-edited value — it is *not* trusted as
        /// a path), or a Windows record, whose named-pipe endpoint has no filesystem
        /// leftover to reap in the first place.
        ///
        /// Reported from the record alone, without stat-ing anything (a preview
        /// touches no filesystem beyond the scan and the liveness probes), so a
        /// directory named here may already be gone — reaping it is best-effort,
        /// exactly like the record/lock deletions.
        socket_dir: Option<String>,
    },
    /// A lone `.lock` file with no `.json` sibling — there is no record to pull
    /// `run_id`/`started_at` from, so it is identified by its file name instead.
    OrphanedLock {
        /// The lock file's name (no directory component — resolved against the
        /// registry directory, exactly like [`Liveness::lock_file`]).
        lock_file_name: String,
    },
}

/// What one [`Registry::probe_run`] pass concluded about a single `run_id` — the
/// question `wait` (see [`crate::wait`]) asks the registry over and over: *is this
/// run still going?*
///
/// Deliberately **not** expressed as a bare `bool` (or as [`Health`]): the honest
/// answer has four cases, and collapsing them would either invent liveness the probe
/// never confirmed or hide the ambiguity a duplicated `run_id` creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// **Confirmed finished.** Every record matching the `run_id` probed as
    /// [`Health::Stale`], or there is no matching record at all. The two are one
    /// case on purpose: a run that exits cleanly deletes its own entry, so "never
    /// registered" and "already finished and cleaned up" are indistinguishable from
    /// the registry alone — see [`Registry::probe_run`].
    Finished,
    /// **Still going.** Exactly one matching record is confirmed live (a runner
    /// holds its advisory lock).
    Live,
    /// **Not a single run.** More than one matching record is confirmed live at
    /// once — the registry never enforces `run_id` uniqueness (`docs/registry.md`,
    /// "Run id resolution"), so the id does not name one run and no per-run question
    /// can be answered about it. Carries how many live records collided.
    Ambiguous { live: usize },
    /// **Unknown.** No matching record is confirmed live, but at least one could not
    /// be probed at all (its lock file would not open, or the lock call errored), so
    /// "finished" cannot be *confirmed* — only assumed. Kept apart from
    /// [`RunStatus::Finished`] for the same reason [`Registry::prune`] keeps its
    /// probe `Err` apart from a confirmed-stale entry (see [K-024]): an unprobeable
    /// entry is not evidence of anything.
    Unprobed,
}

/// One by-id liveness probe plus the terminal-event locator published by its sole
/// confirmed-live record. `wait --report-outcome` remembers the locator while the
/// record still exists; once a clean exit removes it, the registry has no history
/// from which to recover the path.
pub(crate) struct RunProbe {
    pub status: RunStatus,
    pub jsonl: Option<PathBuf>,
}

/// A handle onto the per-user run registry directory.
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// Open the per-user registry, creating its directory (and parents) restricted
    /// to the owner. The location is [`REGISTRY_DIR_ENV`] if set, else the platform
    /// default.
    ///
    /// This is the *mutating* open used by [`Registry::register`]'s caller (`run`):
    /// it must create the directory (and re-assert its owner-only permissions on a
    /// pre-existing one) because a run is about to write a record into it. A caller
    /// that only wants to *read* the registry — `list`/`prune`/`wait`, and the
    /// control clients `inspect`/`cancel`/`kill` — must use
    /// [`Registry::open_read_only`] instead, so a read-only scan cannot itself create
    /// registry state or touch its permissions.
    pub fn open() -> io::Result<Self> {
        Self::open_in(resolve_dir()?)
    }

    /// Open a registry rooted at an explicit directory (the env override and the
    /// tests use this). Creates the directory with owner-only permissions and
    /// re-asserts them if it already exists.
    pub fn open_in(dir: PathBuf) -> io::Result<Self> {
        platform::create_owner_only_dir(&dir)?;
        Ok(Self { dir })
    }

    /// Open the per-user registry **without** creating its directory or touching its
    /// permissions — the read-only counterpart of [`Registry::open`], for callers
    /// (`list`/`prune`/`wait`, and the control clients `inspect`/`cancel`/`kill`)
    /// that must never mutate registry state just to look at it. The
    /// location is resolved exactly as [`Registry::open`] resolves it
    /// ([`REGISTRY_DIR_ENV`] if set, else the platform default); a directory that
    /// does not exist yet is not an error here either — [`Registry::entries`]
    /// already treats a missing directory as an empty registry.
    pub fn open_read_only() -> io::Result<Self> {
        Ok(Self::open_read_only_in(resolve_dir()?))
    }

    /// Open a registry rooted at an explicit directory, read-only (the tests use
    /// this directly; [`Registry::open_read_only`] resolves the directory and
    /// delegates here). Never touches the filesystem — it cannot fail.
    pub fn open_read_only_in(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Register a starting run: write its [`Record`] and take the exclusive advisory
    /// lock that marks it live. The returned [`Registration`] holds that lock for the
    /// run's lifetime; dropping it (or calling [`Registration::remove`]) tears the
    /// entry down.
    ///
    /// If the record is never actually published — the `fs::write` of the JSON call
    /// itself fails, or an earlier `?` returns first — the reserved lock file does
    /// not leak: [`ReservedEntry`]'s [`LockCleanupGuard`] backstop deletes it when the
    /// unpublished reservation drops (see [`Registry::reserve_entry`]), so a failed
    /// `register` never leaves an orphaned `.lock` file with no `.json` for
    /// `Registry::prune`'s orphan-lock pass to have to clean up later.
    ///
    /// `endpoint` is the local transport address the runner published (a unix socket
    /// path / Windows pipe name), or `None` when no transport could be stood up.
    /// `started` is the run's start time.
    ///
    /// `command` is the run's **redaction-safe** identification — the argv
    /// fingerprint and worker-shape hint an operator uses to tell several live
    /// entries apart (T-215). It is deliberately an [`events::CommandFingerprint`]
    /// rather than the argv itself: the raw command line is not an input to this
    /// function at all, so no code path here — present or future, `--argv-raw` or not
    /// — can put argv into a registry record. Redaction is a property of the
    /// signature, not of remembering to apply it.
    pub fn register(
        &self,
        run_id: &str,
        endpoint: Option<&str>,
        started: SystemTime,
        command: &events::CommandFingerprint,
    ) -> io::Result<Registration> {
        self.register_with_labels(run_id, endpoint, started, command, &BTreeMap::new())
    }

    /// Register a run together with its validated operator labels. Kept separate
    /// from [`Registry::register`] so existing metadata-free internal callers stay
    /// concise while the production runner cannot forget to publish its labels.
    pub fn register_with_labels(
        &self,
        run_id: &str,
        endpoint: Option<&str>,
        started: SystemTime,
        command: &events::CommandFingerprint,
        labels: &BTreeMap<String, String>,
    ) -> io::Result<Registration> {
        self.register_with_labels_and_artifacts(
            run_id,
            endpoint,
            started,
            command,
            labels,
            ArtifactLocators::default(),
        )
    }

    /// Register a run with its labels and absolute artifact locations. Production
    /// runners use this entry point; the simpler registration helpers retain the
    /// legacy artifact-free shape for focused registry tests.
    pub fn register_with_labels_and_artifacts(
        &self,
        run_id: &str,
        endpoint: Option<&str>,
        started: SystemTime,
        command: &events::CommandFingerprint,
        labels: &BTreeMap<String, String>,
        artifacts: ArtifactLocators<'_>,
    ) -> io::Result<Registration> {
        // Reserve a unique, opaque entry stem via the filesystem itself (create_new),
        // and take the live lock on the fresh lock file before publishing the record.
        let reserved = self.reserve_entry()?;

        let record = Record {
            registry_version: REGISTRY_VERSION,
            run_id: run_id.to_string(),
            endpoint: endpoint.map(str::to_string),
            started_at: events::format_rfc3339_utc(started),
            argv_sha256: Some(command.argv_sha256.clone()),
            hint: command.hint.map(str::to_string),
            labels: labels.clone(),
            jsonl: artifacts.jsonl.map(str::to_string),
            capture_dir: artifacts.capture_dir.map(str::to_string),
            liveness: Liveness {
                kind: LIVENESS_ADVISORY_LOCK.to_string(),
                lock_file: file_name(&reserved.lock_path),
            },
        };
        let json = serde_json::to_string(&record).map_err(io::Error::other)?;
        // The record is written only after the lock is held, so an entry is never
        // visible to a scanner in a state where it looks live but no lock exists.
        fs::write(&reserved.json_path, json)?;
        // The record is now published: disarm the Drop backstop so it does not
        // delete the very lock file the just-written record names. Every earlier
        // return above (the `?` on `fs::write` failing) leaves the guard armed, so
        // `reserved` drops with it still active and the lock file is reclaimed.
        reserved.cleanup.disarm();

        Ok(Registration {
            json_path: reserved.json_path,
            lock_path: reserved.lock_path,
            lock: reserved.lock,
            removed: AtomicBool::new(false),
        })
    }

    /// Test-only [`Registry::register`] with a fixed, representative command
    /// fingerprint — for the many tests (here and in [`crate::control`]) whose
    /// subject is an entry's liveness, locking, scanning, or reaping behavior and
    /// not the command metadata it publishes. It exists so those tests keep reading
    /// as what they are about, while the production entry point above still takes
    /// the fingerprint explicitly, with no default that could quietly publish
    /// nothing. Tests that *are* about the published metadata call `register`
    /// directly with their own fingerprint.
    #[cfg(test)]
    pub(crate) fn register_plain(
        &self,
        run_id: &str,
        endpoint: Option<&str>,
        started: SystemTime,
    ) -> io::Result<Registration> {
        self.register(
            run_id,
            endpoint,
            started,
            &events::CommandFingerprint::for_argv(["pkc-test-fixture", run_id]),
        )
    }

    /// Scan every entry, classifying each as [`Health::Live`], [`Health::Stale`], or
    /// [`Health::Unprobed`] by probing its lock file. A malformed *record*
    /// (unparsable JSON, or one whose `started_at`/`lock_file` field is not the shape
    /// a well-behaved runner writes) is corrupt-record noise and is skipped outright
    /// — there is no lock path worth probing. A well-formed record whose lock file
    /// *cannot be probed* (any non-`NotFound` error opening it, or a lock/unlock
    /// error) is different: the record itself is trustworthy, only its liveness is
    /// unknowable, so the entry is still returned — classified [`Health::Unprobed`]
    /// ("could not confirm liveness — neither live nor confirmed stale") — rather
    /// than dropped or misreported as confirmed-dead. A failure to even iterate one
    /// directory entry (a transient filesystem error or a removal race on that one
    /// item, distinct from `fs::read_dir` itself failing on the directory as a whole —
    /// see [`Registry::scan`]) is the same kind of per-item noise and is likewise
    /// skipped, not fatal. Either way one bad entry never aborts the whole scan or
    /// blinds a client to the healthy ones. This is the read
    /// side the control-plane client (`inspect`, T-008; `cancel`/`kill`, T-009) builds
    /// on: find the run whose `record.run_id` matches, then act only if it is live —
    /// which a probe-failed entry, being [`Health::Unprobed`] (not [`Health::Live`]),
    /// never is, so those clients behave exactly as they did when this case was
    /// folded into `Stale` (see [K-024]). `list` (the operator-facing discovery
    /// surface, T-206) and `wait --all` (T-216, [`crate::wait::run_all`]) are the two
    /// consumers that must tell the two apart. For `list`, printing a probe-failed
    /// entry as `"stale"` would be a positive, unconfirmed claim that the runner is
    /// dead, distinct from what the probe actually established. For `wait --all`, the
    /// distinction is not merely a display nicety but a condition of correctness at
    /// snapshot time: an entry this method reports [`Health::Unprobed`] is excluded
    /// from the wait's target set from the start (never entering it, unlike an entry
    /// that re-probes `Unprobed` on a *later* pass, which does stay outstanding — see
    /// `docs/registry.md`, "The aggregate barrier — `wait --all`"), so folding
    /// `Unprobed` into `Stale` here would silently shrink what `--all` waits for.
    pub fn entries(&self) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for ScannedRecord {
            record,
            json_path,
            lock_path,
        } in self.scan()?
        {
            // A per-record probe failure (an unreadable target, one rejected as a
            // symlink/reparse point at open time, or a lock/unlock error — see
            // [`probe_health`]) does not discredit the record itself: only its
            // liveness could not be confirmed. Classify it `Unprobed` rather than
            // dropping the entry, aborting the scan, or fabricating a confirmed
            // `Stale` verdict the probe never actually reached — this keeps the entry
            // visible for the `prune` reaper (T-164) and preserves the misrouting fix
            // this method exists for: `inspect`/`cancel`/`kill` act only on `Live`
            // entries, so a record whose probe failed can no longer fail the whole
            // scan and take down an operation on an unrelated, healthy run_id.
            // **Prune must not reuse this value** — it needs the acquired lock held
            // across its deletions, which a pure liveness query like this one already
            // released — and so probes on its own path (see [`Registry::prune`] /
            // [`probe_for_prune`]).
            let health = match probe_health(&lock_path) {
                Ok(LivenessProbe::Live) => Health::Live,
                Ok(LivenessProbe::Stale) => Health::Stale,
                Err(_) => Health::Unprobed,
            };
            entries.push(Entry {
                record,
                health,
                path: json_path,
            });
        }
        Ok(entries)
    }

    /// Take one snapshot of every entry whose advisory-lock probe confirms it live.
    /// Aggregate control mutations and `wait --all` deliberately share this exact
    /// inclusion bar; each consumer projects the returned records into its own target
    /// shape, but neither reimplements the `Health::Live` filter.
    pub(crate) fn snapshot_live_entries(&self) -> io::Result<Vec<Entry>> {
        Ok(self
            .entries()?
            .into_iter()
            .filter(|entry| entry.health == Health::Live)
            .collect())
    }

    /// Read, validate, and probe one exact registry record without scanning or
    /// probing unrelated entries. A missing path is the ordinary `None` result;
    /// corrupt or unreadable content remains an error so a caller cannot mistake an
    /// unvalidated replacement for a run that is confirmed gone.
    pub(crate) fn probe_entry(&self, record_path: &Path) -> io::Result<Option<Entry>> {
        let text = match read_record_text(record_path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let record = parse_and_validate_record(&text).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the registry record no longer passes validation",
            )
        })?;
        let lock_path = self.dir.join(&record.liveness.lock_file);
        let health = match probe_health(&lock_path) {
            Ok(LivenessProbe::Live) => Health::Live,
            Ok(LivenessProbe::Stale) => Health::Stale,
            Err(_) => Health::Unprobed,
        };
        Ok(Some(Entry {
            record,
            health,
            path: record_path.to_path_buf(),
        }))
    }

    /// Reap every **confirmed-stale** entry — both its files, plus (on unix) the
    /// control socket it published — and leave every other entry untouched. The
    /// safe-by-construction cleanup for what an abruptly-killed runner leaves behind
    /// once its clean-exit [`Registration::remove`] never runs: the `.json`/`.lock`
    /// pair the registry would otherwise accumulate forever, and the private socket
    /// directory nothing else ever cleaned up (see "The third leftover" below).
    ///
    /// The single load-bearing safety property is that pruning deletes **only** an
    /// entry whose liveness probe **succeeded and returned stale** — and nothing else:
    ///
    /// - **`Ok(stale)` ⇒ reap.** The lock file was absent, or the exclusive lock was
    ///   free and taken: no live runner holds it, so the record is genuinely dead.
    /// - **`Ok(live)` ⇒ never touch.** A live runner holds the lock — reaping it would
    ///   delete a running run's registry entry.
    /// - **`Err(_)` ⇒ leave in place.** The probe could not even be performed (the lock
    ///   file would not open — EISDIR, permission-denied, a rejected reparse point — or
    ///   the lock call itself errored). Liveness is *unknown*, not confirmed stale, so
    ///   the entry is kept, on every repeated prune, rather than risk deleting a record
    ///   that may belong to a live run — the same [`Health::Unprobed`] verdict
    ///   [`Registry::entries`] now reports for this case (see [K-024]), but prune
    ///   cannot simply read [`Entry::health`] here: it needs the probe's acquired
    ///   lock held across the two deletions below, and a pure liveness query like
    ///   `entries` already released it. So prune probes on its **own** path
    ///   ([`probe_for_prune`]), which keeps the three cases apart *and* keeps the
    ///   lock, rather than reading [`Entry::health`].
    ///
    /// Corrupt records the scan already skips (unreadable, unparsable JSON, a malformed
    /// `started_at`, or a `lock_file` that is not a simple in-directory name) are
    /// **not** candidates — they are never probed and never deleted, exactly as
    /// [`Registry::entries`] leaves them alone. No entry is ever addressed by PID: a
    /// candidate is reached only through the record path the directory scan already
    /// produced.
    ///
    /// A confirmed-stale entry is reaped **while its lock is still held** (see
    /// [`probe_for_prune`]): the reclaim keeps the exclusive lock across the
    /// deletions, so a second concurrent prune sees the entry as live and skips it
    /// rather than racing on the same files — the "hold the lock to reclaim" pattern
    /// `docs/registry.md` documents. Deletion mirrors [`Registration::remove`]: the
    /// record (`.json`) first, then the lock (`.lock`), each best-effort — an OS delete
    /// error on one entry never aborts the reaping of the others (a leftover just reads
    /// as stale again next time). Running prune over an already-clean registry is a
    /// no-op, not an error.
    ///
    /// **The third leftover: the control socket (T-207).** On unix a runner also
    /// leaves its control transport behind when it dies abruptly — a `0700` private
    /// directory under `/tmp` holding one socket file (`crate::control`'s
    /// `ControlServer::bind`, removed only by its `Drop` on a clean teardown), whose
    /// path the dead record still publishes in [`Record::endpoint`]. Nothing else
    /// ever reaps it, and the record naming it is about to be deleted, so a
    /// confirmed-stale entry's socket directory is removed here too — otherwise
    /// `prune`, the documented cleanup counterpart for "runners that died abruptly",
    /// would keep closing only half of that leak. Three properties bound it:
    ///
    /// - **The endpoint is not trusted as a path.** It is untrusted deserialized data
    ///   exactly like `liveness.lock_file`, so it is validated by *shape* first —
    ///   [`platform::control_socket_dir_to_reap`], which yields a directory only for
    ///   the exact `<temp base>/pkc-<token>/c.sock` form the control server publishes,
    ///   and `None` for anything else (absent, empty, relative, `..`-carrying,
    ///   differently named, or outside those bases). A record that fails it keeps
    ///   whatever its endpoint pointed at and is still reaped itself: the check gates
    ///   only this extra deletion.
    /// - **No symlink is ever followed.** The validated directory is opened
    ///   `O_NOFOLLOW | O_DIRECTORY` and the socket is unlinked relative to that
    ///   handle, so neither component can be swapped for a link redirecting the
    ///   deletion elsewhere (see [`platform::reap_control_socket_dir`]).
    /// - **Best-effort, like every other deletion here.** A socket that will not go
    ///   is left behind for the next pass and never aborts the reaping of other
    ///   entries.
    ///
    /// On Windows this step does nothing at all: a named pipe lives in the kernel
    /// object namespace and vanishes with the process that created it, leaving no
    /// filesystem leftover to reap.
    ///
    /// A second pass, after the paired-record pass above, reaps **orphaned lock
    /// files**: `.lock` files with no sibling `.json` at all, invisible to
    /// [`Registry::scan`] (which only ever walks `.json` records) and therefore never
    /// reachable by the pass above, however long it has sat there. Such an orphan
    /// arises when a record's `.json` write never lands (the fresh-registration case
    /// is now backstopped by [`ReservedEntry`]'s Drop guard, but a hand-edited or
    /// partially-cleaned-up directory can still produce one) or when
    /// [`Registration::remove`]'s best-effort `.json` delete succeeds while its
    /// `.lock` delete does not. This second pass reuses the exact same
    /// [`probe_for_prune`] lock-probe safety as the paired-record pass — a `Live` lock
    /// is never touched, a probe `Err` leaves the file in place, and only a confirmed-
    /// stale lock is deleted — tallied into [`PruneOutcome::orphaned_locks`] (kept
    /// distinct from `pruned` because it deletes one file, not a `.json`/`.lock` pair)
    /// while sharing the `live`/`unprobed` counters with the paired-record pass, since
    /// those two verdicts mean exactly the same thing whether or not the lock has a
    /// `.json` sibling. Unlike the paired-record pass, a candidate here must also
    /// clear [`Registry::orphaned_lock_paths`]'s [`ORPHAN_LOCK_MIN_AGE`] floor before
    /// it is even probed — see that constant's doc for why a bare lock-probe-safety
    /// guarantee is not, on its own, enough for a record-less lock file (R-01).
    pub fn prune(&self) -> io::Result<PruneOutcome> {
        self.prune_matching(&[])
    }

    /// Reap only paired entries carrying every requested operator label. With no
    /// filters this is exactly [`Registry::prune`], including its orphan-lock pass.
    /// With an explicit filter, record-less orphan locks stay out of scope because
    /// no record remains from which their ownership labels could be established.
    pub fn prune_matching(
        &self,
        filters: &[crate::labels::OperatorLabel],
    ) -> io::Result<PruneOutcome> {
        let mut outcome = PruneOutcome::default();
        for ScannedRecord {
            record,
            json_path,
            lock_path,
        } in self.scan()?
        {
            if !crate::labels::matches(&record.labels, filters) {
                continue;
            }
            match probe_for_prune(&lock_path) {
                // Confirmed stale: reap the entry's leftovers while still holding the
                // acquired lock (when there was one). The socket the record published
                // goes first, then the record, then the lock file (the last two in
                // `Registration::remove`'s own order); the held lock is released only
                // when `_held_lock` drops after all of them.
                Ok(PruneProbe::Reapable(_held_lock)) => {
                    // T-207: the control socket is reaped *before* the record that
                    // names it. The record is the only thing that points at the
                    // socket's directory, so a pass interrupted between the two
                    // deletions must not be the one that leaves the socket
                    // unreferenced — the reverse order would strand it forever, while
                    // this order at worst re-reaps an already-socket-less record next
                    // pass.
                    if let Some(socket_dir) =
                        platform::control_socket_dir_to_reap(record.endpoint.as_deref())
                    {
                        platform::reap_control_socket_dir(&socket_dir);
                    }
                    let _ = fs::remove_file(&json_path);
                    let _ = fs::remove_file(&lock_path);
                    outcome.pruned += 1;
                }
                // A live runner holds the lock — never touch a running run's entry.
                Ok(PruneProbe::Live) => outcome.live += 1,
                // The probe could not be performed: liveness is unknown, not
                // confirmed stale, so the entry is left exactly as it is.
                Err(_) => outcome.unprobed += 1,
            }
        }

        if filters.is_empty() {
            for lock_path in self.orphaned_lock_paths()? {
                match probe_for_prune(&lock_path) {
                    // Confirmed stale: there is no `.json` sibling to delete, only the
                    // lock file itself.
                    Ok(PruneProbe::Reapable(_held_lock)) => {
                        let _ = fs::remove_file(&lock_path);
                        outcome.orphaned_locks += 1;
                    }
                    Ok(PruneProbe::Live) => outcome.live += 1,
                    Err(_) => outcome.unprobed += 1,
                }
            }
        }

        Ok(outcome)
    }

    /// Non-destructive counterpart to [`Registry::prune`] (`prune --dry-run`,
    /// T-199): the exact same two passes — the paired-record scan
    /// ([`Registry::scan`]) then the orphaned-lock scan
    /// ([`Registry::orphaned_lock_paths`]) — classified through the exact same
    /// [`probe_for_prune`] three-way probe, but never calling `fs::remove_file` on
    /// anything, regardless of what a candidate classifies as. Only the *action* on
    /// a confirmed-stale (`Reapable`) verdict differs from `prune`: instead of
    /// deleting files while the probe-acquired lock is held, this releases that lock
    /// at once (there is nothing to reclaim it for — a preview reaps nothing) and
    /// records a [`PruneCandidate`] describing the entry instead. The `Live`/`Err`
    /// arms are handled identically to `prune`, so the returned tally
    /// ([`PrunePreview::outcome`]) is exactly what a following, untouched `prune`
    /// pass over the same on-disk state would report — see this module's
    /// `preview_prune_matches_a_real_prune_over_identical_state` test.
    ///
    /// The control socket a confirmed-stale record published (T-207) is reported the
    /// same way: [`PruneCandidate::Entry::socket_dir`] names the directory a real
    /// reap would remove, classified by the identical
    /// [`platform::control_socket_dir_to_reap`] call `prune` makes — so the preview
    /// can neither name a socket directory the reap would refuse nor stay silent
    /// about one it would delete. Classification is purely lexical there too, so this
    /// still stats nothing and still removes nothing.
    pub fn preview_prune(&self) -> io::Result<PrunePreview> {
        self.preview_prune_matching(&[])
    }

    /// Preview the same label-scoped operation as [`Registry::prune_matching`].
    /// The filter is applied before probing, and an explicit filter excludes the
    /// ownerless orphan-lock pass exactly as the real operation does.
    pub fn preview_prune_matching(
        &self,
        filters: &[crate::labels::OperatorLabel],
    ) -> io::Result<PrunePreview> {
        let mut outcome = PruneOutcome::default();
        let mut candidates = Vec::new();

        for ScannedRecord {
            record, lock_path, ..
        } in self.scan()?
        {
            if !crate::labels::matches(&record.labels, filters) {
                continue;
            }
            match probe_for_prune(&lock_path) {
                // Confirmed stale: unlike `prune`, nothing is reclaimed under the
                // lock — release it immediately and record the candidate instead of
                // deleting anything. The published control socket is classified
                // through the very same `control_socket_dir_to_reap` the real reap
                // uses (T-207), so the preview names a socket directory exactly when
                // a real prune would delete one; it is reported, not stat-ed.
                Ok(PruneProbe::Reapable(held_lock)) => {
                    drop(held_lock);
                    candidates.push(PruneCandidate::Entry {
                        socket_dir: platform::control_socket_dir_to_reap(
                            record.endpoint.as_deref(),
                        )
                        .map(|dir| dir.to_string_lossy().into_owned()),
                        run_id: record.run_id,
                        started_at: record.started_at,
                    });
                    outcome.pruned += 1;
                }
                // A live runner holds the lock — same "never touch" verdict `prune`
                // reaches, just nothing to touch here either way.
                Ok(PruneProbe::Live) => outcome.live += 1,
                // The probe could not be performed: liveness is unknown, counted
                // exactly as `prune` counts it.
                Err(_) => outcome.unprobed += 1,
            }
        }

        if filters.is_empty() {
            for lock_path in self.orphaned_lock_paths()? {
                match probe_for_prune(&lock_path) {
                    Ok(PruneProbe::Reapable(held_lock)) => {
                        drop(held_lock);
                        candidates.push(PruneCandidate::OrphanedLock {
                            lock_file_name: file_name(&lock_path),
                        });
                        outcome.orphaned_locks += 1;
                    }
                    Ok(PruneProbe::Live) => outcome.live += 1,
                    Err(_) => outcome.unprobed += 1,
                }
            }
        }

        Ok(PrunePreview {
            outcome,
            candidates,
        })
    }

    /// Ask the registry, in one pass, whether the run named `run_id` is still going —
    /// the read step [`crate::wait`] polls, and the third consumer of [`Registry::scan`]
    /// alongside [`Registry::entries`] and [`Registry::prune`].
    ///
    /// **Why this does not build on [`Registry::entries`].** This method needs to
    /// count *matching* (by `run_id`) live and unprobed records separately —
    /// `entries()` returns every record with no such filtering or counting — so it
    /// shares only the underlying [`Registry::scan`] step and probes each matching
    /// record on its own path through [`probe_health`], whose `Ok(Live)` / `Ok(Stale)`
    /// / `Err` triple keeps a probe failure distinct from a confirmed-stale record —
    /// the same discipline [`Registry::prune`] applies with [`probe_for_prune`] for
    /// the analogous reason (see [K-024]: minting `Finished` from a probe that never
    /// actually ran would report a live run as over, exactly as reaping one would
    /// delete a live run's record), differing only in what it does with the acquired
    /// lock: prune *reclaims* the entry and keeps the lock held across its deletions,
    /// while this is a pure query and releases it immediately, exactly as `list` does
    /// (whose `entries()` now reports the identical [`Health::Unprobed`] verdict for
    /// this same case, since T-206).
    ///
    /// **Counting.** Matching records are selected by the identity predicate — the
    /// `run_id` field — **first**, and only then classified by health; folding both into
    /// one filter pass is how an ambiguity check silently undercounts (see [K-016], where
    /// a live-but-endpoint-less duplicate evaded exactly this check in `src/control/mod.rs`).
    /// A live entry counts as a duplicate here whether or not it publishes an `endpoint`,
    /// and for a stronger reason than in `control`: `wait` needs no endpoint at all, so a
    /// run whose control transport never came up is still a perfectly ordinary run to
    /// wait for.
    ///
    /// **The `run_id` nobody registered.** A record is absent for two indistinguishable
    /// reasons — the id was never used, or the run finished and deleted its own entry on
    /// the way out ([`Registration::remove`]) — and the registry keeps no history that
    /// could tell them apart. Both therefore yield [`RunStatus::Finished`]: the same
    /// answer for the same observation, rather than a guess dressed up as a distinct
    /// outcome. See [`crate::wait`] and `docs/registry.md`, "Waiting — `wait`", for the
    /// consequence a caller must plan for (a typo in a `run_id` reads as "already
    /// finished", not as an error).
    ///
    /// Read-only and PID-free like every other scan-side consumer: it opens each lock
    /// file only to test the lock, deletes nothing, creates nothing, and reaches an entry
    /// only through the record path the directory scan produced. Corrupt records `scan`
    /// already skips are invisible here too, exactly as they are to `entries`/`prune`.
    /// Only a wholesale-unreadable registry directory is an `Err` (see [`Registry::scan`]).
    pub fn probe_run(&self, run_id: &str) -> io::Result<RunStatus> {
        self.probe_run_with_jsonl(run_id).map(|probe| probe.status)
    }

    /// The same one-scan verdict as [`Registry::probe_run`], additionally retaining
    /// the JSONL locator from the sole live record. The locator is deliberately
    /// absent for ambiguous, unprobed, stale, and missing observations: only a
    /// confirmed single live run is safe to associate with one outcome stream.
    pub(crate) fn probe_run_with_jsonl(&self, run_id: &str) -> io::Result<RunProbe> {
        let mut live = 0usize;
        let mut unprobed = 0usize;
        let mut jsonl = None;
        for ScannedRecord {
            record, lock_path, ..
        } in self.scan()?
        {
            // Identity first: whether this record is *about* the requested run is
            // decided by `run_id` alone, before any liveness question is asked.
            if record.run_id != run_id {
                continue;
            }
            match probe_health(&lock_path) {
                Ok(LivenessProbe::Live) => {
                    live += 1;
                    jsonl = if live == 1 {
                        record.jsonl.map(PathBuf::from)
                    } else {
                        None
                    };
                }
                // Confirmed stale: this record is a leftover, not a running run, and
                // contributes nothing to either count.
                Ok(LivenessProbe::Stale) => {}
                Err(_) => unprobed += 1,
            }
        }

        if live > 1 {
            // Two or more live runs under one id: nothing per-run can be answered, and
            // guessing would silently wait on whichever entry the scan happened to yield
            // first. Reported ahead of the `unprobed` tally below because a *confirmed*
            // ambiguity is a stronger fact than an unconfirmed liveness.
            return Ok(RunProbe {
                status: RunStatus::Ambiguous { live },
                jsonl: None,
            });
        }
        if live == 1 {
            return Ok(RunProbe {
                status: RunStatus::Live,
                jsonl,
            });
        }
        if unprobed > 0 {
            // Nothing is confirmed live, but something could not be probed — so
            // "finished" is unconfirmed, and saying it would be a fabrication.
            return Ok(RunProbe {
                status: RunStatus::Unprobed,
                jsonl: None,
            });
        }
        Ok(RunProbe {
            status: RunStatus::Finished,
            jsonl: None,
        })
    }

    /// The `.lock` files in the registry directory that have no sibling `.json`
    /// record — the candidates [`Registry::prune`]'s second pass probes. Unlike
    /// [`Registry::scan`]'s `.json` records, there is no per-file corruption guard to
    /// apply here: a `.lock` file name is never deserialized untrusted data (it is
    /// either a filesystem-provided directory-listing name here, or, for a paired
    /// record, the already-validated `lock_file` field [`Registry::scan`] resolves) —
    /// pairing is decided purely by whether `<stem>.json` exists next to it, matching
    /// exactly the on-disk convention [`Registry::reserve_entry`]/[`next_stem`]
    /// establish. A missing directory yields no candidates, exactly as
    /// [`Registry::scan`] treats it as an empty registry.
    ///
    /// A candidate must also be at least [`ORPHAN_LOCK_MIN_AGE`] old (by mtime) — see
    /// that constant's doc for the race this closes ([R-01]). A candidate whose mtime
    /// cannot be read (the entry vanished between the directory read and this stat,
    /// or a transient I/O error) is excluded rather than risk-treating an unreadable
    /// age as "old enough"; a candidate whose mtime is in the future (clock skew) is
    /// excluded for the same reason — it is not affirmatively confirmed old.
    fn orphaned_lock_paths(&self) -> io::Result<Vec<PathBuf>> {
        let read_dir = match fs::read_dir(&self.dir) {
            Ok(read_dir) => read_dir,
            // A missing directory is simply an empty registry.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let now = SystemTime::now();
        let mut orphans = Vec::new();
        for dir_entry in read_dir {
            // Per-item iteration noise is skipped here exactly as in `scan` — a
            // transient error on one entry must not abort the whole listing.
            let Ok(dir_entry) = dir_entry else {
                continue;
            };
            let path = dir_entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("lock") {
                continue;
            }
            match fs::symlink_metadata(path.with_extension("json")) {
                Ok(_) => {
                    // A `.json` sibling exists (whether or not it is itself a valid,
                    // parsable record) — this lock is not an orphan, so leave it to the
                    // paired-record pass above (or, for a corrupt `.json`, to neither
                    // pass, exactly as `scan` already leaves a corrupt record untouched).
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                // An unreadable sibling is unknown, not confirmed absent. The orphan
                // pass deletes only when absence is affirmative, just as its later
                // liveness probe reaps only an affirmative stale verdict.
                Err(_) => continue,
            }
            let is_old_enough = match dir_entry.metadata().and_then(|meta| meta.modified()) {
                Ok(modified) => now
                    .duration_since(modified)
                    .is_ok_and(|age| age >= ORPHAN_LOCK_MIN_AGE),
                Err(_) => false,
            };
            if !is_old_enough {
                // Too young to trust as a confirmed orphan (or its age could not be
                // confirmed at all) — see [`ORPHAN_LOCK_MIN_AGE`]. Leave it alone; it
                // will be reconsidered on a later `prune` once it has aged, if it
                // really is an orphan.
                continue;
            }
            orphans.push(path);
        }
        Ok(orphans)
    }

    /// Scan the registry directory into the records that pass every corruption guard,
    /// each paired with the two on-disk paths it resolves to — the shared read step
    /// under [`Registry::entries`] (which probes each into an [`Entry`]),
    /// [`Registry::prune`] (which reaps only the confirmed-stale ones), and
    /// [`Registry::probe_run`] (which probes only those matching one `run_id`).
    /// Sharing this step guarantees all three paths agree exactly on which records are
    /// corrupt-and-skipped versus real-and-probed, so prune can never act on a record
    /// `entries` would have dropped, and `wait` can never wait on one neither of them
    /// can see. A missing directory is simply an empty registry.
    ///
    /// Two distinct levels of read failure are handled differently. `fs::read_dir`
    /// itself failing (the registry directory as a whole is unreadable — permissions,
    /// or a non-`NotFound` I/O error) is fatal and returned as `Err`, exactly as
    /// before. A failure to iterate a *single* `DirEntry` within an otherwise-readable
    /// directory (a transient filesystem error, or a race with something removing that
    /// one entry between the directory read and this step) is different: it is
    /// per-item noise, the same class of failure as a corrupt record below, and is
    /// skipped so the scan continues over the remaining entries rather than aborting
    /// and discarding every other, healthy one.
    ///
    /// Untested by design, not by oversight: std's `ReadDir::next()` on both target
    /// platforms only ever produces `Err` from the underlying `readdir`/
    /// `FindNextFileW` call failing outright (e.g. `EIO`, a yanked or disconnected
    /// volume, a corrupted directory index) — failure modes with no reliable,
    /// deterministic, cross-platform trigger from a unit test running as an ordinary
    /// user (unlike the sibling per-record probe failures this task's other tests
    /// force via a lock-file-that-is-really-a-directory, see [K-014]/[K-024]: there is
    /// no file to redirect here, only the directory-iteration syscall itself). The
    /// `let Ok(dir_entry) = dir_entry else { continue };` line above is exercised by
    /// every other test in this module through its `Ok` arm; only the `Err` arm goes
    /// unexercised, and is covered by inspection instead: it is a direct structural
    /// mirror of the corrupt-record `let Ok(text) = ... else { continue }` guard two
    /// lines below, which *is* covered.
    fn scan(&self) -> io::Result<Vec<ScannedRecord>> {
        let read_dir = match fs::read_dir(&self.dir) {
            Ok(read_dir) => read_dir,
            // A missing directory is simply an empty registry.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut scanned = Vec::new();
        for dir_entry in read_dir {
            // A single `DirEntry` iteration failure (a transient filesystem error, or a
            // race with something removing the entry between the directory read and
            // this step) is the same class of per-item noise as a corrupt record
            // below: skip it and keep scanning the rest of the directory rather than
            // aborting the whole scan and returning `Err` for every other, healthy
            // entry. Only `fs::read_dir` itself failing (the directory is wholesale
            // unreadable, handled above) is fatal.
            let Ok(dir_entry) = dir_entry else {
                continue;
            };
            let path = dir_entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = read_record_text(&path) else {
                continue;
            };
            let Some(record) = parse_and_validate_record(&text) else {
                continue;
            };
            let lock_path = self.dir.join(&record.liveness.lock_file);
            scanned.push(ScannedRecord {
                record,
                json_path: path,
                lock_path,
            });
        }
        Ok(scanned)
    }

    /// Reserve a unique entry by atomically creating its lock file (`create_new`) and
    /// taking the exclusive lock on it. The stem is a time+counter token with no PID;
    /// uniqueness is guaranteed by the filesystem, so a collision just retries.
    ///
    /// The returned [`ReservedEntry`] carries an armed [`LockCleanupGuard`]: if it is
    /// dropped before [`Registry::register`] publishes the record and disarms it, the
    /// guard deletes the fresh lock file this call just created — the reservation is
    /// then indistinguishable from never having happened, rather than leaking an
    /// orphaned `.lock` with no `.json`.
    ///
    /// **[R-01] safety note.** A freshly `create_new`-d, unique file cannot already be
    /// locked by another *reservation* — but [`Registry::prune`]'s orphan-lock pass
    /// now also probes (and can win the lock on, and delete) unpaired `.lock` files,
    /// so a denied lock, or a lock this call *does* win but that has since been
    /// deleted out from under it, are no longer impossible: they are the signature of
    /// racing a concurrent `prune` in the narrow window between this file's creation
    /// and its lock being taken (see [`ORPHAN_LOCK_MIN_AGE`], which makes the window
    /// vanishingly rare but not provably empty). Both cases below retry with a fresh
    /// stem rather than surface a hard error or publish a record naming a lock file
    /// that is no longer there.
    fn reserve_entry(&self) -> io::Result<ReservedEntry> {
        const MAX_TRIES: u32 = 128;
        for _ in 0..MAX_TRIES {
            let stem = next_stem();
            let lock_path = self.dir.join(format!("{stem}.lock"));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(lock) => {
                    // Bundle the handle with cleanup at the first instant this call
                    // owns a newly-created path. Field drop order closes the handle
                    // before cleanup unlinks the path on every later `?` or retry.
                    let created = CreatedLock::new(lock, lock_path.clone());
                    // A concurrent `prune` orphan-lock probe can legitimately hold
                    // this exact lock (see the doc above) — try a fresh stem instead
                    // of treating the denial as a real error.
                    if !platform::try_lock_exclusive(&created.lock)? {
                        continue;
                    }
                    // The lock is ours, but a concurrent `prune` could have already
                    // reaped this very file between the `create_new` above and the
                    // lock just taken: it may have opened the same path, won the race
                    // for the lock first, deleted the file while holding it, and
                    // released the lock afterwards — leaving this call holding a lock
                    // on an inode no longer reachable at `lock_path`. Confirm the path
                    // still resolves to the exact file this handle holds before
                    // trusting it enough to publish a record naming it; a mismatch
                    // (including the file simply being gone) means try again with a
                    // fresh stem rather than publish a record whose `lock_file` does
                    // not exist on disk.
                    if !platform::lock_path_still_matches(&created.lock, &lock_path)? {
                        continue;
                    }
                    let json_path = self.dir.join(format!("{stem}.json"));
                    return Ok(ReservedEntry {
                        json_path,
                        cleanup: created.cleanup,
                        lock_path,
                        lock: created.lock,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::other(
            "could not allocate a unique registry entry after many attempts",
        ))
    }
}

/// Read one untrusted registry record with a strict byte ceiling. The extra byte
/// distinguishes an exact ceiling-sized file from an oversized one without trusting
/// metadata that could race with a concurrent writer.
fn read_record_text(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(MAX_RECORD_BYTES as usize + 1);
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry record exceeds the maximum supported size",
        ));
    }
    String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// A newly-created lock before it becomes a full [`ReservedEntry`]. Field order is
/// load-bearing: Rust drops `lock` before `cleanup`, so Windows closes the handle
/// before [`LockCleanupGuard`] attempts to unlink the path on an early return.
struct CreatedLock {
    lock: File,
    cleanup: LockCleanupGuard,
}

impl CreatedLock {
    fn new(lock: File, lock_path: PathBuf) -> Self {
        Self {
            lock,
            cleanup: LockCleanupGuard::new(lock_path),
        }
    }
}

/// A reserved-but-not-yet-published entry: its paths, the held lock, and the
/// [`LockCleanupGuard`] backstop that deletes the fresh lock file if the entry is
/// dropped before [`Registry::register`] finishes publishing its record.
struct ReservedEntry {
    json_path: PathBuf,
    lock_path: PathBuf,
    lock: File,
    /// Armed until [`Registry::register`] successfully writes the record and
    /// disarms it; see [`LockCleanupGuard`].
    cleanup: LockCleanupGuard,
}

/// Drop-backstop for a freshly reserved lock file: deletes it if dropped while still
/// armed, i.e. the entry it belongs to was never published.
///
/// [`Registry::reserve_entry`] atomically creates and locks `<stem>.lock` *before*
/// [`Registry::register`] writes `<stem>.json`. If that write never happens — the
/// `fs::write` call itself fails, or `register` returns early for any other reason —
/// the [`ReservedEntry`] simply drops with no record ever written, and without this
/// guard the freshly created lock file would be left on disk forever: invisible to
/// [`Registry::scan`] (which only walks `.json` files), so neither `list` nor the old
/// `prune` could ever see or reap it. This guard closes that leak: it stays armed
/// from creation until `register` explicitly [`disarm`](Self::disarm)s it right after
/// the record write succeeds, and a still-armed guard's `Drop` deletes the lock file.
///
/// On the successful reservation path, the lock is still held by this process at
/// drop time. On an earlier retry or error, a racing orphan-lock probe may briefly
/// hold or may already have removed the path. Cleanup is therefore deliberately
/// best-effort: deleting our fresh path restores the pre-reservation state, while a
/// sharing violation or an already-missing path is harmless and must not replace the
/// original reservation result.
///
/// Carved out as its own type, rather than an `impl Drop` directly on
/// [`ReservedEntry`], because [`Registry::register`] constructs [`Registration`] by
/// moving `reserved.json_path` / `reserved.lock_path` / `reserved.lock` out
/// field-by-field — a partial move Rust forbids from any type that itself
/// implements `Drop`. Only this narrow guard type implements `Drop`; `ReservedEntry`
/// does not, so that partial move keeps compiling.
struct LockCleanupGuard {
    lock_path: PathBuf,
    active: bool,
}

impl LockCleanupGuard {
    /// A freshly armed guard for `lock_path`.
    fn new(lock_path: PathBuf) -> Self {
        Self {
            lock_path,
            active: true,
        }
    }

    /// Disarm the guard: the entry was successfully published, so its lock file must
    /// survive as `<stem>.json`'s sibling, not be deleted when this guard drops.
    /// Consumes `self` — once disarmed there is no path back to "armed" — so its own
    /// `Drop` still runs when this returns, but as a no-op.
    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for LockCleanupGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

/// A live registry entry owned by the running `run` process. Holding it keeps the
/// entry's advisory lock (so the entry reads as live); [`Registration::remove`]
/// tears the entry down on a clean exit.
pub struct Registration {
    json_path: PathBuf,
    lock_path: PathBuf,
    // The open handle *is* the liveness lock: the OS holds the exclusive advisory
    // lock as long as this file stays open, and releases it when the handle closes —
    // including on an abrupt kill, which is what makes an orphaned entry detectably
    // stale. Never read directly; held purely for that side effect.
    #[allow(dead_code)]
    lock: File,
    removed: AtomicBool,
}

impl Registration {
    /// Remove this entry — the clean-exit path. Idempotent and best-effort (a delete
    /// error only means a scanner may later find a self-evidently stale entry, never
    /// a reason to fail an exiting run), mirroring the best-effort container teardown
    /// in [`crate::run`]. The runner calls this from the same site as the
    /// `ProcessGroup` teardown, on every decided ending; [`Drop`] is only a backstop
    /// for early error returns.
    ///
    /// The record file is deleted first so a scanner never observes a record whose
    /// lock file has already gone (which would misread as stale). The lock is
    /// released when this [`Registration`] finally drops.
    pub fn remove(&self) {
        if self.removed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = fs::remove_file(&self.json_path);
        let _ = fs::remove_file(&self.lock_path);
    }

    /// The record file path (tests inspect it).
    #[cfg(test)]
    pub fn record_path(&self) -> &Path {
        &self.json_path
    }

    /// The lock file path (tests probe it).
    #[cfg(test)]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Simulate an abrupt runner death for tests: release the lock but leave both
    /// files on disk, exactly as a killed process would. The entry then reads as
    /// stale purely through the released lock — proving file existence alone does not
    /// imply liveness.
    #[cfg(test)]
    pub fn simulate_abrupt_death(self) {
        // Mark as removed so `Drop` does not delete the files, then let `self` drop:
        // the lock `File` closes, releasing the OS lock like an abrupt kill would.
        self.removed.store(true, Ordering::SeqCst);
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Backstop for return paths that did not call `remove` explicitly (e.g. a
        // setup error after registration). An abrupt kill never runs this, which is
        // exactly why such a death leaves a detectably stale entry behind.
        self.remove();
    }
}

/// The two-way verdict a *successful* liveness probe reaches — deliberately not
/// [`Health`] itself: [`Health::Unprobed`] is what a caller reports when
/// [`probe_health`] returns `Err`, never a value this function constructs, so a
/// separate, genuinely two-variant type lets every match on `Ok(_)` at the call
/// sites stay exhaustive without an `unreachable!()` arm for a case that cannot
/// occur here.
enum LivenessProbe {
    /// No process holds the lock (or the lock file is gone): confirmed stale.
    Stale,
    /// A live runner holds the lock.
    Live,
}

/// Probe an entry's liveness through its lock file, without trusting file existence.
///
/// Trying a non-blocking exclusive lock is the whole test: acquiring it means no
/// live runner holds it, so the entry is stale; being denied means a live runner
/// holds it, so it is live. A missing lock file is stale by definition. When the
/// probe acquires the lock it drops it immediately (the entry is stale, not being
/// claimed) — a client that means to *reclaim* a stale entry would instead keep the
/// lock held. An `Err` means the probe itself could not run at all (see the call
/// sites, which report that as [`Health::Unprobed`] / [`RunStatus::Unprobed`], never
/// as a confirmed verdict this function did not actually reach).
///
/// The lock file is opened *without following a symlink* at its final component
/// ([`platform::open_lock_file`]: `O_NOFOLLOW` on unix, reparse-point rejection on
/// Windows), closing the open-time TOCTOU window that a symlink swapped in after the
/// name check would otherwise open — the probe can only ever touch a regular file
/// inside the registry directory, never a link redirecting elsewhere.
fn probe_health(lock_path: &Path) -> io::Result<LivenessProbe> {
    let lock = match platform::open_lock_file(lock_path) {
        Ok(lock) => lock,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(LivenessProbe::Stale),
        Err(err) => return Err(err),
    };
    if platform::try_lock_exclusive(&lock)? {
        // Acquired: no live holder. Drop the handle here to release it at once.
        drop(lock);
        Ok(LivenessProbe::Stale)
    } else {
        Ok(LivenessProbe::Live)
    }
}

/// The verdict [`probe_for_prune`] returns — the reaping counterpart to the
/// [`Health`] that [`probe_health`] yields, but deliberately keeping the case
/// [`Registry::entries`] discards.
enum PruneProbe {
    /// The entry is **confirmed stale** and safe to reap. Carries the held exclusive
    /// lock when there was a lock file to acquire, so the caller deletes the entry's
    /// files while the lock is still held (nothing can slip in and claim the entry
    /// between the check and the delete); `None` when the lock file was already gone —
    /// the record is an orphan with nothing left to hold.
    Reapable(Option<File>),
    /// A live runner holds the lock: the entry must never be reaped.
    Live,
}

/// Probe an entry's lock file **for pruning**, keeping the same three cases
/// [`Registry::entries`] now keeps apart via [`Health::Unprobed`] (see [K-024]) —
/// but returning the acquired lock alongside the confirmed-stale verdict, which
/// `Entry::health` alone cannot carry. Prune cannot simply read `Entry::health`
/// here: it needs that lock held across its deletions, and a pure liveness query
/// like `entries()` already released it. So this probes on its own path, because
/// prune deletes files and must never act on a record it did not actually confirm dead:
///
/// - lock file **absent** (`NotFound`) ⇒ [`PruneProbe::Reapable`]`(None)` — stale by
///   definition, an orphaned record with no lock left to hold;
/// - lock **acquired** (no live holder) ⇒ [`PruneProbe::Reapable`]`(Some(lock))` —
///   confirmed stale, and the acquired lock is **kept held** and handed back so the
///   reap runs under it (pruning *reclaims* the entry, unlike [`probe_health`], which
///   drops the lock at once for a pure liveness query — the "keep the lock to reclaim"
///   pattern `docs/registry.md` documents);
/// - lock **denied** (a live runner holds it) ⇒ [`PruneProbe::Live`];
/// - any real probe **failure** — the lock file cannot be opened (EISDIR/permission-
///   denied/reparse-point rejection) or the lock call itself errors — is returned as
///   `Err`, so the caller leaves the entry in place rather than deleting an
///   unconfirmed record.
fn probe_for_prune(lock_path: &Path) -> io::Result<PruneProbe> {
    let lock = match platform::open_lock_file(lock_path) {
        Ok(lock) => lock,
        // A missing lock file is stale by definition — and there is no lock to hold.
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(PruneProbe::Reapable(None));
        }
        Err(err) => return Err(err),
    };
    if platform::try_lock_exclusive(&lock)? {
        // Acquired: no live holder. Keep the handle so the reap deletes the files
        // under the still-held lock.
        Ok(PruneProbe::Reapable(Some(lock)))
    } else {
        Ok(PruneProbe::Live)
    }
}

/// Parse and validate a registry record from raw on-disk text, applying the same
/// corruption guards [`Registry::scan`] applies to every `.json` file it reads —
/// valid JSON deserializing to [`Record`], a well-formed `started_at`
/// ([`is_valid_rfc3339_millis_utc`]), and a simple in-directory `lock_file` name
/// ([`is_simple_lock_file_name`]) — and returns `None` for anything that fails
/// any of those guards, the same "corrupt record, skip it" verdict `scan` uses.
/// The two redaction-safe command fields ([`Record::argv_sha256`]/[`Record::hint`])
/// are guarded too, but by *sanitizing* rather than rejecting — see the comment at
/// that step for why a field nothing acts on must not be able to hide a live run.
/// Pure — it never touches the filesystem, unlike `scan` itself, which is what
/// lets this double as the bytes → parse/validate target of the registry-record
/// fuzz tier (`fuzz/fuzz_targets/registry_record.rs`, T-186) without spinning up
/// a real registry directory.
#[doc(hidden)]
pub fn parse_and_validate_record(text: &str) -> Option<Record> {
    let mut record = serde_json::from_str::<Record>(text).ok()?;
    // `started_at` is untrusted deserialized data too: a record written by a
    // well-behaved runner always carries an [`events::format_rfc3339_utc`] value,
    // but a corrupted or hand-edited record could carry anything `serde_json`
    // will accept into a `String` field. A malformed value is corrupt-record
    // noise, not a real start time — reject it like any other corrupt entry
    // rather than listing (and sorting) garbage as if it were valid.
    if record.registry_version != REGISTRY_VERSION
        || record.liveness.kind != LIVENESS_ADVISORY_LOCK
        || !is_valid_rfc3339_millis_utc(&record.started_at)
    {
        return None;
    }
    // The `lock_file` field is untrusted deserialized data. Validate it as a
    // simple, single-component, relative `.lock` name *before* it is ever joined
    // onto the registry directory — a value carrying `..`, a path separator, an
    // absolute path, a NUL/control character, or a Windows reserved device name
    // (even in the name-plus-extension aliasing form) would otherwise let a
    // corrupt or adversarial record steer the liveness probe at a file outside
    // the owner-only registry directory. A failing value is a corrupt record and
    // is rejected, exactly like an unreadable or unparsable file.
    if !is_simple_lock_file_name(&record.liveness.lock_file) {
        return None;
    }
    // The two redaction-safe command fields (T-215) are untrusted deserialized data
    // as well — but unlike the two guards above, a malformed value here **degrades
    // the field, not the record**. The distinction is what the value can do:
    //
    // - `lock_file` is joined onto the registry directory and *opened*, and
    //   `started_at` is what every client sorts, reports, and reasons about a run's
    //   age by. A malformed value there steers an action, so the honest verdict is
    //   "this file is not a record a runner wrote" — skip it entirely.
    // - `argv_sha256`/`hint` steer nothing. Nothing is opened through them, no
    //   deletion is gated on them, no run is resolved by them; they are reported to
    //   an operator and nothing else. Discarding the whole record over one of them
    //   would hand a purely cosmetic field the power to hide a **live** run from
    //   `list`, `prune`, `wait`, and every control client at once — one hand-edited
    //   byte in a live entry's `hint` and `cancel`/`kill` can no longer find the run
    //   they are aimed at. That is a strictly worse failure than losing the field.
    //
    // So a value that is not the exact shape a runner writes is dropped to `None` —
    // "not reported", precisely the value a pre-T-215 record already carries and
    // every consumer already renders — and the record survives intact. A record a
    // live runner wrote can never take this path: `register` publishes only
    // `events::CommandFingerprint`'s own hex digest and catalog labels (see
    // `hint_labels_from_the_real_catalog_pass_the_record_guard`).
    record.argv_sha256 = record
        .argv_sha256
        .filter(|value| is_valid_argv_sha256(value));
    record.hint = record.hint.filter(|value| is_valid_hint(value));
    record
        .labels
        .retain(|key, value| crate::labels::valid_key(key) && crate::labels::valid_value(value));
    // `run_id` and `endpoint` remain byte-for-byte identity/address data here:
    // changing either during parsing would make a later resolver target something
    // other than the record actually says. Human-readable renderers pass them
    // through `crate::text::terminal_safe` at the terminal boundary instead, while
    // JSON output relies on serde's escaping and preserves the original values.
    Some(record)
}

/// The number of hex characters in a SHA-256 digest — the exact length
/// [`crate::hash::sha256_hex`] produces, and therefore the only length
/// [`is_valid_argv_sha256`] accepts.
const SHA256_HEX_LEN: usize = 64;

/// The longest [`Record::hint`] value accepted from disk. Every label the real
/// catalog mints is far shorter (the seed entry is 18 characters); the cap exists
/// only so a corrupt or hand-edited record cannot make `list` print an unbounded
/// string it read off disk.
const MAX_HINT_LEN: usize = 64;

/// Validate a record's `argv_sha256` as exactly what [`crate::hash::sha256_hex`]
/// emits: [`SHA256_HEX_LEN`] characters of **lowercase** hex, nothing else. Length
/// is checked in bytes, which is equivalent to characters here because every byte is
/// then required to be an ASCII hex digit (a multi-byte character fails that check
/// and is rejected before the two could diverge).
///
/// Lowercase specifically, not "hex in either case": `docs/schema.md` pins the
/// fingerprint's rendering as lowercase hex, so an uppercase digest is not a
/// differently-spelled equal value — it is a value no writer of this format
/// produces, and accepting it would let the same run appear under two spellings of
/// one fingerprint, defeating the "same command ⇒ same string" comparison the field
/// exists for.
fn is_valid_argv_sha256(value: &str) -> bool {
    value.len() == SHA256_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Validate a record's `hint` as a plausible catalog label: non-empty, at most
/// [`MAX_HINT_LEN`] bytes, and made only of ASCII lowercase letters, digits, and
/// `_` — the `snake_case` shape `docs/schema.md` requires of every `hint` label
/// ("Choose a stable, snake_case `hint` label").
///
/// Checked by **shape rather than membership** in the current
/// [`events::classify_hint`] catalog on purpose: a record can legitimately be
/// written by a *newer* runner sharing the same per-user registry directory, and a
/// membership test would silently drop a label this binary simply has not heard of
/// yet — the same forward-compatibility trap `docs/schema.md` warns consumers about
/// for new event values. The shape check still removes everything that could make
/// the value more than a category name: an embedded newline (which would forge an
/// extra row in `list`'s table), an ANSI escape or other control character, a
/// separator, whitespace, or an unbounded blob. An anti-drift test asserts every
/// label the real catalog *can* emit passes this.
fn is_valid_hint(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HINT_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Validate that `value` has the exact shape [`events::format_rfc3339_utc`]
/// produces: `YYYY-MM-DDTHH:MM:SS.sssZ`, 24 ASCII bytes, with the four calendar/
/// clock fields in their documented ranges (month 1-12, day valid for that month
/// *and* year — including leap-year February 29 — hour 0-23, minute 0-59, second
/// 0-59). This **is** a full calendar validator: day 31 of a 30-day month, day 30 of
/// February, and February 29 of a non-leap year are all rejected, alongside the pure
/// shape/digit checks — that is enough to catch the corrupt-record case this guards
/// against (garbage, truncated, or wrong-format text swapped into `started_at`),
/// which is the same standard [`is_simple_lock_file_name`] holds `lock_file` to. A
/// live runner only ever writes values this function accepts.
fn is_valid_rfc3339_millis_utc(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24 {
        return false;
    }
    const DIGIT_POSITIONS: [usize; 17] =
        [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22];
    if !DIGIT_POSITIONS.iter().all(|&i| bytes[i].is_ascii_digit()) {
        return false;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return false;
    }
    let four = |i: usize| {
        u32::from(bytes[i] - b'0') * 1000
            + u32::from(bytes[i + 1] - b'0') * 100
            + u32::from(bytes[i + 2] - b'0') * 10
            + u32::from(bytes[i + 3] - b'0')
    };
    let two = |i: usize| u32::from(bytes[i] - b'0') * 10 + u32::from(bytes[i + 1] - b'0');
    let year = four(0);
    let month = two(5);
    let day = two(8);
    let hour = two(11);
    let minute = two(14);
    let second = two(17);
    (1..=12).contains(&month)
        && day >= 1
        && day <= days_in_month(year, month)
        && hour <= 23
        && minute <= 59
        && second <= 59
}

/// Number of days in `month` (1-12) of `year`, per the proleptic Gregorian calendar —
/// including leap-year handling for February (divisible by 4, except centuries not
/// divisible by 400). Only called from [`is_valid_rfc3339_millis_utc`] after `month`
/// has already been range-checked to `1..=12`; any other value falls through to the
/// `_ => 31` arm, which is unreachable in that caller but keeps this total rather
/// than panicking if ever reused elsewhere.
fn days_in_month(year: u32, month: u32) -> u32 {
    let is_leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year => 29,
        2 => 28,
        _ => 31,
    }
}

/// Validate a registry record's `lock_file` field as a **simple, single-component,
/// relative** file name that is safe to resolve against the registry directory. This
/// is a pure check on the string and its path components — it never touches the
/// filesystem — so it runs *before* the value is ever joined onto `self.dir` or
/// opened, and a value that fails it is treated as a corrupt record (the scan skips
/// that entry). A live runner only ever writes the `run-<hex>-<hex>.lock` names
/// [`next_stem`] mints, all of which pass; the guard exists purely for corrupt or
/// adversarial deserialized input.
///
/// Rejected: an empty name; any embedded NUL or control character (a NUL can
/// truncate the name at the OS boundary); any path separator (`/` or `\`) or Windows
/// drive / alternate-data-stream delimiter (`:`); anything that is not exactly one
/// *normal* path component (so `.`, `..`, an absolute path, and a `C:`-style prefix
/// are all out); a name without the expected `.lock` extension; and a Windows
/// reserved device name, including its name-plus-extension aliasing form
/// (see [`is_windows_reserved_device_name`]).
fn is_simple_lock_file_name(name: &str) -> bool {
    // Reject empties and any embedded NUL / control character up front.
    if name.is_empty() || name.chars().any(char::is_control) {
        return false;
    }
    // Reject every path separator and the Windows drive / stream delimiter outright,
    // so the value can never denote a subdirectory, a drive-relative path, or an
    // alternate data stream — regardless of the OS the record is scanned on.
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return false;
    }
    // The value must resolve to exactly one *normal* component equal to itself. This
    // rejects `.`, `..`, absolute paths, and any platform prefix.
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(only)), None) if only.to_str() == Some(name) => {}
        _ => return false,
    }
    // Require the documented `.lock` extension.
    if Path::new(name).extension().and_then(|ext| ext.to_str()) != Some("lock") {
        return false;
    }
    // Finally reject Windows reserved device names (including `NUL.tar.gz.lock`).
    !is_windows_reserved_device_name(name)
}

/// Whether `name` aliases a Windows reserved legacy device name. Win32 treats a file
/// whose base name — the part before the *first* `.` — matches one of these as the
/// device itself, not a file, **regardless of any trailing extension** (so
/// `NUL.tar.gz.lock` still aliases `NUL`). The match is case-insensitive and also
/// covers the Latin-1 superscript digit forms of `COM`/`LPT` (`COM¹`/`COM²`/`COM³`/
/// `LPT¹`/`LPT²`/`LPT³`, code points U+00B9/U+00B2/U+00B3), which current Windows
/// still reserves — only digits 1-3 have such a code point, so there is no
/// superscript form for `COM4`-`COM9`/`LPT4`-`LPT9`. Rejected on every platform (not
/// just Windows) so a record written on one OS cannot alias a device when scanned on
/// another.
fn is_windows_reserved_device_name(name: &str) -> bool {
    // Windows reserves on the base name up to the first dot, ignoring the extension.
    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    // `COMx` / `LPTx` where `x` is an ASCII digit 1-9 or a Latin-1 superscript 1-3.
    for prefix in ["COM", "LPT"] {
        if let Some(ordinal) = base.strip_prefix(prefix)
            && matches!(
                ordinal,
                "1" | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{b9}"
                    | "\u{b2}"
                    | "\u{b3}"
            )
        {
            return true;
        }
    }
    false
}

/// Resolve the registry directory: the env override if set and non-empty, else the
/// platform default.
fn resolve_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(REGISTRY_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    platform::default_registry_dir()
}

/// A unique, opaque, PID-free entry stem: the run start time in nanoseconds plus a
/// per-process counter. Cross-process uniqueness is enforced by the `create_new`
/// that consumes this (a rare collision just retries), so the token never needs a
/// PID — keeping the registry's "nothing is addressed by PID" property intact.
fn next_stem() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("run-{nanos:032x}-{sequence:016x}")
}

/// The file name (final component) of a path as an owned string, lossily. Registry
/// paths are all ASCII stems the code itself builds, so the lossy step never bites.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The current user's SID in its string form (`S-1-5-…`). The registry restricts
/// its directory to exactly this identity; the local control transport
/// ([`crate::control`]) reuses it to build the owner-only DACL for its named pipe,
/// so the pipe and the registry are locked to the same single user. Windows-only —
/// the unix transport gates access through `0700`/`0600` file modes instead.
#[cfg(windows)]
pub(crate) fn current_user_sid_string() -> io::Result<String> {
    platform::current_user_sid_string()
}

#[cfg(unix)]
#[path = "platform/unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "platform/windows.rs"]
mod platform;

/// Shared in-tree fixtures. Public only so integration tests can use the same
/// typed record builder as unit tests; this crate's library API is not supported.
#[doc(hidden)]
pub mod test_support;

#[cfg(test)]
mod tests;
