//! Windows registry primitives: an owner-only *protected* DACL (the equivalent
//! of unix `0700`) and `LockFileEx` liveness locks.
//!
//! # Why the hardening is verify-then-repair rather than always-write (T-309)
//!
//! [`create_owner_only_dir`] runs on every `run` invocation — it is the one
//! mutating registry open left after T-174 routed every reader through
//! `Registry::open_read_only`. Writing the DACL unconditionally is not a
//! constant-cost metadata write on this platform: the ACE is *inheritable*
//! (`OICI`) and the object is a *directory*, so `SetNamedSecurityInfoW`
//! re-propagates it over the directory's existing children — and the registry
//! holds a `.json` record plus a `.lock` file per remembered run.
//!
//! The in-repo attribution benchmark (`benches/registry_open_bench.rs`) measured
//! exactly that shape on Windows 11 / NTFS rather than assuming it: the whole
//! open cost ~0.75 ms on an empty registry but ~22 ms at 64 entries, ~89 ms at
//! 256, and ~310 ms at 1024 — roughly 0.15 ms per child object — while the
//! `fs::create_dir_all` half of the same call stayed flat at ~0.2 ms and a
//! read-only open at ~50 ns. So the propagation effect is **confirmed**, and it
//! is the whole of the growth. (The same benchmark's `hardening_write` column
//! keeps that measurement reproducible now that `run` no longer pays it.) With
//! the fast path below the open is flat at ~0.1 ms across every one of those
//! registry sizes, and the create path — one `CreateDirectoryW` carrying the
//! descriptor instead of `create_dir_all` plus a separate write — dropped from
//! ~0.79 ms to ~0.54 ms.
//!
//! So the fast path *verifies* the directory's current DACL
//! ([`dacl_already_owner_only`], a single `GetNamedSecurityInfoW` that reads one
//! object's security and walks no children) and writes only on a mismatch. This
//! is safe to do because it is **not** conditional on any weaker proxy for the
//! guarantee:
//!
//! - The skip is taken only when the observed DACL is *exactly* the one this
//!   module would otherwise write — present, `SE_DACL_PROTECTED`, and ACE-for-ACE
//!   identical (type, inheritance flags, access mask, and binary SID) to the
//!   descriptor built right here. The post-condition of the two branches is the
//!   same state, so nothing an attacker can do to the DACL yields a *weaker*
//!   directory: any deviation — including a stricter one — routes to the
//!   unconditional write.
//! - Any doubt fails **closed**: an unreadable security descriptor, an absent
//!   (`NULL`) DACL, an unprotected one, an extra or missing ACE, a non-allow ACE,
//!   or a path that is not a directory all fall through to the write.
//! - Deliberately **rejected** alternatives that would have made hardening
//!   conditional on attacker-influenceable state: skipping because the directory
//!   merely *exists*, because a sentinel/marker file is present, because an
//!   mtime or a cached per-process/per-boot "already hardened" flag says so, or
//!   verifying only part of the ACL (just the protected bit, or just "some ACE
//!   names our SID"). Each of those can be satisfied by a principal who cannot
//!   currently defeat the DACL, so each would let that principal *suppress* the
//!   repair. None is used.
//!
//! Rewriting the DACL is not a lock either way: a principal holding `WRITE_DAC`
//! on the directory can loosen it the instant after an unconditional write just
//! as well as before a verifying one, so the verify does not narrow the window
//! that the always-write version had. Ownership is out of scope here and
//! unchanged: neither the old nor the new code touches `OWNER_SECURITY_INFORMATION`.
//!
//! The creation path additionally attaches the descriptor to `CreateDirectoryW`
//! itself, so a freshly created registry directory is never momentarily
//! reachable through permissions inherited from its parent — a real window in
//! the old create-then-restrict order whenever `PROCESSKIT_CLI_REGISTRY_DIR`
//! points somewhere world-writable, because a handle opened during that window
//! keeps its granted access after the DACL is tightened. Its performance
//! contribution is nil (a registry directory is created once per user), and the
//! result is still verified before the write is skipped, so a filesystem that
//! silently ignores creation-time security descriptors falls through to
//! `SetNamedSecurityInfoW` and fails there exactly as it does today.

use std::fs::{self, File};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use crate::win_security::SecurityDescriptor;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_LOCK_VIOLATION, ERROR_PATH_NOT_FOUND, HANDLE, HLOCAL, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PRESENT, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The allow-ACE type tag (`ACCESS_ALLOWED_ACE_TYPE`, 0). windows-sys 0.61 only
/// re-exports the constant behind `Win32_System_SystemServices`, which this crate
/// does not enable; the value is a stable part of the ACE ABI.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// Owner-only directory. On return the directory exists and carries the
/// protected, owner-only DACL described in [`owner_only_descriptor`] — by having
/// been created with it, by having been verified to already have exactly it, or
/// by having had it written. See this module's header for why the middle branch
/// does not weaken the guarantee.
pub fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
    let descriptor = owner_only_descriptor()?;

    // (1) The steady state every invocation after the first hits: an existing
    //     directory whose DACL already *is* the target. Nothing to write, and no
    //     inheritable-ACE propagation over the records already in it.
    if is_existing_directory(dir) && dacl_already_owner_only(dir, &descriptor) {
        return Ok(());
    }

    // (2) Create it carrying the descriptor, so it never exists with merely
    //     inherited permissions. Verified rather than trusted: a filesystem that
    //     ignores creation-time security descriptors falls through to (3).
    if create_directory_with_descriptor(dir, &descriptor)?
        && dacl_already_owner_only(dir, &descriptor)
    {
        return Ok(());
    }

    // (3) Pre-existing with the wrong DACL (the repair path), or created on a
    //     filesystem that dropped the descriptor: assert it unconditionally.
    apply_dacl(dir, &descriptor)
}

/// Does `dir` name an existing directory? Used only to keep the verify fast path
/// from accepting a *file* that happens to carry a matching DACL — a path that
/// exists as a non-directory must still reach [`create_directory_with_descriptor`],
/// which reports it as an error the way `fs::create_dir_all` always has.
fn is_existing_directory(dir: &Path) -> bool {
    fs::metadata(dir).is_ok_and(|metadata| metadata.is_dir())
}

/// The registry directory's access policy: a **P**rotected DACL (inherited ACEs
/// from the parent are blocked — the Windows analogue of not letting a parent's
/// looser permissions apply) holding one allow-**F**ull-**A**ccess ACE for the
/// current user, inherited by child objects and containers (**OICI**) so the
/// records and lock files created inside it are covered too.
///
/// The inheritance flags are load-bearing and are deliberately **not** dropped as
/// part of T-309's cost reduction, even though every child is created by this
/// process inside the already-protected directory: Windows grants
/// `SeChangeNotifyPrivilege` (bypass traverse checking) to everyone by default, so
/// a file inside an owner-only directory is still reachable by full path and its
/// *own* DACL is what refuses another principal. Without an inheritable ACE a new
/// record would fall back to the creating token's default DACL instead of the
/// explicit owner-only grant. What T-309 removed is the repeated *re-propagation*
/// of this ACE over children that already carry it, not the inheritance itself.
fn owner_only_descriptor() -> io::Result<SecurityDescriptor> {
    let sid = current_user_sid_string()?;
    // The shared RAII wrapper owns the LocalAlloc'd descriptor and frees it on
    // drop, so there is no manual `LocalFree` here.
    SecurityDescriptor::from_sddl(&format!("D:P(A;OICI;FA;;;{sid})"))
}

/// Create `dir` with `descriptor` attached, reporting whether this call is the one
/// that created it (`true`) or it already existed (`false`).
///
/// Only the final component is created with the descriptor; missing intermediate
/// components are created by `fs::create_dir_all` with the permissions they
/// inherit, exactly as they were before T-309 (the old code created the whole
/// chain that way and hardened only the leaf).
fn create_directory_with_descriptor(
    dir: &Path,
    descriptor: &SecurityDescriptor,
) -> io::Result<bool> {
    match create_directory(dir, descriptor) {
        Ok(()) => return Ok(true),
        // Missing intermediate components — create them and retry the leaf once.
        Err(err) if is_os_error(&err, ERROR_PATH_NOT_FOUND) => {}
        Err(err) => return existing_directory_or(dir, err),
    }

    match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => fs::create_dir_all(parent)?,
        _ => {}
    }
    match create_directory(dir, descriptor) {
        Ok(()) => Ok(true),
        Err(err) => existing_directory_or(dir, err),
    }
}

/// The fallback for a failed `CreateDirectoryW`: `Ok(false)` — "it is already
/// there, go assert its DACL" — when the path *is* a directory, and otherwise the
/// create error itself, which is what an existing **file** at the registry path
/// surfaces as (`AlreadyExists`, the same verdict `fs::create_dir_all` gave).
///
/// Applied to every failure rather than only `ERROR_ALREADY_EXISTS`, mirroring
/// `fs::create_dir_all`'s own "any error, but the directory exists, is not an
/// error" arm. That matters for the guarantee, not just for compatibility: a
/// create that fails for some *other* reason on a directory that nonetheless
/// exists must still reach the unconditional DACL write, never abort before it.
fn existing_directory_or(dir: &Path, err: io::Error) -> io::Result<bool> {
    if is_existing_directory(dir) {
        return Ok(false);
    }
    Err(err)
}

/// One `CreateDirectoryW` carrying `descriptor` as the new directory's security
/// descriptor.
fn create_directory(dir: &Path, descriptor: &SecurityDescriptor) -> io::Result<()> {
    let path = crate::win_security::to_wide_path(dir);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: `path` is NUL-terminated and `attributes` points at a well-formed
    // SECURITY_ATTRIBUTES whose descriptor (`descriptor`) outlives this call;
    // CreateDirectoryW only reads both.
    let ok = unsafe { CreateDirectoryW(path.as_ptr(), &attributes) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Does `err` carry the Win32 error code `code`?
fn is_os_error(err: &io::Error, code: u32) -> bool {
    err.raw_os_error() == Some(code as i32)
}

/// Is `dir`'s current DACL already **exactly** the one `expected` carries — the
/// verify half of the verify-then-repair fast path?
///
/// Strict by construction: the descriptor compared against is the very one
/// [`apply_dacl`] would write, so the two can never drift apart, and the match
/// requires the security descriptor to be `SE_DACL_PRESENT` and
/// `SE_DACL_PROTECTED` plus an ACE-for-ACE identity (count, allow-type,
/// inheritance flags, access mask, and *binary* SID — see [K-002] for why a
/// string SDDL round-trip is not a sound comparison). Everything else, including
/// every failure to read the descriptor at all, answers `false` and sends the
/// caller to the unconditional write: this predicate may only ever *skip* work
/// that is provably redundant, never decide that hardening is unnecessary.
fn dacl_already_owner_only(dir: &Path, expected: &SecurityDescriptor) -> bool {
    let Ok(expected_dacl) = descriptor_dacl(expected) else {
        return false;
    };
    read_dacl(dir, |control, current| {
        control & SE_DACL_PRESENT != 0
            && control & SE_DACL_PROTECTED != 0
            && acls_are_identical(current, expected_dacl)
    })
    .unwrap_or(false)
}

/// Test-only: would the *next* [`create_owner_only_dir`] on `dir` take the verify
/// fast path — i.e. is the directory in the exact state that makes the
/// `SetNamedSecurityInfoW` write provably redundant?
///
/// Exists so the regression tests can pin the optimization itself, not just its
/// security post-condition: without it, a silent regression to always-write (or,
/// worse, a fast path that engages on a *loosened* directory) would be invisible
/// to a test that only checks the resulting permissions.
#[cfg(test)]
pub fn takes_verified_fast_path(dir: &Path) -> io::Result<bool> {
    let descriptor = owner_only_descriptor()?;
    Ok(is_existing_directory(dir) && dacl_already_owner_only(dir, &descriptor))
}

/// Test-only: replace `dir`'s DACL with a **loosened, unprotected** one that also
/// grants read access to Everyone — the Windows analogue of the `chmod 0755` the
/// unix owner-only tests use to simulate a pre-existing directory whose
/// permissions were widened out of band. Both halves of the owner-only guarantee
/// are broken by it (the DACL is no longer protected against inherited ACEs, and
/// it names a principal other than the current user), so a subsequent open must
/// repair it.
#[cfg(test)]
pub fn loosen_dacl(dir: &Path) -> io::Result<()> {
    // `WD` is Everyone, `FR` file-read — deliberately read-only and applied to an
    // empty scratch directory that the same test repairs and deletes, so the
    // fixture never grants another principal more than the unix `0755` twin does.
    let loosened = SecurityDescriptor::from_sddl("D:(A;OICI;FR;;;WD)")?;
    let dacl = descriptor_dacl(&loosened)?;

    let path = crate::win_security::to_wide_path(dir);
    // SAFETY: `path` is NUL-terminated and `dacl` points into the live `loosened`
    // descriptor. Note the deliberately *unprotected* information flags — the
    // fixture must leave the DACL inheritable-from-parent, unlike production.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl.cast_mut(),
            std::ptr::null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// Do two DACLs grant exactly the same thing to exactly the same principals, in
/// the same order? Both must be non-null, hold the same number of ACEs, and every
/// ACE must be an allow-ACE with identical flags, mask, and SID.
fn acls_are_identical(current: *const ACL, expected: *const ACL) -> bool {
    if current.is_null() || expected.is_null() {
        return false;
    }
    // SAFETY: both pointers are non-null per the guard above and point at live
    // ACLs owned by their respective security descriptors.
    let (current_count, expected_count) = unsafe { ((*current).AceCount, (*expected).AceCount) };
    // An empty DACL denies everyone, including the owner: never our policy, and
    // never something to accept as "already correct".
    if expected_count == 0 || current_count != expected_count {
        return false;
    }
    (0..u32::from(expected_count)).all(|index| {
        match (allow_ace_at(current, index), allow_ace_at(expected, index)) {
            (Some(left), Some(right)) => left.matches(&right),
            _ => false,
        }
    })
}

/// The comparable content of one allow-ACE: its inheritance flags, its access
/// mask, and a borrowed pointer to its in-place SID.
struct AllowAce {
    flags: u8,
    mask: u32,
    sid: *mut core::ffi::c_void,
}

impl AllowAce {
    /// Same inheritance flags, same access mask, same account. The SIDs are
    /// compared **binary** (`EqualSid`), never as rendered SDDL text — see [K-002].
    fn matches(&self, other: &Self) -> bool {
        if self.flags != other.flags || self.mask != other.mask {
            return false;
        }
        // SAFETY: both pointers address the in-place SID of a live ACE inside an
        // ACL that outlives this call; EqualSid only reads them.
        unsafe { EqualSid(self.sid, other.sid) != 0 }
    }
}

/// The ACE at `index`, or `None` when it cannot be read or is not a plain
/// allow-ACE (a deny/audit/object ACE means the DACL is more than the flat grant
/// this module writes, so it is never "already correct").
fn allow_ace_at(acl: *const ACL, index: u32) -> Option<AllowAce> {
    let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `acl` is a live ACL and `index` is within `0..AceCount`.
    let got = unsafe { GetAce(acl, index, &mut ace) };
    if got == 0 || ace.is_null() {
        return None;
    }
    let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
    // SAFETY: `ace` points at a valid ACE inside the live ACL; every ACE begins
    // with an `ACE_HEADER`, so reading the header is in bounds whatever its type.
    let (ace_type, flags) = unsafe { ((*ace).Header.AceType, (*ace).Header.AceFlags) };
    if ace_type != ACCESS_ALLOWED_ACE_TYPE {
        return None;
    }
    // SAFETY: the type check above establishes the ACE really is an
    // `ACCESS_ALLOWED_ACE`, so its `Mask` and in-place `SidStart` are within it.
    let (mask, sid) = unsafe { ((*ace).Mask, (&raw const (*ace).SidStart).cast_mut().cast()) };
    Some(AllowAce { flags, mask, sid })
}

/// Read the DACL of the file-system object at `object` (the registry directory,
/// or — for the test-only child checks — a record file) and hand it, with the
/// security descriptor's control word, to `inspect`, which runs while the
/// `LocalAlloc`'d descriptor is still alive and must not let either escape. `dacl`
/// is null when the object has no DACL at all (a `NULL` DACL, which grants
/// everyone); callers must handle that.
fn read_dacl<T>(object: &Path, inspect: impl FnOnce(u16, *const ACL) -> T) -> io::Result<T> {
    let path = crate::win_security::to_wide_path(object);
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
        Ok(inspect(control, dacl))
    };

    // SAFETY: `descriptor` came from GetNamedSecurityInfoW (LocalAlloc'd).
    unsafe { LocalFree(descriptor as HLOCAL) };
    verdict
}

/// The DACL inside a security descriptor this process built.
fn descriptor_dacl(descriptor: &SecurityDescriptor) -> io::Result<*const ACL> {
    let mut present = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = 0;
    // SAFETY: the descriptor is alive for the whole call (borrowed from the
    // caller); on success `dacl` points into it.
    let ok = unsafe {
        GetSecurityDescriptorDacl(descriptor.as_ptr(), &mut present, &mut dacl, &mut defaulted)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(dacl)
}

/// Write `descriptor`'s DACL onto `dir` as a **protected** DACL — the
/// unconditional assertion the verify fast path falls through to. This is the
/// call whose cost grows with the directory's child count, because
/// `SetNamedSecurityInfoW` re-propagates the inheritable ACE down the tree.
fn apply_dacl(dir: &Path, descriptor: &SecurityDescriptor) -> io::Result<()> {
    let dacl = descriptor_dacl(descriptor)?;

    let path = crate::win_security::to_wide_path(dir);
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
            dacl.cast_mut(),
            std::ptr::null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
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
    let _ = unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
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
    let user_sid = current_user_sid_bytes()?;
    read_dacl(dir, |control, dacl| {
        dacl_is_owner_only(control, dacl, &user_sid)
    })
}

/// Test-only: is `dacl` (with security-descriptor `control` flags) an owner-only
/// grant to `user_sid` — present, protected (no inherited ACEs), and composed solely
/// of allow-ACEs naming that one SID? An absent/null DACL (grants everyone), an
/// unprotected DACL (could inherit wider ACEs), an empty DACL (denies even the
/// owner), any non-allow ACE, or any ACE for a different account (Everyone included)
/// all fail the check — making it strictly stronger than the old SDDL scan.
///
/// Deliberately *weaker* than [`has_exact_owner_only_dacl`] below: this asks only
/// "is any other principal named here", which is the question the pre-existing
/// owner-only tests (and their unix counterpart, a plain `0700` compare) ask.
#[cfg(test)]
fn dacl_is_owner_only(control: u16, dacl: *const ACL, user_sid: &[u8]) -> bool {
    control & SE_DACL_PROTECTED != 0 && dacl_names_only(control, dacl, user_sid)
}

/// Test-only: does `dir`'s record/lock file — a *child* of the registry directory,
/// created by us inside it — itself grant access to nobody but the current user?
///
/// The `SE_DACL_PROTECTED` requirement [`is_owner_only`] applies to the directory
/// is deliberately *not* checked here: a child's owner-only grant arrives by
/// **inheritance** from the directory's `OICI` ACE, and an inheriting DACL is by
/// definition unprotected. This is the assertion that keeps decision (c) of T-309
/// honest — dropping the inheritance flags to make the DACL write cheaper would
/// leave a record file falling back to the creating token's default DACL, which
/// matters because bypass-traverse-checking lets another principal reach the file
/// by full path regardless of the directory's own ACL.
#[cfg(test)]
pub fn grants_only_current_user(path: &Path) -> io::Result<bool> {
    let user_sid = current_user_sid_bytes()?;
    read_dacl(path, |control, dacl| {
        dacl_names_only(control, dacl, &user_sid)
    })
}

/// Test-only: is `dacl` present, non-empty, and composed solely of allow-ACEs
/// naming `user_sid`? An absent (`NULL`) DACL grants everyone, and an empty one
/// denies even the owner; both fail, as does any ACE for another account.
#[cfg(test)]
fn dacl_names_only(control: u16, dacl: *const ACL, user_sid: &[u8]) -> bool {
    if dacl.is_null() || control & SE_DACL_PRESENT == 0 {
        return false;
    }

    // SAFETY: `dacl` is present and non-null per the guard above.
    let ace_count = unsafe { (*dacl).AceCount };
    // The DACL we apply is exactly one allow-ACE; an empty DACL is not owner-only.
    if ace_count == 0 {
        return false;
    }

    (0..u32::from(ace_count)).all(|index| match allow_ace_at(dacl, index) {
        // SAFETY: `ace.sid` is the ACE's in-place SID inside the live DACL and
        // `user_sid` is our owned copy of the current user's SID; EqualSid only
        // reads both.
        Some(ace) => unsafe { EqualSid(ace.sid, user_sid.as_ptr() as *mut core::ffi::c_void) != 0 },
        // Unreadable, or a non-allow (deny/audit/object) ACE: more than a plain grant.
        None => false,
    })
}

/// Test-only: is `dir`'s DACL **exactly** the shape both T-309 branches must
/// produce — a present, protected DACL holding one single allow-ACE that grants
/// `FILE_ALL_ACCESS` to the current user and is inherited by child objects and
/// containers (`OICI`)?
///
/// Written against literal Win32 constants rather than against the production
/// descriptor, deliberately. [`dacl_already_owner_only`] compares the directory
/// with the very SDDL this module would write, so by construction it cannot
/// notice the policy itself changing (or silently narrowing — e.g. losing the
/// inheritance flags a record file depends on, see [`owner_only_descriptor`]).
/// This function is the independent pin the regression tests assert the
/// *resulting permissions* with, on both the verified fast path and the repair
/// path.
#[cfg(test)]
pub fn has_exact_owner_only_dacl(dir: &Path) -> io::Result<bool> {
    use windows_sys::Win32::Security::{CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE};
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    /// `OICI`: inherited by both child objects (files) and containers (directories).
    const OBJECT_AND_CONTAINER_INHERIT: u8 = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;

    let user_sid = current_user_sid_bytes()?;
    read_dacl(dir, |control, dacl| {
        if dacl.is_null() || control & SE_DACL_PRESENT == 0 || control & SE_DACL_PROTECTED == 0 {
            return false;
        }
        // SAFETY: `dacl` is present and non-null per the guard above.
        if unsafe { (*dacl).AceCount } != 1 {
            return false;
        }
        let Some(ace) = allow_ace_at(dacl, 0) else {
            return false;
        };
        // SAFETY: `ace.sid` is the ACE's in-place SID inside the live DACL and
        // `user_sid` is our owned copy of the current user's SID; EqualSid only
        // reads both.
        let same_account =
            unsafe { EqualSid(ace.sid, user_sid.as_ptr() as *mut core::ffi::c_void) != 0 };
        ace.flags == OBJECT_AND_CONTAINER_INHERIT && ace.mask == FILE_ALL_ACCESS && same_account
    })
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
    let _ = unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
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
