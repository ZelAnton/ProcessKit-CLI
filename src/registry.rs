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
//!   local process. The restriction is re-asserted on every open so a pre-existing
//!   directory is locked down too.
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
//! The connection *endpoint* is reserved but unset here: the local transport lands
//! in T-008, so [`Record::endpoint`] is `None` today and future work fills it
//! without reshaping the record.

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
    /// The local transport connection address. **Reserved**: the transport is set up
    /// in T-008, so this is `None` today. The field exists now so filling it later
    /// does not reshape the record.
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
/// path of the record file (so a client can act on or reap it).
// The read side is exercised by the unit tests now and consumed by the
// control-plane clients (`inspect`/`cancel`/`kill`, T-008/T-009); the write side
// (create/remove) is what the runner uses today, so the reader is not yet called
// from the binary itself.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Entry {
    pub record: Record,
    pub health: Health,
    pub path: PathBuf,
}

/// A handle onto the per-user run registry directory.
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// Open the per-user registry, creating its directory (and parents) restricted
    /// to the owner. The location is [`REGISTRY_DIR_ENV`] if set, else the platform
    /// default.
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

    /// The registry directory on disk.
    #[allow(dead_code)] // Used by the tests; consumed by control-plane clients (T-008/T-009).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Register a starting run: write its [`Record`] and take the exclusive advisory
    /// lock that marks it live. The returned [`Registration`] holds that lock for the
    /// run's lifetime; dropping it (or calling [`Registration::remove`]) tears the
    /// entry down.
    ///
    /// `endpoint` is the local transport address, or `None` until the transport
    /// lands (T-008). `started` is the run's start time.
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
    /// by probing its lock file. Unreadable or malformed files are skipped rather
    /// than failing the whole scan — a corrupt entry must not blind a client to the
    /// healthy ones. This is the read side future clients (`inspect`/`cancel`/`kill`)
    /// build on: find the run whose `record.run_id` matches, then act only if it is
    /// live.
    #[allow(dead_code)] // Exercised by the tests; consumed by the clients in T-008/T-009.
    pub fn entries(&self) -> io::Result<Vec<Entry>> {
        let read_dir = match fs::read_dir(&self.dir) {
            Ok(read_dir) => read_dir,
            // A missing directory is simply an empty registry.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let mut entries = Vec::new();
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
            let lock_path = self.dir.join(&record.liveness.lock_file);
            let health = probe_health(&lock_path)?;
            entries.push(Entry {
                record,
                health,
                path,
            });
        }
        Ok(entries)
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
#[allow(dead_code)] // Called by `entries` (the read side); see its note.
fn probe_health(lock_path: &Path) -> io::Result<Health> {
    let lock = match OpenOptions::new().read(true).write(true).open(lock_path) {
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

#[cfg(unix)]
mod platform {
    //! Unix registry primitives: `0700` directories and BSD `flock` liveness locks.

    use std::fs::{self, DirBuilder, File, Permissions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    /// Owner-only directory: mode `0700`, re-asserted with `chmod` (which, unlike the
    /// initial `mkdir`, is not filtered by the umask) so both a freshly created and a
    /// pre-existing directory end up owner-only.
    pub fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        fs::set_permissions(dir, Permissions::from_mode(0o700))
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
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_LOCK_VIOLATION, HANDLE, HLOCAL, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Owner-only directory: create the chain, then replace its DACL with a protected
    /// (inheritance-blocking) ACL granting full control only to the current user.
    pub fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        restrict_to_current_user(dir)
    }

    /// Replace `dir`'s DACL with `D:P(A;OICI;FA;;;<current-user-SID>)`: **P**rotected
    /// (no inherited ACEs — the Windows analogue of not letting a parent's looser
    /// permissions apply), one allow-**F**ull-**A**ccess ACE for the current user,
    /// inherited by child objects and containers (**OICI**). Re-applied on every open,
    /// so a pre-existing directory is locked down too.
    fn restrict_to_current_user(dir: &Path) -> io::Result<()> {
        let sid = current_user_sid_string()?;
        let sddl = to_wide(&format!("D:P(A;OICI;FA;;;{sid})"));

        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `sddl` is a valid NUL-terminated UTF-16 string; on success
        // `descriptor` receives a LocalAlloc'd security descriptor freed below.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let result = apply_dacl(dir, descriptor);
        // SAFETY: `descriptor` came from the converter above (LocalAlloc'd).
        unsafe { LocalFree(descriptor as HLOCAL) };
        result
    }

    /// Apply the DACL from `descriptor` to `dir` as a protected DACL.
    fn apply_dacl(dir: &Path, descriptor: *mut core::ffi::c_void) -> io::Result<()> {
        let mut present = 0;
        let mut dacl = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: `descriptor` is a valid security descriptor from the converter.
        let ok = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let path = to_wide(&dir.to_string_lossy());
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
    fn current_user_sid_string() -> io::Result<String> {
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

    /// Encode a string as a NUL-terminated UTF-16 buffer for the wide Win32 APIs.
    fn to_wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
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
    /// (no inheritance), granting the current user, and granting no world/Everyone
    /// ACE? Verified by reading the DACL back as SDDL.
    #[cfg(test)]
    pub fn is_owner_only(dir: &Path) -> io::Result<bool> {
        let sddl = dacl_sddl(dir)?;
        let sid = current_user_sid_string()?;
        let protected = sddl.starts_with("D:P");
        let grants_user = sddl.contains(&sid);
        // `WD` is the SDDL alias for the Everyone/World SID; its presence in the DACL
        // would mean the directory is not owner-only.
        let grants_world = sddl.contains(";WD)") || sddl.contains(";WD;");
        Ok(protected && grants_user && !grants_world)
    }

    /// Test-only: read `dir`'s DACL back as an SDDL string.
    #[cfg(test)]
    fn dacl_sddl(dir: &Path) -> io::Result<String> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        };

        let path = to_wide(&dir.to_string_lossy());
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated; on success `descriptor`/`dacl` receive
        // pointers into a LocalAlloc'd descriptor freed below. Returns a WIN32_ERROR.
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

        let mut raw: *mut u16 = std::ptr::null_mut();
        // SAFETY: `descriptor` is the descriptor just read; on success `raw` receives
        // a LocalAlloc'd string freed below.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut raw,
                std::ptr::null_mut(),
            )
        };
        let result = if ok != 0 {
            // SAFETY: `raw` is a NUL-terminated UTF-16 string from the converter.
            Ok(unsafe { wide_to_string(raw) })
        } else {
            Err(io::Error::last_os_error())
        };
        // SAFETY: both allocations came from the Win32 calls above (LocalAlloc'd).
        unsafe {
            if !raw.is_null() {
                LocalFree(raw as HLOCAL);
            }
            LocalFree(descriptor as HLOCAL);
        }
        result
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
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        assert!(
            platform::is_owner_only(registry.dir()).expect("read permissions"),
            "the registry directory must be restricted to its owner"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A registered run writes a well-formed record: the run id, a reserved (null)
    /// endpoint, the start timestamp, and the advisory-lock liveness signal — and
    /// carries no PID.
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
        assert!(record.endpoint.is_none(), "endpoint is reserved for T-008");
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
}
