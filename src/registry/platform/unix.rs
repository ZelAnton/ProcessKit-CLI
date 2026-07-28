//! Unix registry primitives: `0700` directories and BSD `flock` liveness locks.

use std::ffi::CString;
use std::fs::{self, DirBuilder, File, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::control::{SOCKET_FILE_NAME, socket_base_dirs, unix_control_endpoint_dir};

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
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
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
    let dir = unix_control_endpoint_dir(endpoint)?;
    // The directory must sit *directly* in a base `ControlServer::bind` uses —
    // one level, no deeper — so nothing outside those bases is ever a candidate.
    // The bases themselves come from this process's own environment, not from the
    // record, so they are compared as paths (`/tmp` and `/tmp/` are one place),
    // while the untrusted part above had to match to the character.
    let base = dir.parent()?;
    if !bases.iter().any(|candidate| candidate == base) {
        return None;
    }
    Some(dir)
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
