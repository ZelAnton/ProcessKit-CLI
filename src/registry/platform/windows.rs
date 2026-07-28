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
    let ok =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
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
        let (ace_type, ace_sid) = unsafe { ((*ace).Header.AceType, &raw const (*ace).SidStart) };
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
