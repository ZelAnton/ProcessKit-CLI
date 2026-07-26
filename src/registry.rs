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

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::events;

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
/// Nothing acts on either field either: no file is opened, no deletion gated, no run
/// resolved through them (see [`parse_and_validate_record`]), so no reader can be led
/// into a wrong *action* by a value it does not understand. That is exactly the
/// additive case `docs/schema.md`'s own "Versioning" section describes for the event
/// stream, applied on this axis. A bump would be required for the opposite kind of
/// change: renaming/removing/retyping an existing field, changing what a value means,
/// or adding a field a reader must understand to behave correctly.
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
    /// How a client decides whether this record is live or stale — never by the file
    /// merely existing.
    pub liveness: Liveness,
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
        let mut outcome = PruneOutcome::default();
        for ScannedRecord {
            record,
            json_path,
            lock_path,
        } in self.scan()?
        {
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
        let mut outcome = PruneOutcome::default();
        let mut candidates = Vec::new();

        for ScannedRecord {
            record, lock_path, ..
        } in self.scan()?
        {
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
    /// a live-but-endpoint-less duplicate evaded exactly this check in `src/control.rs`).
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
        let mut live = 0usize;
        let mut unprobed = 0usize;
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
                Ok(LivenessProbe::Live) => live += 1,
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
            return Ok(RunStatus::Ambiguous { live });
        }
        if live == 1 {
            return Ok(RunStatus::Live);
        }
        if unprobed > 0 {
            // Nothing is confirmed live, but something could not be probed — so
            // "finished" is unconfirmed, and saying it would be a fabrication.
            return Ok(RunStatus::Unprobed);
        }
        Ok(RunStatus::Finished)
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
            if path.with_extension("json").exists() {
                // A `.json` sibling exists (whether or not it is itself a valid,
                // parsable record) — this lock is not an orphan, so leave it to the
                // paired-record pass above (or, for a corrupt `.json`, to neither
                // pass, exactly as `scan` already leaves a corrupt record untouched).
                continue;
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
            let Ok(text) = fs::read_to_string(&path) else {
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
    if !is_valid_rfc3339_millis_utc(&record.started_at) {
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
mod platform {
    //! Unix registry primitives: `0700` directories and BSD `flock` liveness locks.

    use std::ffi::CString;
    use std::fs::{self, DirBuilder, File, Permissions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use crate::control::{SOCKET_DIR_PREFIX, SOCKET_FILE_NAME, socket_base_dirs};

    /// Owner-only directory: mode `0700`, re-asserted with `chmod` (which, unlike the
    /// initial `mkdir`, is not filtered by the umask) so both a freshly created and a
    /// pre-existing directory end up owner-only.
    pub fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        fs::set_permissions(dir, Permissions::from_mode(0o700))
    }

    /// Open an existing lock file for a liveness probe **without following a symlink**
    /// at its final component. `O_NOFOLLOW` makes the open fail (`ELOOP`) rather than
    /// traverse a symlink swapped in at the lock's name, closing the open-time TOCTOU
    /// window; the registry directory itself is owner-only and created by us, so only
    /// the final component needs guarding. A missing file surfaces as `NotFound` (the
    /// caller reads that as a stale entry).
    pub fn open_lock_file(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }

    /// Try to take a non-blocking exclusive advisory lock. Returns `true` if
    /// acquired, `false` if another open file description already holds it.
    ///
    /// BSD `flock` (not POSIX `fcntl`) is deliberate: its lock is tied to the open
    /// file description, so a *second* handle from the same process still conflicts —
    /// which the same-process stale-detection unit test relies on — and the kernel
    /// releases it when the last such handle closes, including on an abrupt kill.
    pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
        // SAFETY: `file` owns a valid fd for the duration of this call; `flock` only
        // reads it.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // The lock is held elsewhere (EAGAIN and EWOULDBLOCK are the same value on
            // Linux but distinct on some BSDs; accept either).
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(false),
            _ => Err(err),
        }
    }

    /// Does the still-open handle `lock` refer to the exact same file as the current
    /// contents of `lock_path` — the [R-01] identity check [`super::Registry::reserve_entry`]
    /// performs right after taking its exclusive lock. `flock`'s lock lives on the
    /// open file description, not the path: if a concurrent `prune` orphan-lock probe
    /// won the race first, deleted the file, and released the lock before this call's
    /// own `try_lock_exclusive` ran, `lock` would still hold a valid lock on the
    /// now-unlinked inode while `lock_path` no longer names it (or names nothing at
    /// all). Comparing device/inode — via a fresh `lstat` on the path, deliberately
    /// not following a symlink — catches exactly that: a deleted path fails with
    /// `NotFound` (treated as `Ok(false)`, not an error, since "no longer there" is
    /// itself a definitive answer here), and any survivor is compared by identity, not
    /// merely by existing.
    pub fn lock_path_still_matches(lock: &File, lock_path: &Path) -> io::Result<bool> {
        let held = lock.metadata()?;
        let current = match fs::symlink_metadata(lock_path) {
            Ok(current) => current,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        Ok(held.dev() == current.dev() && held.ino() == current.ino())
    }

    /// Per-user default: `$XDG_RUNTIME_DIR/processkit-cli/runs` (a user-only tmpfs,
    /// the natural home for live-run state) when set, else `$HOME/.local/state/...`.
    pub fn default_registry_dir() -> io::Result<PathBuf> {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())
        {
            return Ok(PathBuf::from(runtime).join("processkit-cli").join("runs"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("processkit-cli")
                .join("runs"));
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no XDG_RUNTIME_DIR or HOME to locate the per-user run registry",
        ))
    }

    /// Test-only: does `dir` grant access to its owner alone (mode `0700`)?
    #[cfg(test)]
    pub fn is_owner_only(dir: &Path) -> io::Result<bool> {
        let mode = fs::metadata(dir)?.permissions().mode();
        Ok(mode & 0o777 == 0o700)
    }

    /// The private control-socket directory a record's published `endpoint` names —
    /// or `None` when that value is not, to the letter, the shape this project's own
    /// control server publishes (T-207).
    ///
    /// **`endpoint` is untrusted deserialized data**, exactly like
    /// [`super::Liveness::lock_file`] (see [`super::is_simple_lock_file_name`]): a
    /// corrupt or hand-edited record can carry any string `serde_json` accepts into a
    /// `String` field, and [`super::Registry::prune`] is about to *delete* what this
    /// returns. So the value is never used as a path on trust — it must match the
    /// exact form [`crate::control`]'s `ControlServer::bind` publishes:
    ///
    /// - absolute, and made of nothing but plain `/`-separated names — so a relative
    ///   path, a `.` or `..` segment anywhere, and a doubled separator are all out
    ///   **before** anything is resolved, and no `..` can climb out of the directory
    ///   this returns;
    /// - final component exactly [`SOCKET_FILE_NAME`];
    /// - its parent named [`SOCKET_DIR_PREFIX`] plus a non-empty token of ASCII
    ///   alphanumerics and `-` only — the character set `control::unique_token` mints;
    /// - that parent sitting **directly** inside one of [`socket_base_dirs`]'s bases,
    ///   the very directories `bind` creates it in — so a well-formed-looking path
    ///   anywhere else (`/etc/pkc-x/c.sock`, `$HOME/pkc-x/c.sock`) is refused.
    ///
    /// Both [`super::Registry::prune`] (which reaps the directory) and
    /// [`super::Registry::preview_prune`] (which only reports it) classify through
    /// this one function, so a preview can never name a candidate a real reap would
    /// not touch — the same "one classification, two actions" discipline
    /// [`super::probe_for_prune`] already gives the two passes (see [K-024]).
    ///
    /// This is a **purely lexical** verdict: it touches no filesystem, so it cannot
    /// be raced, and it deliberately does not check that the directory exists (a
    /// preview must not stat anything, and a reap has to survive the directory being
    /// gone anyway). The symlink question is settled where it can be settled without
    /// a TOCTOU window — at open time, in [`reap_control_socket_dir`].
    ///
    /// A run whose socket lived under a *different* temp directory than the pruning
    /// process sees (a changed `TMPDIR` between the run and the prune) simply fails
    /// this check and keeps its socket: refusing to delete an unrecognized path is
    /// the whole point, and the record itself is still reaped.
    pub fn control_socket_dir_to_reap(endpoint: Option<&str>) -> Option<PathBuf> {
        socket_dir_within(endpoint?, &socket_base_dirs())
    }

    /// [`control_socket_dir_to_reap`] against an explicit list of allowed base
    /// directories — the whole check, with its one environment-derived input
    /// ([`socket_base_dirs`]) passed in so it can be exercised deterministically.
    pub fn socket_dir_within(endpoint: &str, bases: &[PathBuf]) -> Option<PathBuf> {
        // Reject any embedded NUL or control character up front: a NUL truncates the
        // name at the OS boundary, and no path this project publishes contains one.
        if endpoint.chars().any(char::is_control) {
            return None;
        }
        // Deliberately parsed as raw `/`-separated segments rather than through
        // `Path::components()`: that iterator *normalizes* — it silently drops `.`
        // and empty (doubled-separator) segments — so a value like `/tmp/./pkc-1/
        // c.sock` would inspect as though it were the published form. Here every
        // segment is checked as written, and `.`/`..`/empty are all refused, so what
        // this returns is a path with no traversal or normalization left in it.
        let mut segments = endpoint.split('/');
        // An absolute path, and only an absolute path, splits with an empty leading
        // segment.
        if segments.next() != Some("") {
            return None;
        }
        let segments: Vec<&str> = segments.collect();
        if segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
        {
            return None;
        }

        // `<base…>/pkc-<token>/c.sock`: the socket file itself, its private directory,
        // and the base directory that directory was created in.
        let [base_segments @ .., dir_name, file_name] = segments.as_slice() else {
            return None;
        };
        if *file_name != SOCKET_FILE_NAME {
            return None;
        }
        let token = dir_name.strip_prefix(SOCKET_DIR_PREFIX)?;
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        // The directory must sit *directly* in a base `ControlServer::bind` uses —
        // one level, no deeper — so nothing outside those bases is ever a candidate.
        // The bases themselves come from this process's own environment, not from the
        // record, so they are compared as paths (`/tmp` and `/tmp/` are one place),
        // while the untrusted part above had to match to the character.
        let base = PathBuf::from(format!("/{}", base_segments.join("/")));
        if !bases.contains(&base) {
            return None;
        }
        Some(base.join(dir_name))
    }

    /// Reap a confirmed-stale record's control socket and the private directory that
    /// holds it, **without ever following a symlink** at either component — the
    /// deletion half of T-207, run only on a directory
    /// [`control_socket_dir_to_reap`] has already validated (never on a raw
    /// `endpoint` value).
    ///
    /// Best-effort by design, exactly like the record/lock deletions it accompanies:
    /// every failure is swallowed, because a socket that could not be removed is a
    /// leftover to retry next pass, never a reason to abort the reaping of other
    /// entries.
    pub fn reap_control_socket_dir(dir: &Path) {
        let _ = remove_socket_dir(dir);
    }

    /// The fallible body of [`reap_control_socket_dir`].
    ///
    /// `O_NOFOLLOW | O_DIRECTORY` is the same open-time discipline
    /// [`open_lock_file`] applies to a lock file (see [K-024]): the `pkc-…`
    /// component must *be* a directory and must not be a symlink, so a link planted
    /// (or swapped in) under that name fails the open outright instead of redirecting
    /// the deletion somewhere else. Everything below then happens **relative to that
    /// open handle**, so even a swap landing after the open cannot misdirect it.
    ///
    /// The directory itself is removed by path — but `rmdir` never follows a symlink
    /// at its final component and only ever removes an *empty* directory, so the
    /// worst a post-open swap can do here is make this call fail. Anything unexpected
    /// left inside (an extra file, a refused non-socket, see [`unlink_socket_in`])
    /// keeps the directory too, rather than being deleted along with it.
    fn remove_socket_dir(dir: &Path) -> io::Result<()> {
        let handle = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir)?;
        unlink_socket_in(&handle)?;
        drop(handle);
        fs::remove_dir(dir)
    }

    /// Unlink the control socket inside an already-opened, already-validated private
    /// directory — addressed *relative to that open handle* (`fstatat`/`unlinkat`),
    /// never by a path that could be re-resolved somewhere else in between.
    ///
    /// The entry is unlinked only when it really is a **socket**, checked through the
    /// same handle and without following a symlink: a regular file, a symlink, a
    /// directory, or a device node under that name is not something a control server
    /// ever created, so it is refused rather than deleted. An entry that is already
    /// gone is not a failure — there is simply nothing left to unlink, and the
    /// directory removal that follows can still proceed.
    fn unlink_socket_in(dir: &File) -> io::Result<()> {
        let name = CString::new(SOCKET_FILE_NAME).map_err(io::Error::other)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `dir` owns a valid directory fd for the whole call and `name` is a
        // NUL-terminated C string that outlives it; both are only read. `stat` is
        // written by the kernel and is read below only after a success return.
        let statted = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if statted != 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(err)
            };
        }
        // SAFETY: the call above returned success, so the kernel initialized `stat`.
        let mode = unsafe { stat.assume_init() }.st_mode;
        if mode & libc::S_IFMT != libc::S_IFSOCK {
            return Err(io::Error::other(
                "the endpoint's file is not a socket; refusing to delete it",
            ));
        }
        // SAFETY: same fd/name validity as the `fstatat` above; `unlinkat` only reads
        // them, and removes exactly one non-directory entry of the open directory.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    //! Windows registry primitives: an owner-only *protected* DACL (the equivalent
    //! of unix `0700`) and `LockFileEx` liveness locks.

    use std::fs::{self, File};
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};

    use crate::win_security::SecurityDescriptor;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_LOCK_VIOLATION, HANDLE, HLOCAL, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        GetFileInformationByHandle, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Owner-only directory: create the chain, then replace its DACL with a protected
    /// (inheritance-blocking) ACL granting full control only to the current user.
    pub fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        restrict_to_current_user(dir)
    }

    /// Open an existing lock file for a liveness probe **without following a reparse
    /// point** (symlink or junction) at its final component. `FILE_FLAG_OPEN_REPARSE_POINT`
    /// yields a handle to the link itself rather than its target — a regular file
    /// ignores the flag and opens as usual — and the handle's attributes are then
    /// checked so a reparse point is rejected outright, closing the open-time TOCTOU
    /// window a symlink swapped in at the lock's name would open. The registry
    /// directory itself is owner-only and created by us, so only the final component
    /// needs guarding. A missing file surfaces as `NotFound` (the caller reads that as
    /// a stale entry).
    pub fn open_lock_file(path: &Path) -> io::Result<File> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry lock file is a reparse point (symlink/junction), not a regular file",
            ));
        }
        Ok(file)
    }

    /// Replace `dir`'s DACL with `D:P(A;OICI;FA;;;<current-user-SID>)`: **P**rotected
    /// (no inherited ACEs — the Windows analogue of not letting a parent's looser
    /// permissions apply), one allow-**F**ull-**A**ccess ACE for the current user,
    /// inherited by child objects and containers (**OICI**). Re-applied on every open,
    /// so a pre-existing directory is locked down too.
    fn restrict_to_current_user(dir: &Path) -> io::Result<()> {
        let sid = current_user_sid_string()?;
        // The inheritable (`OICI`) DACL for a *directory*, converted through the
        // shared RAII wrapper: it owns the LocalAlloc'd descriptor and frees it on
        // drop, so there is no manual `LocalFree` here anymore. The descriptor stays
        // alive across the `apply_dacl` call below and is freed when it drops at the
        // end of this function.
        let descriptor = SecurityDescriptor::from_sddl(&format!("D:P(A;OICI;FA;;;{sid})"))?;
        apply_dacl(dir, descriptor.as_ptr())
    }

    /// Apply the DACL from `descriptor` to `dir` as a protected DACL.
    fn apply_dacl(dir: &Path, descriptor: *mut core::ffi::c_void) -> io::Result<()> {
        let mut present = 0;
        let mut dacl = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: `descriptor` is a valid security descriptor borrowed from the
        // caller's live [`SecurityDescriptor`] (still owned there for this call).
        let ok = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let path = crate::win_security::to_wide(&dir.to_string_lossy());
        // SAFETY: `path` is NUL-terminated; `dacl` points into the live `descriptor`.
        // Owner/group/SACL are left untouched (null). SetNamedSecurityInfoW returns a
        // WIN32_ERROR (0 == success), not last-error.
        let status = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    /// The current user's SID as its string form (e.g. `S-1-5-21-...`).
    ///
    /// `pub(super)` so the crate-level re-export ([`super::current_user_sid_string`])
    /// can hand the same identity to the control transport's owner-only pipe DACL.
    pub(super) fn current_user_sid_string() -> io::Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` is a pseudo-handle needing no close; `token`
        // receives a real handle closed below.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = token_user_sid_string(token);
        // SAFETY: `token` is a valid handle from OpenProcessToken.
        unsafe { CloseHandle(token) };
        result
    }

    /// Read the `TokenUser` SID out of `token` and stringify it.
    fn token_user_sid_string(token: HANDLE) -> io::Result<String> {
        let mut needed = 0u32;
        // SAFETY: the documented sizing call — a null buffer of length 0 fails and
        // writes the required byte count to `needed`.
        let _ =
            unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut buffer = vec![0u8; needed as usize];
        // SAFETY: `buffer` holds `needed` bytes; TokenUser fills a `TOKEN_USER` at its
        // head.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `buffer` now holds a `TOKEN_USER`; its `User.Sid` points within the
        // same buffer, valid until `buffer` drops (after the conversion below).
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        sid_to_string(sid)
    }

    /// Convert a SID pointer to its string form, freeing the allocated string.
    fn sid_to_string(sid: *mut core::ffi::c_void) -> io::Result<String> {
        let mut raw: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` points into a live token buffer; on success `raw` receives a
        // LocalAlloc'd NUL-terminated UTF-16 string freed below.
        let ok = unsafe { ConvertSidToStringSidW(sid, &mut raw) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a valid NUL-terminated UTF-16 string from the converter.
        let string = unsafe { wide_to_string(raw) };
        // SAFETY: `raw` came from ConvertSidToStringSidW (LocalAlloc'd).
        unsafe { LocalFree(raw as HLOCAL) };
        Ok(string)
    }

    /// Try to take a non-blocking exclusive advisory lock on the whole file. Returns
    /// `true` if acquired, `false` if another handle already holds it.
    ///
    /// `LockFileEx` byte-range locks are enforced across handles even within one
    /// process, so a second handle from the same process is denied — mirroring the
    /// unix `flock` semantics the same-process stale-detection test relies on — and
    /// the OS releases the lock when the handle closes, including on an abrupt kill.
    pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
        let handle = file.as_raw_handle() as HANDLE;
        // SAFETY: a zeroed OVERLAPPED means offset 0; the lock covers the whole
        // 64-bit range.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // SAFETY: `handle` is a valid file handle owned by `file`.
        let ok = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if ok != 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(false)
        } else {
            Err(err)
        }
    }

    /// Does the still-open handle `lock` refer to the exact same file as the current
    /// contents of `lock_path` — the [R-01] identity check [`super::Registry::reserve_entry`]
    /// performs right after taking its exclusive lock. See the unix counterpart's doc
    /// for the race this closes (a concurrent `prune` orphan-lock probe winning the
    /// lock first, deleting the file, and releasing the lock before this call's own
    /// `LockFileEx` ran). Identity is compared via the NTFS file index + volume serial
    /// number (the Windows analogue of device/inode) read through
    /// `GetFileInformationByHandle` — std's own `MetadataExt::file_index`/
    /// `volume_serial_number` are gated behind the still-unstable `windows_by_handle`
    /// feature, so this goes straight to the underlying Win32 call instead — reached
    /// through a fresh, reparse-point-rejecting [`open_lock_file`] on the path rather
    /// than trusting mere existence.
    pub fn lock_path_still_matches(lock: &File, lock_path: &Path) -> io::Result<bool> {
        let held = file_identity(lock)?;

        let current = match open_lock_file(lock_path) {
            Ok(file) => file_identity(&file)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        Ok(held == current)
    }

    /// The `(volume serial number, file index)` pair `GetFileInformationByHandle`
    /// reports for an open handle — NTFS's stable per-file identity, the analogue of
    /// a unix `(dev, ino)` pair. Used only by [`lock_path_still_matches`] above.
    fn file_identity(file: &File) -> io::Result<(u32, u64)> {
        // SAFETY: `file` owns a valid handle for the duration of this call;
        // `GetFileInformationByHandle` only reads through it and writes into the
        // zero-initialized, correctly-sized `info` below.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let handle = file.as_raw_handle() as HANDLE;
        // SAFETY: `handle` is valid; `info` is a valid, writable
        // `BY_HANDLE_FILE_INFORMATION` for the call to fill in.
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let file_index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok((info.dwVolumeSerialNumber, file_index))
    }

    /// Per-user default: `%LOCALAPPDATA%\processkit-cli\runs` (already a per-user
    /// location), falling back to the same path built from `%USERPROFILE%`.
    pub fn default_registry_dir() -> io::Result<PathBuf> {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(local).join("processkit-cli").join("runs"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(profile)
                .join("AppData")
                .join("Local")
                .join("processkit-cli")
                .join("runs"));
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no LOCALAPPDATA or USERPROFILE to locate the per-user run registry",
        ))
    }

    /// Read a NUL-terminated UTF-16 string into an owned `String`.
    ///
    /// # Safety
    /// `ptr` must point to a valid NUL-terminated UTF-16 string.
    unsafe fn wide_to_string(ptr: *const u16) -> String {
        let mut len = 0usize;
        // SAFETY: the caller guarantees a NUL-terminated string, so walking to the
        // terminator stays in bounds.
        while unsafe { *ptr.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `ptr..ptr+len` is the string's body per the caller's guarantee.
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        String::from_utf16_lossy(slice)
    }

    /// Test-only: does `dir`'s DACL restrict it to the current user alone — protected
    /// (no inheritance) and granting access to the current user, with no ACE for any
    /// other account (Everyone included)?
    ///
    /// The DACL is verified against the current user's **binary** SID (via [`EqualSid`],
    /// through [`dacl_is_owner_only`]), *not* by string-matching a read-back SDDL. The
    /// production side builds the ACE from the full `S-1-...` SID string
    /// ([`ConvertSidToStringSidW`] never abbreviates), but the read-back converter
    /// [`ConvertSecurityDescriptorToStringSecurityDescriptorW`] renders *well-known* SIDs
    /// as their two-letter SDDL alias. On a normal interactive developer account the user
    /// SID (`S-1-5-21-…-<RID ≥ 1000>`) has no alias, so an old substring match on the
    /// numeric SID happened to pass; but under an account whose SID is well-known — e.g.
    /// the built-in local Administrator (`…-500` → alias `LA`), which is the kind of
    /// elevated account a GitHub Actions `windows-latest` runner executes as — the
    /// read-back SDDL carries the alias instead of the numeric SID and the substring match
    /// spuriously failed, even though the DACL applied to the directory is correct. A
    /// binary SID comparison is account-agnostic and holds for both contexts.
    ///
    /// [`EqualSid`]: windows_sys::Win32::Security::EqualSid
    /// [`ConvertSidToStringSidW`]: windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW
    /// [`ConvertSecurityDescriptorToStringSecurityDescriptorW`]: windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW
    #[cfg(test)]
    pub fn is_owner_only(dir: &Path) -> io::Result<bool> {
        use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
        use windows_sys::Win32::Security::{ACL, GetSecurityDescriptorControl};

        let user_sid = current_user_sid_bytes()?;

        let path = crate::win_security::to_wide(&dir.to_string_lossy());
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated; on success `dacl` points into the
        // LocalAlloc'd `descriptor` (freed below) and stays valid until then.
        // GetNamedSecurityInfoW returns a WIN32_ERROR (0 == success), not last-error.
        let status = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }

        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        // SAFETY: `descriptor` is the security descriptor just read; the out-params
        // receive its control word and revision (always written on success).
        let control_ok =
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        let verdict = if control_ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(dacl_is_owner_only(control, dacl, &user_sid))
        };

        // SAFETY: `descriptor` came from GetNamedSecurityInfoW (LocalAlloc'd).
        unsafe { LocalFree(descriptor as HLOCAL) };
        verdict
    }

    /// Test-only: is `dacl` (with security-descriptor `control` flags) an owner-only
    /// grant to `user_sid` — present, protected (no inherited ACEs), and composed solely
    /// of allow-ACEs naming that one SID? An absent/null DACL (grants everyone), an
    /// unprotected DACL (could inherit wider ACEs), an empty DACL (denies even the
    /// owner), any non-allow ACE, or any ACE for a different account (Everyone included)
    /// all fail the check — making it strictly stronger than the old SDDL scan.
    #[cfg(test)]
    fn dacl_is_owner_only(
        control: u16,
        dacl: *const windows_sys::Win32::Security::ACL,
        user_sid: &[u8],
    ) -> bool {
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, EqualSid, GetAce, SE_DACL_PRESENT, SE_DACL_PROTECTED,
        };

        // The allow-ACE type tag (`ACCESS_ALLOWED_ACE_TYPE`, 0). windows-sys 0.61 does
        // not re-export the constant; the value is a stable part of the ACE ABI.
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

        if dacl.is_null() || control & SE_DACL_PRESENT == 0 || control & SE_DACL_PROTECTED == 0 {
            return false;
        }

        // SAFETY: `dacl` is present and non-null per the guard above.
        let ace_count = unsafe { (*dacl).AceCount };
        // The DACL we apply is exactly one allow-ACE; an empty DACL is not owner-only.
        if ace_count == 0 {
            return false;
        }

        for index in 0..u32::from(ace_count) {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `dacl` is valid and `index` is within `0..AceCount`.
            let got = unsafe { GetAce(dacl, index, &mut ace) };
            if got == 0 || ace.is_null() {
                return false;
            }
            let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: `ace` points at a valid ACE inside the live DACL; reading its
            // header and taking the address of its in-place `SidStart` stays within it.
            let (ace_type, ace_sid) =
                unsafe { ((*ace).Header.AceType, &raw const (*ace).SidStart) };
            if ace_type != ACCESS_ALLOWED_ACE_TYPE {
                // A non-allow ACE (deny/audit/…) means the DACL is more than a plain grant.
                return false;
            }
            // SAFETY: `ace_sid` is the ACE's in-place SID and `user_sid` is our owned copy
            // of the current user's SID; EqualSid only reads both.
            let equal = unsafe {
                EqualSid(
                    ace_sid as *mut core::ffi::c_void,
                    user_sid.as_ptr() as *mut core::ffi::c_void,
                )
            };
            if equal == 0 {
                return false;
            }
        }
        true
    }

    /// Test-only: the current user's binary SID copied into an owned buffer, so it
    /// outlives the process token it was read from.
    #[cfg(test)]
    fn current_user_sid_bytes() -> io::Result<Vec<u8>> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` is a pseudo-handle needing no close; `token`
        // receives a real handle closed below.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = token_user_sid_bytes(token);
        // SAFETY: `token` is a valid handle from OpenProcessToken.
        unsafe { CloseHandle(token) };
        result
    }

    /// Test-only: read the `TokenUser` SID out of `token` and copy its bytes into an
    /// owned buffer (sized with [`GetLengthSid`]).
    ///
    /// [`GetLengthSid`]: windows_sys::Win32::Security::GetLengthSid
    #[cfg(test)]
    fn token_user_sid_bytes(token: HANDLE) -> io::Result<Vec<u8>> {
        use windows_sys::Win32::Security::GetLengthSid;

        let mut needed = 0u32;
        // SAFETY: the documented sizing call — a null buffer of length 0 fails and
        // writes the required byte count to `needed`.
        let _ =
            unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut buffer = vec![0u8; needed as usize];
        // SAFETY: `buffer` holds `needed` bytes; TokenUser fills a `TOKEN_USER` at its
        // head.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `buffer` now holds a `TOKEN_USER` whose `User.Sid` points within it;
        // `GetLengthSid` reads only the SID's own header to size it.
        let (sid, len) = unsafe {
            let sid = (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid;
            (sid, GetLengthSid(sid) as usize)
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `sid..sid+len` is the SID's own storage inside the live `buffer`.
        let bytes = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), len) };
        Ok(bytes.to_vec())
    }

    /// The Windows twin of the unix control-socket classifier (T-207): **never** a
    /// candidate, because there is nothing on disk to reap.
    ///
    /// A Windows run publishes a *named pipe* (`\\.\pipe\processkit-cli-…`, see
    /// [`crate::control`]), which lives in the kernel object namespace rather than
    /// the filesystem: the last handle to it closes when its creator dies — abruptly
    /// or not — and the name disappears with it. There is no leftover directory to
    /// accumulate, so the reap this classifies for is a unix-only concern and every
    /// endpoint here classifies `None`. That also means [`super::Registry::prune`]'s
    /// and [`super::Registry::preview_prune`]'s behavior on Windows is exactly what
    /// it was before T-207.
    pub fn control_socket_dir_to_reap(_endpoint: Option<&str>) -> Option<PathBuf> {
        None
    }

    /// The Windows twin of the unix control-socket reaper (T-207): a no-op, and in
    /// practice unreachable — [`control_socket_dir_to_reap`] never yields a directory
    /// to pass it. It exists so the shared `prune`/`preview_prune` code can classify
    /// and act through one pair of platform functions without a `cfg` of its own,
    /// exactly as it already does for [`open_lock_file`]/[`try_lock_exclusive`].
    pub fn reap_control_socket_dir(_dir: &Path) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique, empty scratch directory for a test registry.
    fn scratch(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-registry-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Test-only: set `path`'s mtime `age` in the past, without a real sleep — used
    /// to age an orphan-lock fixture past [`ORPHAN_LOCK_MIN_AGE`] so `prune`'s second
    /// pass actually considers it a candidate (see [R-01]). Works for both a regular
    /// file and a directory (the [K-014] fixture the probe-error orphan test below
    /// uses is a directory), which is why unix opens it plainly (permission to change
    /// an owned file/directory's timestamps does not depend on the fd's access mode)
    /// while Windows must explicitly ask for `FILE_FLAG_BACKUP_SEMANTICS` to get a
    /// handle on a directory at all, plus write access for `SetFileTime`.
    #[cfg(unix)]
    fn backdate(path: &Path, age: Duration) {
        let file = File::open(path).expect("open the fixture to backdate its mtime");
        file.set_modified(SystemTime::now() - age)
            .expect("backdate the fixture's mtime");
    }

    #[cfg(windows)]
    fn backdate(path: &Path, age: Duration) {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("open the fixture to backdate its mtime");
        file.set_modified(SystemTime::now() - age)
            .expect("backdate the fixture's mtime");
    }

    /// The registry directory is created restricted to its owner (`0700` / an
    /// owner-only protected DACL) — a control channel address must not be world
    /// readable.
    #[test]
    fn directory_is_created_owner_only() {
        let dir = scratch("perms");
        let _registry = Registry::open_in(dir.clone()).expect("open registry");
        assert!(
            platform::is_owner_only(&dir).expect("read permissions"),
            "the registry directory must be restricted to its owner"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `open_read_only` is `list`'s entry point and must never create registry
    /// state: scanning an empty registry (one whose directory does not exist yet)
    /// must leave the directory absent, not conjure it into existence just to
    /// discover there is nothing in it.
    #[test]
    fn open_read_only_does_not_create_the_directory() {
        let dir = scratch("read-only-absent");
        assert!(!dir.exists(), "the scratch fixture starts absent");

        let registry = Registry::open_read_only_in(dir.clone());
        assert!(
            !dir.exists(),
            "a read-only open must not create the registry directory"
        );
        assert!(
            registry.entries().expect("scan").is_empty(),
            "a missing directory reads back as an empty registry"
        );
        assert!(
            !dir.exists(),
            "scanning a missing directory must not create it either"
        );
    }

    /// `open_read_only` must not re-assert (or otherwise touch) the permissions of
    /// an *existing* registry directory — only the mutating [`Registry::open`] /
    /// [`Registry::open_in`] path is allowed to do that. Unix-only: it is the
    /// platform whose owner-only enforcement (`chmod`) is cheap to defeat and
    /// re-check from a plain `std::fs` test without extra Windows ACL plumbing.
    #[cfg(unix)]
    #[test]
    fn open_read_only_does_not_touch_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("read-only-existing-perms");
        let _mutating = Registry::open_in(dir.clone()).expect("create the registry once");
        assert!(platform::is_owner_only(&dir).expect("read permissions"));

        // Loosen the directory's permissions out-of-band, simulating an operator (or
        // a prior process) having widened them for some unrelated reason.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("loosen permissions");

        let read_only = Registry::open_read_only_in(dir.clone());
        assert!(
            read_only.entries().expect("scan").is_empty(),
            "an empty existing directory still reads back empty"
        );
        assert!(
            !platform::is_owner_only(&dir).expect("read permissions"),
            "a read-only open must leave a pre-existing directory's permissions alone"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A registered run writes a well-formed record: the run id, the endpoint it was
    /// given (here `None`), the start timestamp, and the advisory-lock liveness
    /// signal — and carries no PID.
    #[test]
    fn register_writes_a_record_without_a_pid() {
        let dir = scratch("record");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let started = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let registration = registry
            .register_plain("run-42", None, started)
            .expect("register run");

        let text = fs::read_to_string(registration.record_path()).expect("read record");
        let record: Record = serde_json::from_str(&text).expect("parse record");
        assert_eq!(record.run_id, "run-42");
        assert_eq!(record.registry_version, REGISTRY_VERSION);
        assert!(
            record.endpoint.is_none(),
            "register stores the endpoint it is given verbatim — here None"
        );
        assert_eq!(record.started_at, events::format_rfc3339_utc(started));
        assert_eq!(record.liveness.kind, LIVENESS_ADVISORY_LOCK);
        assert!(record.liveness.lock_file.ends_with(".lock"));
        assert!(
            !text.contains("\"pid\""),
            "a record must not be keyed by PID: {text}"
        );

        registration.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// T-215's producer side: a registered run publishes the two redaction-safe
    /// command fields — the fingerprint the JSONL stream carries for the same run,
    /// and the worker-shape hint — and publishes **nothing else** about the command.
    /// The argv here is a recognized MSBuild worker shape carrying a secret-looking
    /// token, so the test pins both at once: the classified hint is written, and no
    /// fragment of the command line reaches the on-disk record.
    #[test]
    fn register_publishes_a_fingerprint_and_hint_but_never_argv() {
        let dir = scratch("record-command");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let argv = [
            "C:\\dotnet\\MSBuild.dll",
            "/nodemode:1",
            "/nodeReuse:true",
            "/p:ApiKey=hunter2-do-not-log",
        ];
        let fingerprint = events::CommandFingerprint::for_argv(argv);
        let registration = registry
            .register("run-cmd", None, SystemTime::now(), &fingerprint)
            .expect("register run");

        let text = fs::read_to_string(registration.record_path()).expect("read record");
        let record: Record = serde_json::from_str(&text).expect("parse record");
        assert_eq!(
            record.argv_sha256.as_deref(),
            Some(fingerprint.argv_sha256.as_str()),
            "the record carries the same fingerprint the run's events carry"
        );
        assert_eq!(
            record.hint.as_deref(),
            Some("msbuild_node_reuse"),
            "a recognized worker shape is published as its catalog label"
        );
        for fragment in ["hunter2", "ApiKey", "MSBuild.dll", "nodeReuse"] {
            assert!(
                !text.contains(fragment),
                "no argv content may reach a registry record ({fragment:?}): {text}"
            );
        }

        registration.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// The same fields, for the common case: an argv matching no catalog rule still
    /// gets a fingerprint (it is derived from argv, so it always exists) but no hint
    /// — `null`, not an invented label.
    #[test]
    fn register_publishes_no_hint_for_an_unrecognized_command() {
        let dir = scratch("record-command-unclassified");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register(
                "run-plain",
                None,
                SystemTime::now(),
                &events::CommandFingerprint::for_argv(["cmd", "/c", "echo hi"]),
            )
            .expect("register run");

        let record: Record =
            serde_json::from_str(&fs::read_to_string(registration.record_path()).expect("read"))
                .expect("parse record");
        assert!(
            record
                .argv_sha256
                .as_deref()
                .is_some_and(is_valid_argv_sha256),
            "every run publishes a well-formed fingerprint: {:?}",
            record.argv_sha256
        );
        assert!(
            record.hint.is_none(),
            "an unrecognized shape publishes no hint: {:?}",
            record.hint
        );

        registration.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Backward compatibility, the read side of T-215's additive change: a record
    /// written **before** these fields existed — no `argv_sha256`, no `hint` key at
    /// all — still parses, with both fields simply absent. It is scanned, probed, and
    /// listed exactly as it always was; nothing about the entry depends on the new
    /// fields being there.
    #[test]
    fn a_record_without_the_command_fields_still_reads() {
        let dir = scratch("record-legacy");
        fs::create_dir_all(&dir).expect("create the registry directory");
        // Byte-for-byte the record shape a pre-T-215 runner wrote.
        let legacy = "{\"registry_version\":1,\"run_id\":\"legacy\",\"endpoint\":null,\
             \"started_at\":\"2026-07-22T00:00:00.000Z\",\
             \"liveness\":{\"kind\":\"advisory_lock\",\"lock_file\":\"legacy.lock\"}}";
        let record = parse_and_validate_record(legacy).expect("a pre-T-215 record still parses");
        assert_eq!(record.run_id, "legacy");
        assert!(
            record.argv_sha256.is_none() && record.hint.is_none(),
            "absent fields read back as absent, never as an error or a fabricated value"
        );

        // …and the whole scan path agrees: the entry is found and probed as usual.
        fs::write(dir.join("legacy.json"), legacy).expect("write the legacy record");
        fs::write(dir.join("legacy.lock"), b"").expect("write an unlocked lock file");
        let entries = Registry::open_read_only_in(dir.clone())
            .entries()
            .expect("scan");
        assert_eq!(
            entries.len(),
            1,
            "a legacy record is a perfectly good entry"
        );
        assert_eq!(entries[0].record.run_id, "legacy");
        assert_eq!(
            entries[0].health,
            Health::Stale,
            "its liveness is decided by its lock, exactly as before"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The other direction of the same compatibility claim (the one that makes
    /// [`REGISTRY_VERSION`] not need a bump): a record written by a **newer** writer,
    /// carrying a field this binary has never heard of, is read as an ordinary
    /// record — the unknown field is ignored, not treated as corruption. Both
    /// directions matter in the mixed registry a mid-upgrade user actually has.
    #[test]
    fn a_record_with_an_unknown_field_still_reads() {
        let from_the_future = "{\"registry_version\":1,\"run_id\":\"future\",\"endpoint\":null,\
             \"started_at\":\"2026-07-22T00:00:00.000Z\",\
             \"argv_sha256\":null,\"hint\":null,\"some_future_field\":{\"a\":1},\
             \"liveness\":{\"kind\":\"advisory_lock\",\"lock_file\":\"future.lock\"}}";
        let record =
            parse_and_validate_record(from_the_future).expect("an unknown field is not corruption");
        assert_eq!(record.run_id, "future");
    }

    /// The Drop-backstop this task adds: a [`ReservedEntry`] that is dropped before
    /// its record is ever published (here simulated directly, the same shape
    /// `Registry::register` hits when its `fs::write` of the JSON record fails and
    /// returns early with `?`, before it ever calls `disarm`) must delete its
    /// freshly created `.lock` file — never leave it as an orphan invisible to
    /// `scan()` (which only walks `.json` files).
    #[test]
    fn reserved_entry_drop_backstop_removes_the_lock_file_when_never_published() {
        let dir = scratch("reserve-drop-backstop");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let reserved = registry.reserve_entry().expect("reserve an entry");
        let lock_path = reserved.lock_path.clone();
        assert!(
            lock_path.exists(),
            "reserve_entry creates the lock file up front"
        );

        // Never publish the record (no `fs::write` of the `.json`, no `disarm`) —
        // just drop the reservation, exactly as an early `?` return in `register`
        // would.
        drop(reserved);

        assert!(
            !lock_path.exists(),
            "dropping an unpublished reservation must remove its lock file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// T-230 regression at the earlier boundary: cleanup is useful before a
    /// [`ReservedEntry`] exists at all. `reserve_entry` now constructs the guard
    /// immediately after `create_new`, so an error or retry in either lock probe
    /// drops this exact shape and removes the path best-effort.
    #[test]
    fn early_reservation_cleanup_removes_the_new_lock_path() {
        let dir = scratch("reserve-early-cleanup");
        fs::create_dir_all(&dir).expect("create scratch registry directory");
        let lock_path = dir.join("early.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .expect("create the lock file");
        let created = CreatedLock::new(lock, lock_path.clone());

        drop(created);

        assert!(
            !lock_path.exists(),
            "an armed guard removes the path even before ReservedEntry construction"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A clean exit removes the entry: files gone, and the scan sees nothing.
    #[test]
    fn clean_removal_deletes_the_entry() {
        let dir = scratch("remove");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register_plain("run-clean", None, SystemTime::now())
            .expect("register run");
        let record_path = registration.record_path().to_owned();
        let lock_path = registration.lock_path().to_owned();

        assert_eq!(registry.entries().expect("scan").len(), 1);
        assert!(record_path.exists() && lock_path.exists());

        registration.remove();
        assert!(
            !record_path.exists() && !lock_path.exists(),
            "a clean exit must delete both entry files"
        );
        assert!(
            registry.entries().expect("scan").is_empty(),
            "a removed entry must not be listed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The heart of the task: an abruptly-killed runner leaves its record *and* lock
    /// file on disk, yet the entry is detectably stale — because liveness is the
    /// released lock, not the file's existence.
    #[test]
    fn stale_entry_is_detected_without_relying_on_file_existence() {
        let dir = scratch("stale");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register_plain("run-victim", None, SystemTime::now())
            .expect("register run");
        let record_path = registration.record_path().to_owned();
        let lock_path = registration.lock_path().to_owned();

        // While the runner is alive it holds the lock: the entry reads as live.
        let live = registry.entries().expect("scan");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].health, Health::Live);

        // Simulate an abrupt kill: release the lock but leave the files behind.
        registration.simulate_abrupt_death();

        // The files still exist — so file existence cannot be what marks staleness…
        assert!(
            record_path.exists() && lock_path.exists(),
            "the abrupt-death fixture must leave both files on disk"
        );
        // …yet the released lock makes the entry detectably stale.
        let stale = registry.entries().expect("scan");
        assert_eq!(stale.len(), 1);
        assert_eq!(
            stale[0].health,
            Health::Stale,
            "an entry whose runner died must read as stale"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Concurrent runs get independent entries: distinct files, both live, and
    /// removing one leaves the other untouched.
    #[test]
    fn concurrent_runs_get_independent_entries() {
        let dir = scratch("concurrent");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let now = SystemTime::now();
        let first = registry
            .register_plain("run-a", None, now)
            .expect("register a");
        let second = registry
            .register_plain("run-b", None, now)
            .expect("register b");
        assert_ne!(
            first.record_path(),
            second.record_path(),
            "each run gets its own file"
        );

        let both = registry.entries().expect("scan");
        assert_eq!(both.len(), 2);
        assert!(both.iter().all(|entry| entry.health == Health::Live));

        first.remove();
        let remaining = registry.entries().expect("scan");
        assert_eq!(remaining.len(), 1, "removing one leaves the other");
        assert_eq!(remaining[0].record.run_id, "run-b");
        assert_eq!(
            remaining[0].health,
            Health::Live,
            "the surviving run stays live"
        );

        second.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Write a hand-crafted registry record (`<stem>.json`) with a chosen `lock_file`
    /// value, simulating a corrupt or adversarial deserialized entry a real runner
    /// would never write (`register` only ever mints a safe `run-<hex>-<hex>.lock`).
    fn write_record(dir: &Path, stem: &str, run_id: &str, lock_file: &str) {
        write_record_with_endpoint(dir, stem, run_id, lock_file, None);
    }

    /// Like [`write_record`], but also publishing an `endpoint` — the control-transport
    /// address a record carries, and the value T-207's socket reap validates by shape
    /// before it deletes anything through it.
    fn write_record_with_endpoint(
        dir: &Path,
        stem: &str,
        run_id: &str,
        lock_file: &str,
        endpoint: Option<&str>,
    ) {
        let record = Record {
            registry_version: REGISTRY_VERSION,
            run_id: run_id.to_string(),
            endpoint: endpoint.map(str::to_string),
            started_at: events::format_rfc3339_utc(SystemTime::now()),
            // These fixtures exist to exercise the `lock_file`/`endpoint` guards;
            // publishing no command metadata keeps them focused (and keeps them
            // covering the "record without it" shape every consumer must handle).
            argv_sha256: None,
            hint: None,
            liveness: Liveness {
                kind: LIVENESS_ADVISORY_LOCK.to_string(),
                lock_file: lock_file.to_string(),
            },
        };
        let json = serde_json::to_string(&record).expect("serialize the record");
        fs::write(dir.join(format!("{stem}.json")), json).expect("write the record");
    }

    /// A unique, not-yet-created path of exactly the shape `ControlServer::bind`
    /// creates a private control-socket directory at — `pkc-<token>` directly inside
    /// the platform temp directory, which is always one of `control::socket_base_dirs`'
    /// bases. The token is per-call-site plus a process-wide counter on top of the
    /// pid, for the same reason [`scratch`] carries one (see [K-026]).
    ///
    /// Deliberately much shorter than a [`scratch`] name: a real unix socket is bound
    /// inside it, and the whole path has to stay within `sockaddr_un::sun_path` on the
    /// shortest platform — macOS, whose temp directory is itself ~50 characters (see
    /// [K-009]). Keep the tags short for the same reason.
    #[cfg(unix)]
    fn socket_dir_path(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "{}t{tag}-{}-{n}",
            crate::control::SOCKET_DIR_PREFIX,
            std::process::id()
        ))
    }

    /// The counterpart to [`socket_dir_path`] for a directory that must **not** be a
    /// reap candidate: a unique, short, not-yet-created directory in the platform temp
    /// directory whose name is not the published `pkc-` form, used as the parent of an
    /// off-base fixture (or as a symlink's target). Short for the same `sun_path`
    /// reason [`socket_dir_path`] is.
    #[cfg(unix)]
    fn off_base_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pkt{tag}-{}-{n}", std::process::id()))
    }

    /// A ready-made leftover of an abruptly-killed runner's control transport: the
    /// private `pkc-…` directory of [`socket_dir_path`], with a **real** bound unix
    /// socket inside it. Returns the directory and the endpoint string a record would
    /// publish for it.
    #[cfg(unix)]
    fn socket_fixture(tag: &str) -> (PathBuf, String) {
        let dir = socket_dir_path(tag);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("create the private control-socket directory");
        let endpoint = bind_socket_in(&dir);
        (dir, endpoint)
    }

    /// Bind a real unix socket at `<dir>/c.sock` and return its path as the endpoint a
    /// record publishes. The listener is dropped immediately on purpose: a bound unix
    /// socket file outlives its listener (only an unlink removes it), which is exactly
    /// the leftover an abruptly-killed runner strands on disk.
    #[cfg(unix)]
    fn bind_socket_in(dir: &Path) -> String {
        let path = dir.join(crate::control::SOCKET_FILE_NAME);
        let listener = std::os::unix::net::UnixListener::bind(&path)
            .expect("bind the fixture's control socket");
        drop(listener);
        path.to_str()
            .expect("the fixture's socket path is UTF-8")
            .to_string()
    }

    /// Like [`write_record`], but with an explicit `started_at` string instead of
    /// the current time — for exercising [`is_valid_rfc3339_millis_utc`]'s
    /// corrupt-record guard with values a real runner would never write.
    fn write_record_with_started_at(dir: &Path, stem: &str, run_id: &str, started_at: &str) {
        let record = Record {
            registry_version: REGISTRY_VERSION,
            run_id: run_id.to_string(),
            endpoint: None,
            started_at: started_at.to_string(),
            argv_sha256: None,
            hint: None,
            liveness: Liveness {
                kind: LIVENESS_ADVISORY_LOCK.to_string(),
                lock_file: format!("{stem}.lock"),
            },
        };
        let json = serde_json::to_string(&record).expect("serialize the record");
        fs::write(dir.join(format!("{stem}.json")), json).expect("write the record");
    }

    /// A platform-absolute path (never a simple in-directory name).
    fn absolute_escape() -> &'static str {
        if cfg!(windows) {
            "C:\\Windows\\Temp\\escape.lock"
        } else {
            "/tmp/escape.lock"
        }
    }

    /// A registry record's raw JSON with `argv_sha256`/`hint` set to arbitrary
    /// (here deliberately malformed) values — the corrupt or hand-edited shape no
    /// runner writes, for exercising the read-side guards on those two fields.
    fn record_json_with_command_fields(run_id: &str, argv_sha256: &str, hint: &str) -> String {
        // Built through `serde_json` rather than string-formatted so a value
        // carrying quotes/newlines/control characters is escaped into *valid* JSON:
        // the point of these fixtures is a well-formed file with a bad field value,
        // not a broken file the JSON parser would reject before any guard ran.
        serde_json::json!({
            "registry_version": REGISTRY_VERSION,
            "run_id": run_id,
            "endpoint": serde_json::Value::Null,
            "started_at": "2026-07-22T00:00:00.000Z",
            "argv_sha256": argv_sha256,
            "hint": hint,
            "liveness": { "kind": LIVENESS_ADVISORY_LOCK, "lock_file": format!("{run_id}.lock") },
        })
        .to_string()
    }

    /// The read-side contract for the two new fields: a value that is not the exact
    /// shape a runner writes is **dropped**, and the record itself survives. Every
    /// other field keeps its value, so the entry stays fully usable — the field is
    /// simply "not reported", the same state a record written before these fields
    /// existed is in.
    #[test]
    fn a_malformed_command_field_is_dropped_not_the_record() {
        let record = parse_and_validate_record(&record_json_with_command_fields(
            "victim",
            // Uppercase hex: a digest no writer of this format produces.
            "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
            // A label carrying a newline — what would otherwise forge an extra row
            // in `list`'s table — plus an ANSI escape.
            "msbuild\n\u{1b}[31mFAKE-ROW",
        ))
        .expect("a malformed command field must not discard the record");
        assert_eq!(record.run_id, "victim", "every other field is untouched");
        assert_eq!(record.started_at, "2026-07-22T00:00:00.000Z");
        assert!(
            record.argv_sha256.is_none(),
            "a malformed fingerprint is dropped: {:?}",
            record.argv_sha256
        );
        assert!(
            record.hint.is_none(),
            "a malformed hint is dropped: {:?}",
            record.hint
        );
    }

    /// Why that is the right verdict, demonstrated where it actually bites: a
    /// **live** run whose record has a corrupt `hint` (a hand-edited byte, a partial
    /// write) stays visible to the scan every client shares. Discarding the record
    /// over a field nothing acts on would hide a running run from `list`, and with it
    /// from `wait` and from the `inspect`/`cancel`/`kill` resolution that matches on
    /// `run_id` — a cosmetic field silently disarming the control plane.
    #[test]
    fn a_live_entry_with_a_corrupt_hint_is_still_found() {
        let dir = scratch("corrupt-hint-live");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register_plain("still-running", None, SystemTime::now())
            .expect("register run");

        // Corrupt only the `hint` value of the published record, leaving the held
        // liveness lock — and every other field, including the `lock_file` name that
        // points at it — exactly as they were.
        let corrupted = record_json_with_command_fields(
            "still-running",
            &crate::hash::sha256_hex(b"whatever"),
            "not a valid label!",
        )
        .replace("still-running.lock", &file_name(registration.lock_path()));
        fs::write(registration.record_path(), corrupted)
            .expect("rewrite the record with a corrupt hint");

        let entries = registry.entries().expect("scan");
        assert_eq!(
            entries.len(),
            1,
            "a corrupt cosmetic field must not hide a live run from the scan"
        );
        assert_eq!(entries[0].record.run_id, "still-running");
        assert_eq!(
            entries[0].health,
            Health::Live,
            "the run is still live, and still reported as such"
        );
        assert!(
            entries[0].record.hint.is_none(),
            "the unusable value itself is dropped: {:?}",
            entries[0].record.hint
        );

        registration.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// [`is_valid_argv_sha256`]'s boundary table. A hand-rolled validator is exactly
    /// the kind that passes by inspection and fails on an edge case ([K-030]), so
    /// every boundary is spelled out: the accepted length either side, the case of
    /// the hex digits, a non-hex letter, and a multi-byte character that makes the
    /// byte length "right" while the character length is not.
    #[test]
    fn argv_sha256_guard_accepts_only_a_full_lowercase_hex_digest() {
        let real = crate::hash::sha256_hex(b"processkit-cli");
        assert!(
            is_valid_argv_sha256(&real),
            "the digest this project actually produces must pass: {real}"
        );
        assert!(is_valid_argv_sha256(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));

        for rejected in [
            // Empty, and the two lengths either side of 64.
            "",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            // Uppercase hex — a spelling no writer of this format emits, and a
            // second spelling of one fingerprint if accepted.
            "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF",
            // A non-hex letter, in the first and in the last position.
            "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
            // Surrounding or embedded whitespace, and a control character.
            " 123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd\ne",
            // 64 *bytes* but 63 characters: the length check alone would pass it,
            // the per-byte hex check is what refuses it.
            "α123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            assert!(
                !is_valid_argv_sha256(rejected),
                "not a lowercase-hex SHA-256 digest: {rejected:?}"
            );
        }
    }

    /// [`is_valid_hint`]'s boundary table, in the same [K-030] spirit: the label
    /// shape `docs/schema.md` requires is accepted at both length boundaries, and
    /// everything that would make the value more than a category name — a
    /// separator, whitespace, a newline forging a table row, an ANSI escape, a NUL,
    /// a non-ASCII character, or an unbounded blob — is refused.
    #[test]
    fn hint_guard_accepts_label_shapes_and_refuses_everything_else() {
        for accepted in [
            "msbuild_node_reuse",
            "a",
            "gradle_daemon_7",
            "_leading_underscore",
            &"x".repeat(MAX_HINT_LEN),
        ] {
            assert!(
                is_valid_hint(accepted),
                "a plain snake_case label must be accepted: {accepted:?}"
            );
        }

        for rejected in [
            "",
            &"x".repeat(MAX_HINT_LEN + 1),
            "MSBuild_Node_Reuse",
            "msbuild node reuse",
            "msbuild-node-reuse",
            "msbuild.node.reuse",
            "msbuild/node",
            "msbuild\nnode",
            "msbuild\u{1b}[31m",
            "msbuild\0node",
            "msbuildα",
        ] {
            assert!(
                !is_valid_hint(rejected),
                "not a category label: {rejected:?}"
            );
        }
    }

    /// Anti-drift, in both directions of the one contract that spans two modules:
    /// every label the **real** classifier catalog can emit passes the record guard
    /// that reads it back. A new `HINT_RULES` entry spelled in a shape this guard
    /// refuses would otherwise publish a label that silently vanished at scan time —
    /// visible nowhere except as a mysteriously empty column. Asserted against the
    /// catalog itself ([`events::hint_labels`]), never a copy of it.
    #[test]
    fn hint_labels_from_the_real_catalog_pass_the_record_guard() {
        let mut labels = 0usize;
        for label in events::hint_labels() {
            assert!(
                is_valid_hint(label),
                "the classifier can emit {label:?}, so the record guard must accept it"
            );
            labels += 1;
        }
        assert!(
            labels > 0,
            "the catalog is not empty, so this asserted something"
        );
    }

    /// The names a live runner actually mints, plus benign edge cases that merely
    /// *resemble* a reserved device, are all accepted — the guard must not discard a
    /// legitimate entry (the positive case).
    #[test]
    fn simple_lock_file_names_are_accepted() {
        for name in [
            "run-00000000000000000000000000000000-0000000000000000.lock",
            "run-0123456789abcdef.lock",
            "a.lock",
            // Resembles a device name but is not one: extra letters / an out-of-range
            // ordinal / no ordinal at all.
            "console.lock",
            "nula.lock",
            "com10.lock",
            "com0.lock",
            "lpt.lock",
        ] {
            assert!(
                is_simple_lock_file_name(name),
                "a plain single-component .lock name must be accepted: {name:?}"
            );
        }
    }

    /// Every way a `lock_file` value can fail the simple-name contract — path
    /// traversal, absolute paths, embedded separators, a missing/wrong extension,
    /// NUL/control characters, the `:` drive/stream delimiter, and Windows reserved
    /// device names (bare and in their name-plus-extension aliasing form, including
    /// the superscript `COM`/`LPT` variants) — is rejected.
    #[test]
    fn unsafe_lock_file_names_are_rejected() {
        for name in [
            // Empty / traversal / absolute.
            "",
            "..",
            ".",
            "../escape.lock",
            "..\\escape.lock",
            "/tmp/escape.lock",
            "/etc/passwd.lock",
            "C:\\Windows\\escape.lock",
            "C:escape.lock",
            // Embedded path separators / drive-or-stream delimiter.
            "sub/dir.lock",
            "sub\\dir.lock",
            "stream:evil.lock",
            // Missing or wrong extension.
            "run-0000",
            "run-0000.txt",
            "run-0000.lock.bak",
            ".lock",
            // NUL / control characters.
            "run-0000\0.lock",
            "run-0000\t.lock",
            "run-0000\n.lock",
            // Windows reserved device names, bare and with an added extension chain.
            "CON.lock",
            "con.lock",
            "PRN.lock",
            "AUX.lock",
            "NUL.lock",
            "NUL.tar.gz.lock",
            "COM1.lock",
            "com9.lock",
            "LPT1.lock",
            "lpt9.lock",
            // Latin-1 superscript device-name aliases (still reserved).
            "COM\u{b9}.lock",
            "COM\u{b2}.lock",
            "COM\u{b3}.lock",
            "LPT\u{b9}.lock",
            "LPT\u{b2}.lock",
            "LPT\u{b3}.lock",
        ] {
            assert!(
                !is_simple_lock_file_name(name),
                "an unsafe lock_file value must be rejected: {name:?}"
            );
        }
    }

    /// A record whose `lock_file` is not a simple in-directory name — a `..`
    /// traversal, an absolute path, or a Windows reserved device name — is a corrupt
    /// entry: the scan skips it (never joining the value onto the registry directory
    /// to probe a file outside it) while a well-formed sibling entry is still scanned
    /// and returned. Proves the guard both defends the directory boundary and does not
    /// abort the whole scan over one bad record.
    #[test]
    fn entries_skip_unsafe_lock_files_without_aborting_the_scan() {
        let dir = scratch("unsafe-lock");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        write_record(&dir, "escaper-rel", "escaper-rel", "../escape.lock");
        write_record(&dir, "escaper-abs", "escaper-abs", absolute_escape());
        write_record(&dir, "device", "device", "NUL.tar.gz.lock");

        // A well-formed live entry alongside the corrupt ones.
        let good = registry
            .register_plain("good", None, SystemTime::now())
            .expect("register the good run");

        let entries = registry.entries().expect("scan");
        assert_eq!(
            entries.len(),
            1,
            "every unsafe entry is skipped and only the well-formed one survives"
        );
        assert_eq!(entries[0].record.run_id, "good");
        assert_eq!(entries[0].health, Health::Live);

        good.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// `is_valid_rfc3339_millis_utc` accepts every value the formatter it mirrors can
    /// actually produce (the positive case a corrupt-record guard must not
    /// accidentally reject) and rejects the shapes a hand-edited or truncated record
    /// could plausibly carry instead.
    #[test]
    fn started_at_validator_accepts_the_formatter_output_and_rejects_malformed_values() {
        for secs in [0u64, 1, 59, 3599, 86_399, 1_700_000_000] {
            for millis in [0u64, 5, 500, 999] {
                let formatted = events::format_rfc3339_utc(
                    UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis),
                );
                assert!(
                    is_valid_rfc3339_millis_utc(&formatted),
                    "the formatter's own output must validate: {formatted:?}"
                );
            }
        }

        for bad in [
            "",
            "not-a-timestamp",
            "2026-07-22T00:00:00Z",       // missing millisecond field
            "2026-07-22 00:00:00.000Z",   // space instead of `T`
            "2026-07-22T00:00:00.000",    // missing trailing `Z`
            "2026-13-01T00:00:00.000Z",   // month out of range
            "2026-07-32T00:00:00.000Z",   // day out of range
            "2026-07-22T24:00:00.000Z",   // hour out of range
            "2026-07-22T00:60:00.000Z",   // minute out of range
            "2026-07-22T00:00:60.000Z",   // second out of range
            "2026-07-22T00:00:00.000Z\0", // trailing NUL
            "20260722T000000.000Z",       // no separators at all
            "2026-02-31T00:00:00.000Z",   // February never has 31 days
            "2026-02-30T00:00:00.000Z",   // February never has 30 days
            "2026-02-29T00:00:00.000Z",   // 2026 is not a leap year
            "2100-02-29T00:00:00.000Z",   // century not divisible by 400: not a leap year
            "2026-04-31T00:00:00.000Z",   // April is a 30-day month
            "2026-06-31T00:00:00.000Z",   // June is a 30-day month
            "2026-09-31T00:00:00.000Z",   // September is a 30-day month
            "2026-11-31T00:00:00.000Z",   // November is a 30-day month
        ] {
            assert!(
                !is_valid_rfc3339_millis_utc(bad),
                "a malformed started_at value must be rejected: {bad:?}"
            );
        }

        // Calendar-valid edge cases that must still be accepted: leap-year February 29
        // (both the ordinary `% 4 == 0` rule and the `% 400 == 0` century exception),
        // and the last day of every 30/31-day month.
        for good in [
            "2024-02-29T00:00:00.000Z", // ordinary leap year (divisible by 4, not by 100)
            "2000-02-29T00:00:00.000Z", // century leap year (divisible by 400)
            "2026-02-28T00:00:00.000Z", // last day of February in a non-leap year
            "2026-04-30T00:00:00.000Z", // last day of a 30-day month
            "2026-01-31T00:00:00.000Z", // last day of a 31-day month
            "2026-12-31T00:00:00.000Z", // last day of the year
        ] {
            assert!(
                is_valid_rfc3339_millis_utc(good),
                "a calendar-valid started_at value must be accepted: {good:?}"
            );
        }
    }

    /// A record whose `started_at` is malformed (not the runner's own
    /// [`events::format_rfc3339_utc`] shape) is corrupt-record noise: the scan skips
    /// it — never listing or sorting a fabricated timestamp as if it were real —
    /// while a well-formed sibling entry is still scanned and returned. Mirrors
    /// `entries_skip_unsafe_lock_files_without_aborting_the_scan`'s degradation
    /// proof for the `started_at` field.
    #[test]
    fn entries_skip_malformed_started_at_without_aborting_the_scan() {
        let dir = scratch("bad-started-at");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        write_record_with_started_at(&dir, "garbage", "garbage", "not-a-timestamp");
        write_record_with_started_at(&dir, "truncated", "truncated", "2026-07-22T00:00:00Z");

        let good = registry
            .register_plain("good", None, SystemTime::now())
            .expect("register the good run");

        let entries = registry.entries().expect("scan");
        assert_eq!(
            entries.len(),
            1,
            "every malformed-started_at entry is skipped and only the well-formed one survives"
        );
        assert_eq!(entries[0].record.run_id, "good");

        good.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Unix: a lock file that is a *symlink* is refused at open time (`O_NOFOLLOW`),
    /// even though its name passes the simple-name check — so a record pointing a
    /// valid-looking lock name at a symlink still shows up in the scan (the record
    /// itself is well-formed), but classifies as `Unprobed`: the probe error must
    /// never let the link be followed onto an off-target file, must never be
    /// misreported as a confirmed-dead `Stale` verdict the probe never reached, and
    /// must never abort the whole scan either.
    #[cfg(unix)]
    #[test]
    fn symlink_lock_target_is_refused_at_open_time() {
        use std::os::unix::fs::symlink;

        let dir = scratch("symlink-lock");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        // A decoy the symlink would redirect the probe onto, and a symlink named like
        // a valid lock file pointing at it.
        let decoy = dir.join("decoy-target");
        fs::write(&decoy, b"decoy").expect("write the decoy target");
        let link = dir.join("run-symlink-0000.lock");
        symlink(&decoy, &link).expect("create the symlink lock file");

        // The name itself is a valid simple `.lock` name.
        assert!(is_simple_lock_file_name("run-symlink-0000.lock"));

        write_record(&dir, "run-symlink-0000", "linked", "run-symlink-0000.lock");

        // The open refuses to follow the symlink, so the probe errors — the entry is
        // still returned (its record is well-formed) but classifies `Unprobed`
        // rather than ever being reported `Live` off a link it never actually
        // locked, or `Stale` (a confirmed-dead claim the probe never established).
        let entries = registry.entries().expect("scan");
        let linked = entries
            .iter()
            .find(|entry| entry.record.run_id == "linked")
            .expect("a probe-failed entry is still returned, not dropped");
        assert_eq!(
            linked.health,
            Health::Unprobed,
            "an unprobeable lock file (symlink) must classify Unprobed, not abort the scan"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The regression this task exists for: a lock file that points at a
    /// **directory** rather than a regular file makes the liveness probe's
    /// write-open fail with a semantic error (`EISDIR` on Unix, an
    /// access/"is a directory"-shaped error on Windows) for *any* user, including
    /// root — unlike `chmod 0o000` (see [K-014] in the task's KB section), which a
    /// privileged or `CAP_DAC_OVERRIDE` CI runner simply ignores, making that
    /// approach a false-green trap. `entries()` must not abort the whole scan over
    /// this one unprobeable record: the healthy sibling stays `Live`, and the
    /// broken one classifies `Unprobed` rather than disappearing, being
    /// misreported as the confirmed-dead `Stale` (the T-206 fix), or taking the
    /// scan down with it — the exact misrouting bug T-007 fixed by returning
    /// (rather than dropping/aborting on) a probe-failed record in the first place
    /// (a stale/broken record no longer fails `inspect`/`cancel`/`kill` routing to a
    /// *different*, healthy run_id).
    #[test]
    fn entries_classifies_an_unprobeable_lock_directory_as_unprobed_without_aborting_the_scan() {
        let dir = scratch("dir-lock");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        // A record whose `lock_file` name is well-formed but resolves to a directory,
        // not a file: `OpenOptions::read(true).write(true).open(dir)` fails with a
        // semantic "is a directory" error on every platform and for every user.
        let broken_lock_dir = dir.join("broken.lock");
        fs::create_dir(&broken_lock_dir).expect("create the directory the lock name resolves to");
        write_record(&dir, "broken", "broken", "broken.lock");

        // A well-formed, live sibling entry alongside the unprobeable one.
        let good = registry
            .register_plain("good", None, SystemTime::now())
            .expect("register the good run");

        let entries = registry.entries().expect("scan must not fail");
        assert_eq!(
            entries.len(),
            2,
            "both the healthy and the unprobeable entry are returned"
        );

        let good_entry = entries
            .iter()
            .find(|entry| entry.record.run_id == "good")
            .expect("the healthy entry is present");
        assert_eq!(
            good_entry.health,
            Health::Live,
            "a healthy sibling must stay Live and not be lost to the neighboring probe error"
        );

        let broken_entry = entries
            .iter()
            .find(|entry| entry.record.run_id == "broken")
            .expect("the unprobeable entry is present, not dropped");
        assert_eq!(
            broken_entry.health,
            Health::Unprobed,
            "a record whose lock probe cannot even open must classify Unprobed, never the confirmed-dead Stale"
        );

        good.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Prune reaps a confirmed-stale **orphan**: a record whose lock file is already
    /// gone (`probe_for_prune` opens it and gets `NotFound` — stale by definition, a
    /// successful probe, not an error). The dangling `.json` is deleted; there is no
    /// lock file left to delete.
    #[test]
    fn prune_reaps_a_confirmed_stale_orphan_record() {
        let dir = scratch("prune-orphan");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        // A record pointing at a well-formed lock name that does not exist on disk.
        write_record(&dir, "orphan", "orphan", "orphan.lock");
        let record_path = dir.join("orphan.json");
        assert!(record_path.exists(), "the orphan record starts on disk");

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "an orphaned stale record is reaped"
        );
        assert!(!record_path.exists(), "the orphaned record file is deleted");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Prune reaps a confirmed-stale entry whose runner died abruptly (the released
    /// lock is taken by the probe, so both files are deleted) — and a second prune over
    /// the now-clean registry is a no-op, not an error.
    #[test]
    fn prune_reaps_a_stale_entry_with_a_released_lock_and_is_idempotent() {
        let dir = scratch("prune-released");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let registration = registry
            .register_plain("victim", None, SystemTime::now())
            .expect("register run");
        let record_path = registration.record_path().to_owned();
        let lock_path = registration.lock_path().to_owned();

        // Abrupt death: release the lock, leave both files behind.
        registration.simulate_abrupt_death();
        assert!(
            record_path.exists() && lock_path.exists(),
            "the abrupt-death fixture leaves both files on disk"
        );

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "the confirmed-stale entry is reaped"
        );
        assert!(
            !record_path.exists() && !lock_path.exists(),
            "both files of a reaped entry are deleted"
        );

        // Nothing left to prune: a repeat pass reaps nothing and does not error.
        assert_eq!(
            registry.prune().expect("a second prune must not fail"),
            PruneOutcome::default(),
            "pruning an already-clean registry is a no-op"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A live entry is **never** reaped, even sitting right beside a confirmed-stale
    /// one: the live runner still holds its lock, so the probe reports it live and
    /// prune leaves its files alone while reaping the dead sibling. Modelled on
    /// [`entries_classifies_an_unprobeable_lock_directory_as_unprobed_without_aborting_the_scan`].
    #[test]
    fn prune_never_reaps_a_live_entry() {
        let dir = scratch("prune-live");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let now = SystemTime::now();

        let live = registry
            .register_plain("alive", None, now)
            .expect("register the live run");
        let doomed = registry
            .register_plain("dead", None, now)
            .expect("register the doomed run");
        let live_record = live.record_path().to_owned();
        let live_lock = live.lock_path().to_owned();
        let dead_record = doomed.record_path().to_owned();
        let dead_lock = doomed.lock_path().to_owned();

        // Only the second runner dies abruptly; the first keeps holding its lock.
        doomed.simulate_abrupt_death();

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 1,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "exactly the stale entry is reaped and the live one is counted, not touched"
        );
        assert!(
            live_record.exists() && live_lock.exists(),
            "a live entry's files must survive prune untouched"
        );
        assert!(
            !dead_record.exists() && !dead_lock.exists(),
            "the stale sibling's files are reaped"
        );

        // The survivor still scans as the live run.
        let entries = registry.entries().expect("scan");
        assert_eq!(entries.len(), 1, "only the live entry remains");
        assert_eq!(entries[0].record.run_id, "alive");
        assert_eq!(entries[0].health, Health::Live);

        live.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// A record whose lock probe **fails** (here the lock name resolves to a
    /// *directory*, so the write-open fails with a semantic EISDIR/access error for
    /// any user — the confirmed cross-platform trick from [K-014], never `chmod
    /// 0o000`) is **not** reaped: liveness is unknown, not confirmed stale, so prune
    /// leaves it in place on every pass. One unprobeable entry never aborts the reap
    /// of a healthy stale sibling either.
    #[test]
    fn prune_leaves_an_unprobeable_entry_in_place() {
        let dir = scratch("prune-unprobeable");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        // A well-formed record whose `lock_file` name resolves to a directory: the
        // probe's write-open fails with a semantic error, so `probe_for_prune` returns
        // `Err` — the entry must be kept, not deleted.
        let broken_lock_dir = dir.join("broken.lock");
        fs::create_dir(&broken_lock_dir).expect("create the directory the lock name resolves to");
        write_record(&dir, "broken", "broken", "broken.lock");

        // A confirmed-stale orphan alongside it, which must still be reaped despite the
        // unprobeable neighbor.
        write_record(&dir, "orphan", "orphan", "orphan.lock");

        let outcome = registry
            .prune()
            .expect("prune must not fail on an unprobeable entry");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 1,
                orphaned_locks: 0,
            },
            "the unprobeable entry is kept and the stale sibling is still reaped"
        );
        assert!(
            dir.join("broken.json").exists(),
            "an unprobeable record is never reaped"
        );
        assert!(
            broken_lock_dir.exists(),
            "the unprobeable entry's lock target is left alone"
        );
        assert!(
            !dir.join("orphan.json").exists(),
            "a healthy stale sibling is still reaped past the unprobeable one"
        );

        // Repeated prune keeps leaving the unprobeable entry — at any number of runs.
        assert_eq!(
            registry.prune().expect("a second prune must not fail"),
            PruneOutcome {
                pruned: 0,
                live: 0,
                unprobed: 1,
                orphaned_locks: 0,
            },
            "the unprobeable entry is still kept on a repeat pass"
        );
        assert!(
            dir.join("broken.json").exists(),
            "the unprobeable record survives every prune"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The orphan-lock counterpart to `prune_reaps_a_confirmed_stale_orphan_record`:
    /// a lone `.lock` file with **no `.json` sibling at all** — invisible to `scan()`
    /// and so unreachable by the paired-record pass no matter how long it sits there
    /// — is reaped by `prune`'s second, orphan-lock pass. An unlocked file confirms
    /// stale exactly as `probe_for_prune` documents. The fixture is backdated past
    /// [`ORPHAN_LOCK_MIN_AGE`] ([R-01]) — a fresh, unlocked lock file must *not* be
    /// treated as a candidate at all, since that is exactly the shape of a
    /// legitimate reservation's brief pre-lock window; see
    /// `prune_never_reaps_a_fresh_unlocked_orphaned_lock_file` below for the
    /// *un*-backdated case.
    #[test]
    fn prune_reaps_a_lone_orphaned_lock_file() {
        let dir = scratch("prune-orphan-lock");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let lock_path = dir.join("orphan.lock");
        fs::write(&lock_path, b"").expect("write the orphaned lock file");
        backdate(&lock_path, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 0,
                live: 0,
                unprobed: 0,
                orphaned_locks: 1,
            },
            "a lone, unlocked .lock file with no .json sibling is reaped as an orphan"
        );
        assert!(!lock_path.exists(), "the orphaned lock file is deleted");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A `.lock` file **held by a live holder** is never reaped, orphan or not —
    /// the same "Live ⇒ never touch" rule the paired-record pass follows. Backdated
    /// past [`ORPHAN_LOCK_MIN_AGE`] so this exercises the "old enough, and live" path
    /// rather than being excluded by the age floor before it is ever probed.
    #[test]
    fn prune_never_reaps_a_live_orphaned_lock_file() {
        let dir = scratch("prune-orphan-live");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let lock_path = dir.join("orphan.lock");
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .expect("create the orphaned lock file");
        assert!(
            platform::try_lock_exclusive(&held).expect("take the lock"),
            "a fresh file must not already be locked"
        );
        backdate(&lock_path, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 0,
                live: 1,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "a lock held by a live holder must never be reaped, orphan or not"
        );
        assert!(
            lock_path.exists(),
            "the live-held orphaned lock file survives prune"
        );

        drop(held);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An orphaned `.lock` whose probe **fails** — here the name resolves to a
    /// directory rather than a regular file, the same cross-platform [K-014] trick
    /// used for the paired-record probe-error tests — is left in place, not deleted:
    /// liveness is unknown, not confirmed stale. Backdated past
    /// [`ORPHAN_LOCK_MIN_AGE`] so this exercises the "old enough, but unprobeable"
    /// path rather than being excluded by the age floor before it is ever probed.
    #[test]
    fn prune_leaves_an_unprobeable_orphaned_lock_file_in_place() {
        let dir = scratch("prune-orphan-unprobeable");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let broken = dir.join("broken.lock");
        fs::create_dir(&broken).expect("create the directory the lock name resolves to");
        backdate(&broken, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 0,
                live: 0,
                unprobed: 1,
                orphaned_locks: 0,
            },
            "an unprobeable orphaned lock is kept in place, not deleted"
        );
        assert!(
            broken.exists(),
            "the unprobeable orphaned lock's target is left alone"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// [R-01] regression: a `.lock` file with no `.json` sibling that is younger than
    /// [`ORPHAN_LOCK_MIN_AGE`] must not be touched by `prune`'s orphan-lock pass at
    /// all, even though it is unlocked and would otherwise read as a textbook
    /// "confirmed stale, no live holder" orphan. This is exactly the shape
    /// `Registry::reserve_entry` produces for the brief window between `create_new`
    /// and taking its own lock — before the age floor, a concurrent `prune` racing
    /// that window could reap a legitimate, in-flight reservation's lock file out
    /// from under it.
    #[test]
    fn prune_never_reaps_a_fresh_unlocked_orphaned_lock_file() {
        let dir = scratch("prune-orphan-fresh");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let lock_path = dir.join("orphan.lock");
        fs::write(&lock_path, b"").expect("write the fresh orphaned lock file");

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome::default(),
            "a lock file younger than ORPHAN_LOCK_MIN_AGE must not even be probed, \
             let alone reaped"
        );
        assert!(
            lock_path.exists(),
            "a fresh, not-yet-aged orphan candidate survives prune"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// [R-01] regression, the `reserve_entry` side of the fix: `platform::lock_path_still_matches`
    /// must confirm identity between the still-open lock handle and the current
    /// contents of its path — not merely that *some* file exists there. A file
    /// removed out from under the held lock (the shape a concurrent `prune` leaves
    /// behind after reaping the same path first, see the race in [R-01]'s finding)
    /// must read back as a mismatch, not a false positive.
    #[test]
    fn lock_path_still_matches_detects_a_reaped_lock_file() {
        let dir = scratch("reserve-identity");
        fs::create_dir_all(&dir).expect("create scratch dir");

        let lock_path = dir.join("stem.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .expect("create the lock file");
        assert!(
            platform::try_lock_exclusive(&lock).expect("take the lock"),
            "a fresh file must not already be locked"
        );

        assert!(
            platform::lock_path_still_matches(&lock, &lock_path)
                .expect("identity check must not fail while the file still exists"),
            "the path still resolves to the exact file this handle holds"
        );

        // Simulate a concurrent `prune` winning the race: it deletes the file while
        // holding its own (now-released) lock, exactly as `Registry::prune`'s orphan
        // pass does in its `Reapable` arm.
        fs::remove_file(&lock_path).expect("simulate a concurrent reap");
        assert!(
            !platform::lock_path_still_matches(&lock, &lock_path)
                .expect("a missing path is a definitive mismatch, not an error"),
            "a path whose file has been deleted out from under the held lock must \
             never read back as still matching"
        );

        drop(lock);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Pruning an empty registry — and a never-created one — is a no-op that returns
    /// all-zero counts and never errors, and pruning a missing directory does not
    /// create it (prune, like `list`, opens read-only).
    #[test]
    fn prune_over_a_clean_or_missing_registry_is_a_no_op() {
        let dir = scratch("prune-clean");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        assert_eq!(
            registry.prune().expect("prune an empty registry"),
            PruneOutcome::default(),
            "an empty registry has nothing to prune"
        );
        let _ = fs::remove_dir_all(&dir);

        let missing = scratch("prune-missing");
        assert!(!missing.exists(), "the scratch fixture starts absent");
        let read_only = Registry::open_read_only_in(missing.clone());
        assert_eq!(
            read_only.prune().expect("prune a missing registry"),
            PruneOutcome::default(),
            "a missing registry reads back as empty and prunes nothing"
        );
        assert!(
            !missing.exists(),
            "pruning a missing registry must not create its directory"
        );
    }

    /// Build the mixed fixture `preview_prune`'s equivalence/non-mutation tests
    /// share: a live pair (kept alive by the returned [`Registration`], which the
    /// caller must hold for the test's duration), a confirmed-stale pair (released
    /// lock, both files left behind), an unprobeable pair (its lock name resolves to
    /// a directory, the [K-014] trick), and a confirmed-stale orphaned `.lock` file
    /// with no `.json` sibling, backdated past [`ORPHAN_LOCK_MIN_AGE`] so it reads as
    /// a genuine orphan rather than a fresh, not-yet-locked reservation.
    fn mixed_prune_fixture(dir: &Path, registry: &Registry) -> Registration {
        let live = registry
            .register_plain("alive", None, SystemTime::now())
            .expect("register the live run");
        let doomed = registry
            .register_plain("dead", None, SystemTime::now())
            .expect("register the doomed run");
        doomed.simulate_abrupt_death();

        let broken_lock_dir = dir.join("broken.lock");
        fs::create_dir(&broken_lock_dir)
            .expect("create the directory the unprobeable lock name resolves to");
        write_record(dir, "broken", "broken", "broken.lock");

        let orphan_lock = dir.join("orphan.lock");
        fs::write(&orphan_lock, b"").expect("write the orphaned lock file");
        backdate(&orphan_lock, ORPHAN_LOCK_MIN_AGE + Duration::from_secs(1));

        live
    }

    /// T-199, the heart of `prune --dry-run`'s safety claim: `preview_prune`'s
    /// aggregate tally must exactly match what a following, real `prune` pass over
    /// the identical, untouched registry state reports. Run over a fixture that
    /// exercises every classification at once (live, confirmed-stale, unprobeable,
    /// orphaned lock) — a match on this mix is a much stronger claim than a match on
    /// any single case.
    #[test]
    fn preview_prune_matches_a_real_prune_over_identical_state() {
        let dir = scratch("preview-equivalence");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let live = mixed_prune_fixture(&dir, &registry);

        let expected = PruneOutcome {
            pruned: 1,
            live: 1,
            unprobed: 1,
            orphaned_locks: 1,
        };

        let preview = registry
            .preview_prune()
            .expect("preview_prune must not fail");
        assert_eq!(
            preview.outcome, expected,
            "sanity: the mixed fixture must exercise every classification"
        );

        // The preview must not have touched anything: a real prune run right after it
        // reaps exactly the same tally from the exact same on-disk state.
        let real_outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            preview.outcome, real_outcome,
            "a dry-run preview's aggregate tally must equal a real prune's tally over \
             the identical registry state"
        );

        live.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// One [`snapshot_dir`] entry: name, whether it is a directory, its byte length,
    /// a regular non-`.lock` file's exact byte contents (`None` for a directory or a
    /// `.lock` file — see [`snapshot_dir`]'s docs), and its mtime.
    type DirSnapshotEntry = (String, bool, u64, Option<Vec<u8>>, SystemTime);

    /// A snapshot of every entry directly inside `dir` — see [`DirSnapshotEntry`] for
    /// the fields — sorted for a deterministic comparison. Used to confirm
    /// `preview_prune` mutates nothing: a snapshot taken before and after a preview
    /// pass must compare equal.
    ///
    /// A `.lock` file's content is deliberately **not** read here (`None`, like a
    /// directory): [`platform::try_lock_exclusive`] on Windows takes a whole-file
    /// **mandatory** byte-range lock via `LockFileEx` (unlike POSIX `flock`, which
    /// stays purely advisory and never blocks a plain read), so `fs::read`-ing a
    /// still-live entry's lock file — e.g. this fixture's held `alive` registration —
    /// would spuriously fail with a sharing violation, which is a Windows locking
    /// artifact, not evidence `preview_prune` touched anything. Every `.lock` file in
    /// this codebase is (and only is ever) an empty marker with no meaningful
    /// content, so its length is enough to prove nothing was written to it; its
    /// mtime and the directory listing itself already prove nothing was deleted,
    /// created, or renamed.
    fn snapshot_dir(dir: &Path) -> Vec<DirSnapshotEntry> {
        let mut entries: Vec<DirSnapshotEntry> = fs::read_dir(dir)
            .expect("read the scratch registry directory")
            .filter_map(Result::ok)
            .map(|dir_entry| {
                let name = dir_entry.file_name().to_string_lossy().into_owned();
                let path = dir_entry.path();
                let metadata = dir_entry.metadata().expect("read fixture metadata");
                let is_dir = metadata.is_dir();
                let is_lock = path.extension().and_then(|ext| ext.to_str()) == Some("lock");
                let contents = if is_dir || is_lock {
                    None
                } else {
                    Some(fs::read(&path).expect("read a fixture file's contents"))
                };
                let modified = metadata.modified().expect("read fixture mtime");
                (name, is_dir, metadata.len(), contents, modified)
            })
            .collect();
        entries.sort_by(|(a, ..), (b, ..)| a.cmp(b));
        entries
    }

    /// T-199: `preview_prune` must never delete, create, or otherwise modify
    /// anything — the same mixed fixture as
    /// `preview_prune_matches_a_real_prune_over_identical_state`, but here the proof
    /// is a byte-for-byte directory snapshot taken before and after the preview pass,
    /// rather than only the aggregate counts.
    #[test]
    fn preview_prune_leaves_the_registry_byte_for_byte_untouched() {
        let dir = scratch("preview-untouched");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let live = mixed_prune_fixture(&dir, &registry);

        let before = snapshot_dir(&dir);
        let preview = registry
            .preview_prune()
            .expect("preview_prune must not fail");
        let after = snapshot_dir(&dir);

        assert_eq!(
            before, after,
            "preview_prune must leave the registry directory byte-for-byte untouched"
        );
        assert_eq!(
            preview.outcome,
            PruneOutcome {
                pruned: 1,
                live: 1,
                unprobed: 1,
                orphaned_locks: 1,
            },
            "sanity: the mixed fixture must exercise every classification"
        );

        live.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// `preview_prune`'s candidate list identifies exactly the two confirmed-stale
    /// entries the mixed fixture contains — a paired record by `run_id`/`started_at`,
    /// an orphaned lock by its file name — and none of the live or unprobeable ones.
    #[test]
    fn preview_prune_candidates_identify_the_confirmed_stale_entries() {
        let dir = scratch("preview-candidates");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let live = mixed_prune_fixture(&dir, &registry);

        let preview = registry
            .preview_prune()
            .expect("preview_prune must not fail");
        assert_eq!(
            preview.candidates.len(),
            2,
            "exactly the confirmed-stale pair and the orphaned lock are candidates: \
             {:?}",
            preview.candidates
        );
        assert!(
            preview.candidates.iter().any(|candidate| matches!(
                candidate,
                PruneCandidate::Entry { run_id, socket_dir, .. }
                    if run_id == "dead" && socket_dir.is_none()
            )),
            "the confirmed-stale paired entry is a candidate, and — having published no \
             endpoint — names no control socket to reap with it: {:?}",
            preview.candidates
        );
        assert!(
            preview.candidates.iter().any(|candidate| matches!(
                candidate,
                PruneCandidate::OrphanedLock { lock_file_name } if lock_file_name == "orphan.lock"
            )),
            "the orphaned lock file is a candidate: {:?}",
            preview.candidates
        );

        live.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// `preview_prune` over an empty or missing registry is a no-op that returns an
    /// all-zero tally and no candidates, exactly like `prune` — the dry-run
    /// counterpart to `prune_over_a_clean_or_missing_registry_is_a_no_op`.
    #[test]
    fn preview_prune_over_a_clean_or_missing_registry_is_a_no_op() {
        let dir = scratch("preview-clean");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        assert_eq!(
            registry.preview_prune().expect("preview an empty registry"),
            PrunePreview::default(),
            "an empty registry has nothing to preview"
        );
        let _ = fs::remove_dir_all(&dir);

        let missing = scratch("preview-missing");
        assert!(!missing.exists(), "the scratch fixture starts absent");
        let read_only = Registry::open_read_only_in(missing.clone());
        assert_eq!(
            read_only
                .preview_prune()
                .expect("preview a missing registry"),
            PrunePreview::default(),
            "a missing registry reads back as empty and previews nothing"
        );
        assert!(
            !missing.exists(),
            "previewing a missing registry must not create its directory"
        );
    }

    /// T-207, the shape guard on its own: an `endpoint` is a candidate for the socket
    /// reap **only** in the exact form `ControlServer::bind` publishes, and every
    /// other form — including the ones a corrupt or adversarial record could carry —
    /// yields nothing to delete. Exercised against explicit bases so the verdict does
    /// not depend on the host's `TMPDIR`, and covering the boundary cases a
    /// hand-rolled path validator is easy to get wrong on (see [K-030]): an empty
    /// value, a relative path, `..` segments anywhere, a NUL/control character, a
    /// deeper or shallower nesting, a near-miss directory name, and a directory
    /// outside the allowed bases entirely.
    #[cfg(unix)]
    #[test]
    fn control_socket_endpoints_are_accepted_only_in_the_published_shape() {
        let bases = [PathBuf::from("/tmp"), PathBuf::from("/var/tmp/scratch")];

        // The real thing: what `unique_token`'s pid-nanos-counter form actually looks
        // like, under either base.
        for (endpoint, expected) in [
            (
                "/tmp/pkc-12345-17a2b3c4d5e-0/c.sock",
                "/tmp/pkc-12345-17a2b3c4d5e-0",
            ),
            ("/var/tmp/scratch/pkc-1/c.sock", "/var/tmp/scratch/pkc-1"),
        ] {
            assert_eq!(
                platform::socket_dir_within(endpoint, &bases),
                Some(PathBuf::from(expected)),
                "the published endpoint shape must be recognized: {endpoint:?}"
            );
        }

        for endpoint in [
            // Empty / relative / not a path this project ever publishes.
            "",
            "c.sock",
            "tmp/pkc-1/c.sock",
            "pkc-1/c.sock",
            // Traversal, anywhere in the value — refused before anything resolves it.
            "/tmp/pkc-1/../pkc-2/c.sock",
            "/tmp/../tmp/pkc-1/c.sock",
            "/tmp/pkc-1/c.sock/..",
            // Normalization-equivalent spellings `Path::components()` would silently
            // erase: a `.` segment, a doubled separator, a trailing separator. None
            // of them is what `bind` publishes, so none is accepted here either.
            "/tmp/./pkc-1/c.sock",
            "/tmp//pkc-1/c.sock",
            "//tmp/pkc-1/c.sock",
            "/tmp/pkc-1/c.sock/",
            // NUL / control characters.
            "/tmp/pkc-1/c.sock\0",
            "/tmp/pkc-1\n/c.sock",
            // Wrong file name, or no file at all.
            "/tmp/pkc-1/other.sock",
            "/tmp/pkc-1/c.sock.bak",
            "/tmp/pkc-1/C.SOCK",
            "/tmp/pkc-1",
            "/tmp/pkc-1/",
            // Wrong directory name: near-miss prefix, wrong case, empty token, or a
            // token carrying characters `unique_token` never mints.
            "/tmp/notpkc-1/c.sock",
            "/tmp/PKC-1/c.sock",
            "/tmp/pkc-/c.sock",
            "/tmp/pkc-1 2/c.sock",
            "/tmp/pkc-1.2/c.sock",
            "/tmp/pkc-1:2/c.sock",
            // Right shape, wrong place: too deep, too shallow, or a base that is not
            // one a control server ever binds in.
            "/tmp/sub/pkc-1/c.sock",
            "/tmp/pkc-1/sub/c.sock",
            "/pkc-1/c.sock",
            "/etc/pkc-1/c.sock",
            "/var/tmp/pkc-1/c.sock",
            "/tmp/c.sock",
            // A Windows named pipe, which is not a filesystem path at all.
            r"\\.\pipe\processkit-cli-1234-abc-0",
        ] {
            assert_eq!(
                platform::socket_dir_within(endpoint, &bases),
                None,
                "an endpoint outside the published shape must yield nothing to delete: \
                 {endpoint:?}"
            );
        }
    }

    /// The anti-drift check between the transport that *publishes* an endpoint and the
    /// reaper that consumes it: an endpoint a **real** `ControlServer::bind` just
    /// produced must classify as a reap candidate, must name the very directory that
    /// bind created, and must actually be removable by the reaper — a real
    /// tokio-bound socket, not a hand-built fixture. If the socket's naming, its
    /// private directory's prefix, or the bases it is created in ever change on the
    /// control side, this fails loudly instead of the reap quietly going silent and
    /// the leak this task closes coming back.
    ///
    /// `#[tokio::test]`, not `#[test]`: `UnixListener::bind` needs a reactor (see
    /// [K-009]).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_freshly_bound_control_endpoint_is_recognized_and_reapable() {
        let server = crate::control::ControlServer::bind().expect("bind a control server");
        let endpoint = server.endpoint().to_string();

        let candidate = platform::control_socket_dir_to_reap(Some(&endpoint))
            .expect("a freshly published endpoint must classify as a reap candidate");
        assert_eq!(
            candidate.join(crate::control::SOCKET_FILE_NAME),
            PathBuf::from(&endpoint),
            "the classified directory must be the one the socket was bound in"
        );
        assert!(
            candidate.is_dir() && Path::new(&endpoint).exists(),
            "sanity: bind really created the directory and the socket"
        );

        // Reap it exactly as a confirmed-stale record's would be reaped. The socket is
        // still bound here — an abruptly-killed runner's is too, from the filesystem's
        // point of view — and it goes, along with its directory.
        platform::reap_control_socket_dir(&candidate);
        assert!(
            !Path::new(&endpoint).exists(),
            "a real published control socket is unlinked by the reaper"
        );
        assert!(
            !candidate.exists(),
            "its private directory goes with it, leaving nothing behind"
        );

        // The server's own clean-teardown Drop is best-effort and copes with the
        // files already being gone.
        drop(server);
    }

    /// T-207, the leak this task closes: reaping a confirmed-stale entry also removes
    /// the control socket that entry published and the private directory holding it —
    /// the other half of what an abruptly-killed runner strands on disk, which no
    /// pass ever cleaned up before.
    #[cfg(unix)]
    #[test]
    fn prune_reaps_the_control_socket_a_confirmed_stale_record_published() {
        let dir = scratch("prune-socket-reap");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let (socket_dir, endpoint) = socket_fixture("reap");

        let registration = registry
            .register_plain("victim", Some(&endpoint), SystemTime::now())
            .expect("register run");
        let record_path = registration.record_path().to_owned();
        let lock_path = registration.lock_path().to_owned();
        // Abrupt death: the lock is released, and every file — record, lock, and the
        // socket the clean-teardown `Drop` never got to remove — is left behind.
        registration.simulate_abrupt_death();
        assert!(
            Path::new(&endpoint).exists() && socket_dir.exists(),
            "the abrupt-death fixture leaves the control socket on disk"
        );

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "the confirmed-stale entry is reaped"
        );
        assert!(
            !record_path.exists() && !lock_path.exists(),
            "both registry files of the reaped entry are deleted"
        );
        assert!(
            !Path::new(&endpoint).exists(),
            "the control socket the reaped record published is deleted too"
        );
        assert!(
            !socket_dir.exists(),
            "the socket's private directory is reaped with it, not left behind empty"
        );

        let _ = fs::remove_dir_all(&socket_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A **live** run's control socket is never touched — the same guarantee the live
    /// entry's own files already have. The socket reap runs only inside the
    /// confirmed-stale arm, so a live runner keeps the transport its clients are
    /// still connecting to.
    #[cfg(unix)]
    #[test]
    fn prune_never_reaps_the_control_socket_of_a_live_entry() {
        let dir = scratch("prune-socket-live");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let (socket_dir, endpoint) = socket_fixture("live");

        let live = registry
            .register_plain("alive", Some(&endpoint), SystemTime::now())
            .expect("register the live run");

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 0,
                live: 1,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "the live entry is counted as kept, not reaped"
        );
        assert!(
            Path::new(&endpoint).exists() && socket_dir.exists(),
            "a live run's control socket and its directory must survive prune untouched"
        );

        live.remove();
        let _ = fs::remove_dir_all(&socket_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An entry whose liveness probe **fails** keeps its control socket too: liveness
    /// is unknown, not confirmed stale, so nothing about that entry is deleted — the
    /// socket included. The probe is forced to fail with the cross-platform
    /// lock-file-is-a-directory trick from [K-014], never `chmod 0o000`.
    #[cfg(unix)]
    #[test]
    fn prune_leaves_the_control_socket_of_an_unprobeable_entry_alone() {
        let dir = scratch("prune-socket-unprobeable");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let (socket_dir, endpoint) = socket_fixture("unpr");

        fs::create_dir(dir.join("broken.lock"))
            .expect("create the directory the lock name resolves to");
        write_record_with_endpoint(&dir, "broken", "broken", "broken.lock", Some(&endpoint));

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 0,
                live: 0,
                unprobed: 1,
                orphaned_locks: 0,
            },
            "an unprobeable entry is kept"
        );
        assert!(
            dir.join("broken.json").exists(),
            "an unprobeable record is never reaped"
        );
        assert!(
            Path::new(&endpoint).exists() && socket_dir.exists(),
            "an unprobeable entry's control socket is never reaped either"
        );

        let _ = fs::remove_dir_all(&socket_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The symlink attack the [K-024] `O_NOFOLLOW` discipline exists for, aimed at the
    /// socket directory instead of a lock file: a `pkc-…` name inside a base that is
    /// really a **symlink** to somewhere else entirely. The name passes the lexical
    /// shape check (it is exactly the published form), so only the open-time refusal
    /// stands between the reap and the link's target — and it holds: nothing behind
    /// the link is deleted, and the link itself is left in place rather than being
    /// followed. The record is still reaped, since the endpoint check gates only the
    /// extra socket deletion.
    #[cfg(unix)]
    #[test]
    fn prune_refuses_to_follow_a_symlinked_control_socket_directory() {
        use std::os::unix::fs::symlink;

        let dir = scratch("prune-socket-symlink");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        // What the link points at: a directory holding a socket named exactly like a
        // published one, plus an unrelated bystander file.
        let decoy = off_base_dir("dcy");
        let _ = fs::remove_dir_all(&decoy);
        fs::create_dir_all(&decoy).expect("create the decoy target directory");
        let decoy_socket = bind_socket_in(&decoy);
        let bystander = decoy.join("bystander");
        fs::write(&bystander, b"not yours to delete").expect("write the bystander file");

        // The endpoint: a perfectly-shaped `<base>/pkc-<token>/c.sock`, whose
        // directory component is a symlink onto the decoy.
        let link = socket_dir_path("link");
        let _ = fs::remove_dir_all(&link);
        symlink(&decoy, &link).expect("create the symlinked socket directory");
        let endpoint = link.join(crate::control::SOCKET_FILE_NAME);
        let endpoint = endpoint.to_str().expect("a UTF-8 endpoint");
        assert!(
            platform::control_socket_dir_to_reap(Some(endpoint)).is_some(),
            "sanity: the lexical shape check passes, so only the open-time refusal \
             can stop this reap"
        );

        write_record_with_endpoint(&dir, "linked", "linked", "linked.lock", Some(endpoint));

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "the record itself is still reaped — the endpoint check gates only the \
             socket deletion"
        );
        assert!(
            Path::new(&decoy_socket).exists(),
            "the symlink's target must not be followed: the socket behind it survives"
        );
        assert!(
            bystander.exists() && decoy.exists(),
            "nothing behind the symlink is deleted"
        );
        assert!(
            fs::symlink_metadata(&link).is_ok(),
            "the symlink itself is left in place, not resolved and reaped"
        );

        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&decoy);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Even inside a genuine, validated `pkc-…` directory, only a real **socket** is
    /// unlinked: a regular file planted under the socket's name is refused, and the
    /// directory holding it is then left alone too (rather than being emptied of
    /// whatever happens to sit there). The record is reaped as usual.
    #[cfg(unix)]
    #[test]
    fn prune_refuses_to_delete_an_endpoint_that_is_not_a_socket() {
        let dir = scratch("prune-socket-not-a-socket");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let socket_dir = socket_dir_path("file");
        let _ = fs::remove_dir_all(&socket_dir);
        fs::create_dir(&socket_dir).expect("create the private control-socket directory");
        let planted = socket_dir.join(crate::control::SOCKET_FILE_NAME);
        fs::write(&planted, b"a regular file, not a socket").expect("plant the decoy file");
        let endpoint = planted.to_str().expect("a UTF-8 endpoint").to_string();

        write_record_with_endpoint(&dir, "planted", "planted", "planted.lock", Some(&endpoint));

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "the record is reaped whatever its endpoint turned out to be"
        );
        assert!(
            planted.exists(),
            "a file that is not a socket is refused, never deleted"
        );
        assert!(
            socket_dir.exists(),
            "the directory still holding the refused file is left alone too"
        );

        let _ = fs::remove_dir_all(&socket_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A well-formed-looking endpoint **outside** the base directories a control
    /// server ever binds in deletes nothing at all: the record is reaped, and the
    /// directory it pointed at — socket, sibling file, and the directory itself —
    /// survives untouched. This is the property that keeps a corrupt or hand-edited
    /// record from steering the reap at an arbitrary path.
    #[cfg(unix)]
    #[test]
    fn prune_ignores_an_endpoint_outside_the_control_socket_bases() {
        let dir = scratch("prune-socket-outside");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        // `<temp>/<other>/pkc-1/c.sock`: the right *shape*, one level too deep to be a
        // directory `ControlServer::bind` created.
        let outside = off_base_dir("off");
        let _ = fs::remove_dir_all(&outside);
        let elsewhere = outside.join("pkc-1");
        fs::create_dir_all(&elsewhere).expect("create the off-base directory");
        let socket = bind_socket_in(&elsewhere);
        let bystander = elsewhere.join("bystander");
        fs::write(&bystander, b"not yours to delete").expect("write the bystander file");
        assert!(
            platform::control_socket_dir_to_reap(Some(&socket)).is_none(),
            "sanity: an endpoint outside the published bases is not a candidate"
        );

        write_record_with_endpoint(&dir, "offbase", "offbase", "offbase.lock", Some(&socket));

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "the record itself is reaped as usual"
        );
        assert!(
            Path::new(&socket).exists() && bystander.exists() && elsewhere.exists(),
            "nothing outside the published socket bases is deleted"
        );

        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `prune --dry-run`'s side of T-207: the preview names the socket directory a
    /// real reap would remove — and stays silent for an entry whose endpoint that
    /// reap would refuse — without removing either. The following real prune then
    /// does exactly what the preview said: one socket reaped, the refused one
    /// untouched.
    #[cfg(unix)]
    #[test]
    fn preview_prune_reports_the_control_socket_it_would_reap_and_removes_nothing() {
        let dir = scratch("preview-socket");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        // One confirmed-stale entry whose endpoint the reap accepts...
        let (socket_dir, endpoint) = socket_fixture("pvw");
        write_record_with_endpoint(
            &dir,
            "reapable",
            "reapable",
            "reapable.lock",
            Some(&endpoint),
        );

        // ...and one whose endpoint it refuses (right shape, wrong place).
        let outside = off_base_dir("pvw");
        let _ = fs::remove_dir_all(&outside);
        let elsewhere = outside.join("pkc-1");
        fs::create_dir_all(&elsewhere).expect("create the off-base directory");
        let refused = bind_socket_in(&elsewhere);
        write_record_with_endpoint(&dir, "refused", "refused", "refused.lock", Some(&refused));

        let preview = registry
            .preview_prune()
            .expect("preview_prune must not fail");
        assert_eq!(
            preview.outcome,
            PruneOutcome {
                pruned: 2,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "both records are confirmed-stale candidates"
        );
        assert!(
            preview.candidates.iter().any(|candidate| matches!(
                candidate,
                PruneCandidate::Entry { run_id, socket_dir: Some(reported), .. }
                    if run_id == "reapable" && Path::new(reported) == socket_dir
            )),
            "the preview names the socket directory a real reap would remove: {:?}",
            preview.candidates
        );
        assert!(
            preview.candidates.iter().any(|candidate| matches!(
                candidate,
                PruneCandidate::Entry { run_id, socket_dir, .. }
                    if run_id == "refused" && socket_dir.is_none()
            )),
            "the preview names no socket for an endpoint the reap would refuse: {:?}",
            preview.candidates
        );
        assert!(
            Path::new(&endpoint).exists() && Path::new(&refused).exists(),
            "a preview must not delete either socket"
        );

        // The real pass now does exactly what the preview described.
        registry.prune().expect("prune must not fail");
        assert!(
            !Path::new(&endpoint).exists() && !socket_dir.exists(),
            "the previewed socket directory is what the real reap removes"
        );
        assert!(
            Path::new(&refused).exists(),
            "the endpoint the preview reported no candidate for is still not deleted"
        );

        let _ = fs::remove_dir_all(&socket_dir);
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The Windows side of T-207: a named-pipe endpoint is never a socket candidate —
    /// the pipe lives in the kernel object namespace and disappears with its creator,
    /// so there is no filesystem leftover to classify. The preview reports no socket
    /// directory and the reap deletes exactly the two registry files it always did.
    #[cfg(windows)]
    #[test]
    fn a_named_pipe_endpoint_is_never_a_control_socket_candidate() {
        let dir = scratch("prune-socket-pipe");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let endpoint = r"\\.\pipe\processkit-cli-1234-17a2b3c4d5e-0";
        write_record_with_endpoint(&dir, "piped", "piped", "piped.lock", Some(endpoint));

        let preview = registry
            .preview_prune()
            .expect("preview_prune must not fail");
        assert!(
            preview.candidates.iter().any(|candidate| matches!(
                candidate,
                PruneCandidate::Entry { run_id, socket_dir, .. }
                    if run_id == "piped" && socket_dir.is_none()
            )),
            "a named-pipe endpoint names no directory to reap: {:?}",
            preview.candidates
        );

        let outcome = registry.prune().expect("prune must not fail");
        assert_eq!(
            outcome,
            PruneOutcome {
                pruned: 1,
                live: 0,
                unprobed: 0,
                orphaned_locks: 0,
            },
            "the record is reaped exactly as it was before the socket reap existed"
        );
        assert!(
            !dir.join("piped.json").exists(),
            "the confirmed-stale record is gone"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The `wait` read path end to end on the ordinary lifecycle: a registered run
    /// probes as [`RunStatus::Live`] while its runner holds the lock, and as
    /// [`RunStatus::Finished`] the moment its clean exit removes the entry.
    #[test]
    fn probe_run_tracks_a_run_from_live_to_finished() {
        let dir = scratch("probe-run-lifecycle");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register_plain("waited", None, SystemTime::now())
            .expect("register run");

        assert_eq!(
            registry.probe_run("waited").expect("probe"),
            RunStatus::Live,
            "a run whose runner holds its lock is live"
        );

        registration.remove();
        assert_eq!(
            registry.probe_run("waited").expect("probe"),
            RunStatus::Finished,
            "a clean exit removes the entry, so the run reads as finished"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The documented conflation: a `run_id` with no record at all is
    /// [`RunStatus::Finished`], because a run that exits cleanly deletes its own
    /// entry — "never registered" and "already finished and cleaned up" are the same
    /// observation, and the registry keeps no history that could separate them.
    #[test]
    fn probe_run_reports_an_unknown_run_id_as_finished() {
        let dir = scratch("probe-run-unknown");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        assert_eq!(
            registry.probe_run("never-registered").expect("probe"),
            RunStatus::Finished,
            "an id nobody registered is indistinguishable from one already cleaned up"
        );

        // The same answer with an unrelated live run in the registry: matching is by
        // `run_id`, so another run's liveness never leaks into this one's verdict.
        let other = registry
            .register_plain("someone-else", None, SystemTime::now())
            .expect("register an unrelated run");
        assert_eq!(
            registry.probe_run("never-registered").expect("probe"),
            RunStatus::Finished,
            "an unrelated live run must not make an unknown id look live"
        );

        other.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// An abruptly-killed runner leaves both files on disk, yet the run is over: the
    /// released lock makes the entry confirmed-stale, so `wait` stops waiting. The
    /// files are left exactly where they are — `probe_run` is a query, not a reaper
    /// (that is `prune`'s job).
    #[test]
    fn probe_run_reports_a_stale_leftover_as_finished_without_reaping_it() {
        let dir = scratch("probe-run-stale");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register_plain("crashed", None, SystemTime::now())
            .expect("register run");
        let record_path = registration.record_path().to_owned();
        let lock_path = registration.lock_path().to_owned();

        registration.simulate_abrupt_death();

        assert_eq!(
            registry.probe_run("crashed").expect("probe"),
            RunStatus::Finished,
            "a leftover entry whose lock is released means the run is over"
        );
        assert!(
            record_path.exists() && lock_path.exists(),
            "a read-only probe must leave the stale entry's files on disk"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Two live runs under one `run_id` (the registry never enforces uniqueness) make
    /// the id name no single run, so the verdict is [`RunStatus::Ambiguous`] with the
    /// live count — never a silent pick of whichever entry the scan yielded first.
    ///
    /// One of the duplicates deliberately publishes **no endpoint**: liveness is
    /// counted by the identity predicate (`run_id`) alone, before any secondary
    /// attribute, which is exactly the undercount [K-016] found in `src/control.rs`
    /// when the two were folded into one filter pass. Here the point is even sharper
    /// than there — `wait` never needs an endpoint at all, so an endpoint-less live
    /// run is an entirely ordinary run to wait for, not a lesser one.
    #[test]
    fn probe_run_reports_ambiguity_counting_even_an_endpoint_less_duplicate() {
        let dir = scratch("probe-run-ambiguous");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let now = SystemTime::now();

        let with_endpoint = registry
            .register_plain("dup", Some("endpoint-a"), now)
            .expect("register the first duplicate");
        let without_endpoint = registry
            .register_plain("dup", None, now)
            .expect("register the second duplicate");

        assert_eq!(
            registry.probe_run("dup").expect("probe"),
            RunStatus::Ambiguous { live: 2 },
            "two live runs under one id is an ambiguity, counted by run_id alone"
        );

        // Once one of them ends, the id names a single run again and the wait can
        // resume normally — the ambiguity is a property of the moment, not a curse
        // on the id.
        without_endpoint.remove();
        assert_eq!(
            registry.probe_run("dup").expect("probe"),
            RunStatus::Live,
            "with one duplicate gone the surviving run is unambiguously live"
        );

        with_endpoint.remove();
        let _ = fs::remove_dir_all(&dir);
    }

    /// The [K-024] property this method exists for: a matching record whose liveness
    /// **cannot be probed** (its lock name resolves to a *directory*, so the
    /// write-open fails with a semantic error for any user — the cross-platform trick
    /// from [K-014], never `chmod 0o000`) must read as [`RunStatus::Unprobed`], never
    /// as [`RunStatus::Finished`]. Fabricating "finished" from a probe that never
    /// actually ran would have `wait` announce a live run as over.
    #[test]
    fn probe_run_reports_an_unprobeable_record_as_unprobed_not_finished() {
        let dir = scratch("probe-run-unprobeable");
        let registry = Registry::open_in(dir.clone()).expect("open registry");

        let broken_lock_dir = dir.join("broken.lock");
        fs::create_dir(&broken_lock_dir).expect("create the directory the lock name resolves to");
        write_record(&dir, "broken", "opaque", "broken.lock");

        assert_eq!(
            registry.probe_run("opaque").expect("probe"),
            RunStatus::Unprobed,
            "an unprobeable record leaves the run's fate unknown, not confirmed over"
        );
        // `entries()` reaches the same "not confirmed" verdict independently, via
        // `Health::Unprobed` (T-206) — the two methods agree on this record's health
        // even though neither is built on the other's scan.
        let entries = registry.entries().expect("scan");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].health,
            Health::Unprobed,
            "entries() classifies an unprobeable entry Unprobed, agreeing with probe_run"
        );

        // A confirmed-live record under the same id outranks the unknown one: there
        // is something definite to wait for.
        let live = registry
            .register_plain("opaque", None, SystemTime::now())
            .expect("register a live run under the same id");
        assert_eq!(
            registry.probe_run("opaque").expect("probe"),
            RunStatus::Live,
            "a confirmed-live record is a stronger fact than an unprobeable one"
        );

        live.remove();
        let _ = fs::remove_dir_all(&dir);
    }
}
