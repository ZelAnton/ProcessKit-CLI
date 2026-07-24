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
//!   directory is locked down too; [`Registry::open_read_only`] (`list`'s path)
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
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::events;

/// On-disk record format version. Independent of the JSONL event
/// [`schema_version`](crate::events::SCHEMA_VERSION): the registry is a private
/// per-user contract between a runner and its own control-plane clients, not the
/// public event stream, so it versions on its own axis.
pub const REGISTRY_VERSION: u32 = 1;

/// The only liveness mechanism today: an OS advisory lock held for the run's life.
const LIVENESS_ADVISORY_LOCK: &str = "advisory_lock";

/// Environment override for the registry directory. Set it to pin the location —
/// used by the integration tests to isolate a scratch registry, and available to
/// an orchestrator that wants the registry somewhere specific. When unset the
/// platform default ([`platform::default_registry_dir`]) is used.
const REGISTRY_DIR_ENV: &str = "PROCESSKIT_CLI_REGISTRY_DIR";

/// The registry record a runner writes at start and removes on a clean exit.
///
/// `Serialize` + `Deserialize`: the runner writes it, future control-plane clients
/// read it back. Deliberately carries **no PID** — a run is addressed by `run_id`,
/// never by process id.
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
    pub endpoint: Option<String>,
    /// Run start time, RFC 3339 UTC with millisecond precision (same formatter as the
    /// JSONL events, see [`events::format_rfc3339_utc`]).
    pub started_at: String,
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
    /// the lock file is gone): the entry is stale and must not be treated as live.
    Stale,
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
/// [`Entry`]) and [`Registry::prune`] (which reaps only the confirmed-stale ones).
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
    /// Confirmed-stale entries whose `.json`/`.lock` files were reaped.
    pub pruned: usize,
    /// Live entries left untouched (a live runner holds the lock).
    pub live: usize,
    /// Entries whose liveness could not be probed (the lock file would not open, or
    /// the lock call errored) and were therefore left in place rather than risked.
    pub unprobed: usize,
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
    /// that only wants to *read* the registry — `list`/`prune`, and the control
    /// clients `inspect`/`cancel`/`kill` — must use [`Registry::open_read_only`]
    /// instead, so a read-only scan cannot itself create registry state or touch
    /// its permissions.
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
    /// (`list`/`prune`, and the control clients `inspect`/`cancel`/`kill`) that
    /// must never mutate registry state just to look at it. The
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
    /// `endpoint` is the local transport address the runner published (a unix socket
    /// path / Windows pipe name), or `None` when no transport could be stood up.
    /// `started` is the run's start time.
    pub fn register(
        &self,
        run_id: &str,
        endpoint: Option<&str>,
        started: SystemTime,
    ) -> io::Result<Registration> {
        // Reserve a unique, opaque entry stem via the filesystem itself (create_new),
        // and take the live lock on the fresh lock file before publishing the record.
        let reserved = self.reserve_entry()?;

        let record = Record {
            registry_version: REGISTRY_VERSION,
            run_id: run_id.to_string(),
            endpoint: endpoint.map(str::to_string),
            started_at: events::format_rfc3339_utc(started),
            liveness: Liveness {
                kind: LIVENESS_ADVISORY_LOCK.to_string(),
                lock_file: file_name(&reserved.lock_path),
            },
        };
        let json = serde_json::to_string(&record).map_err(io::Error::other)?;
        // The record is written only after the lock is held, so an entry is never
        // visible to a scanner in a state where it looks live but no lock exists.
        fs::write(&reserved.json_path, json)?;

        Ok(Registration {
            json_path: reserved.json_path,
            lock_path: reserved.lock_path,
            lock: reserved.lock,
            removed: AtomicBool::new(false),
        })
    }

    /// Scan every entry, classifying each as [`Health::Live`] or [`Health::Stale`]
    /// by probing its lock file. A malformed *record* (unparsable JSON, or one whose
    /// `started_at`/`lock_file` field is not the shape a well-behaved runner writes)
    /// is corrupt-record noise and is skipped outright — there is no lock path worth
    /// probing. A well-formed record whose lock file *cannot be probed* (any
    /// non-`NotFound` error opening it, or a lock/unlock error) is different: the
    /// record itself is trustworthy, only its liveness is unknowable, so the entry is
    /// still returned — classified [`Health::Stale`] ("could not confirm liveness ⇒
    /// treat as not live") — rather than dropped. Either way one bad entry never
    /// aborts the whole scan or blinds a client to the healthy ones. This is the read
    /// side the control-plane client (`inspect`, T-008; `cancel`/`kill`, T-009) builds
    /// on: find the run whose `record.run_id` matches, then act only if it is live —
    /// which a probe-failed entry, being `Stale`, never is.
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
            // liveness could not be confirmed. Degrade to `Stale` rather than
            // dropping the entry or aborting the scan — "could not confirm liveness ⇒
            // treat as not live" is the least-surprising verdict, it keeps the entry
            // visible for the `prune` reaper (T-164), and it is exactly what fixes the
            // misrouting bug this task exists for: `inspect`/`cancel`/`kill` act only
            // on `Live` entries, so a record whose probe failed can no longer fail
            // the whole scan and take down an operation on an unrelated, healthy
            // run_id. **Prune must not reuse this collapsed value** — it cannot tell a
            // genuinely stale entry from a probe-failed one — and so probes on its own
            // path (see [`Registry::prune`] / [`probe_for_prune`]).
            let health = probe_health(&lock_path).unwrap_or(Health::Stale);
            entries.push(Entry {
                record,
                health,
                path: json_path,
            });
        }
        Ok(entries)
    }

    /// Reap every **confirmed-stale** entry, deleting both its files, and leave every
    /// other entry untouched. The safe-by-construction cleanup for the leftover
    /// `.json`/`.lock` pair an abruptly-killed runner leaves behind (its clean-exit
    /// [`Registration::remove`] never ran), which the registry would otherwise
    /// accumulate forever.
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
    ///   that may belong to a live run. This is exactly the distinction
    ///   [`Registry::entries`] deliberately throws away (its `.unwrap_or(Health::Stale)`
    ///   folds `Err` into `Stale` — see [K-024]); prune therefore probes on its **own**
    ///   path ([`probe_for_prune`]), which keeps the three cases apart, and never reads
    ///   the collapsed [`Entry::health`].
    ///
    /// Corrupt records the scan already skips (unreadable, unparsable JSON, a malformed
    /// `started_at`, or a `lock_file` that is not a simple in-directory name) are
    /// **not** candidates — they are never probed and never deleted, exactly as
    /// [`Registry::entries`] leaves them alone. No entry is ever addressed by PID: a
    /// candidate is reached only through the record path the directory scan already
    /// produced.
    ///
    /// A confirmed-stale entry is reaped **while its lock is still held** (see
    /// [`probe_for_prune`]): the reclaim keeps the exclusive lock across the two
    /// deletions, so a second concurrent prune sees the entry as live and skips it
    /// rather than racing on the same files — the "hold the lock to reclaim" pattern
    /// `docs/registry.md` documents. Deletion mirrors [`Registration::remove`]: the
    /// record (`.json`) first, then the lock (`.lock`), each best-effort — an OS delete
    /// error on one entry never aborts the reaping of the others (a leftover just reads
    /// as stale again next time). Running prune over an already-clean registry is a
    /// no-op, not an error.
    pub fn prune(&self) -> io::Result<PruneOutcome> {
        let mut outcome = PruneOutcome::default();
        for ScannedRecord {
            json_path,
            lock_path,
            ..
        } in self.scan()?
        {
            match probe_for_prune(&lock_path) {
                // Confirmed stale: reap both files while still holding the acquired
                // lock (when there was one). The record is deleted first, then the
                // lock file, mirroring `Registration::remove`; the held lock is
                // released only when `_held_lock` drops after both deletes.
                Ok(PruneProbe::Reapable(_held_lock)) => {
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
        Ok(outcome)
    }

    /// Scan the registry directory into the records that pass every corruption guard,
    /// each paired with the two on-disk paths it resolves to — the shared read step
    /// under both [`Registry::entries`] (which probes each into an [`Entry`]) and
    /// [`Registry::prune`] (which reaps only the confirmed-stale ones). Sharing this
    /// step guarantees the two paths agree exactly on which records are corrupt-and-
    /// skipped versus real-and-probed, so prune can never act on a record `entries`
    /// would have dropped. A missing directory is simply an empty registry.
    fn scan(&self) -> io::Result<Vec<ScannedRecord>> {
        let read_dir = match fs::read_dir(&self.dir) {
            Ok(read_dir) => read_dir,
            // A missing directory is simply an empty registry.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut scanned = Vec::new();
        for dir_entry in read_dir {
            let path = dir_entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Record>(&text) else {
                continue;
            };
            // `started_at` is untrusted deserialized data too: a record written by a
            // well-behaved runner always carries an [`events::format_rfc3339_utc`]
            // value, but a corrupted or hand-edited record could carry anything
            // `serde_json` will accept into a `String` field. A malformed value is
            // corrupt-record noise, not a real start time — skip it like any other
            // corrupt entry rather than listing (and sorting) garbage as if it were
            // valid.
            if !is_valid_rfc3339_millis_utc(&record.started_at) {
                continue;
            }
            // The `lock_file` field is untrusted deserialized data. Validate it as a
            // simple, single-component, relative `.lock` name *before* joining it onto
            // the registry directory — a value carrying `..`, a path separator, an
            // absolute path, a NUL/control character, or a Windows reserved device
            // name (even in the name-plus-extension aliasing form) would otherwise let
            // a corrupt or adversarial record steer the liveness probe at a file
            // outside the owner-only registry directory. A failing value is a corrupt
            // record and is skipped, exactly like an unreadable or unparsable file.
            if !is_simple_lock_file_name(&record.liveness.lock_file) {
                continue;
            }
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
                    // A freshly created, unique file cannot already be locked, so
                    // failing to acquire it is a real error, not a live-holder signal.
                    if !platform::try_lock_exclusive(&lock)? {
                        return Err(io::Error::other(
                            "could not take the liveness lock on a fresh registry entry",
                        ));
                    }
                    let json_path = self.dir.join(format!("{stem}.json"));
                    return Ok(ReservedEntry {
                        json_path,
                        lock_path,
                        lock,
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

/// A reserved-but-not-yet-published entry: its paths and the held lock.
struct ReservedEntry {
    json_path: PathBuf,
    lock_path: PathBuf,
    lock: File,
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

/// Probe an entry's liveness through its lock file, without trusting file existence.
///
/// Trying a non-blocking exclusive lock is the whole test: acquiring it means no
/// live runner holds it, so the entry is [`Health::Stale`]; being denied means a
/// live runner holds it, so it is [`Health::Live`]. A missing lock file is stale by
/// definition. When the probe acquires the lock it drops it immediately (the entry
/// is stale, not being claimed) — a client that means to *reclaim* a stale entry
/// would instead keep the lock held.
///
/// The lock file is opened *without following a symlink* at its final component
/// ([`platform::open_lock_file`]: `O_NOFOLLOW` on unix, reparse-point rejection on
/// Windows), closing the open-time TOCTOU window that a symlink swapped in after the
/// name check would otherwise open — the probe can only ever touch a regular file
/// inside the registry directory, never a link redirecting elsewhere.
fn probe_health(lock_path: &Path) -> io::Result<Health> {
    let lock = match platform::open_lock_file(lock_path) {
        Ok(lock) => lock,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Health::Stale),
        Err(err) => return Err(err),
    };
    if platform::try_lock_exclusive(&lock)? {
        // Acquired: no live holder. Drop the handle here to release it at once.
        drop(lock);
        Ok(Health::Stale)
    } else {
        Ok(Health::Live)
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

/// Probe an entry's lock file **for pruning**, keeping apart the three cases
/// [`Registry::entries`] deliberately folds together (its `.unwrap_or(Health::Stale)`
/// collapses a probe *error* into `Stale`, so a probe-failed record is indistinguishable
/// there from a genuinely dead one — see [K-024]). Here they stay distinct, because
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

    use std::fs::{self, DirBuilder, File, Permissions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

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
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, LOCKFILE_EXCLUSIVE_LOCK,
        LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

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
            .register("run-42", None, started)
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

    /// A clean exit removes the entry: files gone, and the scan sees nothing.
    #[test]
    fn clean_removal_deletes_the_entry() {
        let dir = scratch("remove");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register("run-clean", None, SystemTime::now())
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
            .register("run-victim", None, SystemTime::now())
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
        let first = registry.register("run-a", None, now).expect("register a");
        let second = registry.register("run-b", None, now).expect("register b");
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
        let record = Record {
            registry_version: REGISTRY_VERSION,
            run_id: run_id.to_string(),
            endpoint: None,
            started_at: events::format_rfc3339_utc(SystemTime::now()),
            liveness: Liveness {
                kind: LIVENESS_ADVISORY_LOCK.to_string(),
                lock_file: lock_file.to_string(),
            },
        };
        let json = serde_json::to_string(&record).expect("serialize the record");
        fs::write(dir.join(format!("{stem}.json")), json).expect("write the record");
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
            .register("good", None, SystemTime::now())
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
            .register("good", None, SystemTime::now())
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
    /// itself is well-formed), but degrades to `Stale`: the probe error must never
    /// let the link be followed onto an off-target file, and must never abort the
    /// whole scan either.
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
        // still returned (its record is well-formed) but degrades to `Stale` rather
        // than ever being reported `Live` off a link it never actually locked.
        let entries = registry.entries().expect("scan");
        let linked = entries
            .iter()
            .find(|entry| entry.record.run_id == "linked")
            .expect("a probe-failed entry is still returned, not dropped");
        assert_eq!(
            linked.health,
            Health::Stale,
            "an unprobeable lock file (symlink) must degrade to Stale, not abort the scan"
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
    /// broken one degrades to `Stale` rather than disappearing or taking the scan
    /// down with it — the exact bug this task fixes (a stale/broken record no
    /// longer fails `inspect`/`cancel`/`kill` routing to a *different*, healthy
    /// run_id).
    #[test]
    fn entries_degrades_an_unprobeable_lock_directory_to_stale_without_aborting_the_scan() {
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
            .register("good", None, SystemTime::now())
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
            Health::Stale,
            "a record whose lock probe cannot even open must degrade to Stale"
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
            .register("victim", None, SystemTime::now())
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
    /// [`entries_degrades_an_unprobeable_lock_directory_to_stale_without_aborting_the_scan`].
    #[test]
    fn prune_never_reaps_a_live_entry() {
        let dir = scratch("prune-live");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let now = SystemTime::now();

        let live = registry
            .register("alive", None, now)
            .expect("register the live run");
        let doomed = registry
            .register("dead", None, now)
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
            },
            "the unprobeable entry is still kept on a repeat pass"
        );
        assert!(
            dir.join("broken.json").exists(),
            "the unprobeable record survives every prune"
        );

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
}
