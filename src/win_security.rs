//! Shared Windows security-FFI primitives for the owner-only control plane.
//!
//! Two call sites build a **protected DACL** that restricts a securable object to the
//! current user alone: the per-user run registry directory ([`crate::registry`]) and
//! the control-plane named pipe ([`crate::control`]). Both do the same unsafe dance —
//! encode an SDDL string as wide text, run
//! `ConvertStringSecurityDescriptorToSecurityDescriptorW` over it, use the resulting
//! `LocalAlloc`'d descriptor, and free it with `LocalFree`. This module owns that one
//! FFI glue so a fix lands in both places at once:
//!
//! - [`to_wide`] — the NUL-terminated UTF-16 encoding both sites need for the wide
//!   Win32 APIs.
//! - [`SecurityDescriptor`] — a RAII wrapper **parameterised on the caller's SDDL**
//!   that owns the converted descriptor and frees it exactly once on drop.
//!
//! The **access policy** stays entirely at each call site: the registry passes an
//! *inheritable* `D:P(A;OICI;FA;;;<sid>)` (its directory has child objects the ACL
//! must flow to), the control pipe a *non-inheritable* `D:P(A;;FA;;;<sid>)` (a pipe
//! has none). This module deliberately unifies only the conversion/lifetime glue, not
//! the SDDL, so the two policies remain distinct.

use std::io;

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};

/// Encode a string as a NUL-terminated UTF-16 buffer for the wide Win32 APIs.
pub fn to_wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// A `LocalAlloc`'d Win32 security descriptor built from a caller-supplied SDDL
/// string, freed with `LocalFree` on drop.
///
/// [`SecurityDescriptor::from_sddl`] converts the given SDDL string with
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW` and takes ownership of the
/// resulting descriptor; [`Drop`] frees it exactly once. The SDDL — and therefore the
/// access policy it encodes — is entirely the caller's: the registry hands in an
/// inheritable `D:P(A;OICI;FA;;;<sid>)` for its directory, the control pipe a
/// non-inheritable `D:P(A;;FA;;;<sid>)`. This type unifies the unsafe
/// conversion/free, never the policy.
pub struct SecurityDescriptor {
    descriptor: *mut core::ffi::c_void,
}

// SAFETY: `descriptor` is a heap security descriptor (LocalAlloc) with no thread
// affinity — moving ownership across threads is sound, and it is freed exactly once,
// in `Drop`. `Send` lets the control server (which stores one for the run's life)
// ride the async runtime.
unsafe impl Send for SecurityDescriptor {}

impl SecurityDescriptor {
    /// Build a security descriptor from an SDDL string (e.g. `D:P(A;;FA;;;<sid>)`).
    /// The returned value owns the `LocalAlloc`'d descriptor and frees it on drop, so
    /// the caller never pairs a manual `LocalFree` with this call.
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = to_wide(sddl);
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 SDDL string that outlives
        // this call; on success `descriptor` receives a LocalAlloc'd security
        // descriptor whose sole owner is the `Self` returned below — it is freed
        // exactly once, in `Drop`, not by the caller.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    /// The raw descriptor pointer, for a Win32 API that only *borrows* it — e.g.
    /// `SECURITY_ATTRIBUTES.lpSecurityDescriptor`, or `GetSecurityDescriptorDacl`.
    /// The pointer is valid only while `self` is alive; the caller must not free it
    /// (drop does that) nor let it outlive `self`.
    pub fn as_ptr(&self) -> *mut core::ffi::c_void {
        self.descriptor
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `descriptor` came from
        // ConvertStringSecurityDescriptorToSecurityDescriptorW (LocalAlloc'd) and is
        // owned solely by this value, so freeing it exactly once here is sound.
        unsafe { LocalFree(self.descriptor as HLOCAL) };
    }
}
