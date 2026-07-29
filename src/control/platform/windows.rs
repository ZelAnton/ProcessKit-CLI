//! Windows transport: a named pipe with an owner-only protected DACL.

use core::ffi::c_void;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

use crate::win_security::SecurityDescriptor;

use super::{
    ControlCommandSink, Infallible, PIPE_ENDPOINT_PREFIX, SnapshotSource, handle_connection, io,
    unique_token,
};

/// The connected client stream type on this platform — a named-pipe client. Named
/// so the platform-agnostic client can hold it without a `cfg`.
pub type Stream = NamedPipeClient;

/// A run's bound control transport: an owner-only named pipe. Holds the owner-only
/// security descriptor (kept alive for every instance it creates) and the first
/// pipe instance created at bind, so the name exists the moment the endpoint is
/// published.
pub struct ControlServer {
    endpoint: String,
    security: SecurityDescriptor,
    first: Option<NamedPipeServer>,
}

impl ControlServer {
    /// Create the pipe name and its first instance, restricted to the current
    /// user. No directory is taken — the pipe lives in the kernel namespace, not
    /// the filesystem.
    pub fn bind() -> io::Result<Self> {
        let endpoint = format!("{PIPE_ENDPOINT_PREFIX}{}", unique_token());
        let security = owner_only_descriptor()?;
        let first = create_instance(&endpoint, &security, true)?;
        Ok(Self {
            endpoint,
            security,
            first: Some(first),
        })
    }

    /// The pipe name a client opens.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Accept and serve clients forever (never returns — see [`super::serve`]).
    pub async fn serve(
        mut self,
        source: &SnapshotSource<'_>,
        commands: &ControlCommandSink,
    ) -> Infallible {
        // Move out the instance created at bind; thereafter each iteration stands
        // up the *next* instance before servicing the current one, so the pipe
        // always has a waiting instance and no client hits a momentary "no pipe".
        let mut server = self
            .first
            .take()
            .expect("the first pipe instance is created at bind");
        loop {
            if server.connect().await.is_err() {
                // Recreate and retry; if even that fails, we can no longer serve —
                // park forever (diverges) so the run's own path is unaffected.
                server = match create_instance(&self.endpoint, &self.security, false) {
                    Ok(next) => next,
                    Err(_) => park_forever().await,
                };
                continue;
            }
            let next = match create_instance(&self.endpoint, &self.security, false) {
                Ok(next) => next,
                // Cannot stand up the next instance: serve this last client, then
                // park forever (no more can be accepted, but the run is fine).
                Err(_) => {
                    handle_connection(server, source, commands).await;
                    park_forever().await
                }
            };
            let connected = std::mem::replace(&mut server, next);
            handle_connection(connected, source, commands).await;
        }
    }
}

/// Park forever, **diverging** (`!`): unlike an `Infallible`-typed future, a `!`
/// return makes the borrow checker treat the code after a call as unreachable, so
/// the accept loop above can drop a moved pipe instance on an unrecoverable error
/// without appearing to reuse it on the next iteration.
async fn park_forever() -> ! {
    match std::future::pending::<Infallible>().await {}
}

/// Create one pipe instance guarded by the owner-only security descriptor.
/// `first` sets `FILE_FLAG_FIRST_PIPE_INSTANCE` so a squatter cannot pre-own the
/// name; remote clients are rejected (local-only channel).
fn create_instance(
    endpoint: &str,
    security: &SecurityDescriptor,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` points at a well-formed SECURITY_ATTRIBUTES whose
    // owner-only descriptor (`security`) outlives this call; tokio passes it
    // straight to CreateNamedPipe.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                endpoint,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
    }
}

/// Build the pipe's owner-only security descriptor: a **P**rotected DACL with a
/// single allow-**F**ull-**A**ccess ACE for the current user and nothing else
/// (`D:P(A;;FA;;;<current-user-SID>)`). **No inheritance flags** — deliberately
/// unlike the registry *directory*'s inheritable `D:P(A;OICI;FA;;;<sid>)`, because
/// a named pipe has no child objects for an ACE to be inherited by. Built from the
/// same SID the registry restricts to (see [`crate::registry::current_user_sid_string`]),
/// so the pipe and the registry are locked to one identity. The returned
/// [`SecurityDescriptor`] owns the `LocalAlloc`'d descriptor and frees it on drop —
/// the shared unsafe conversion/free lives in [`crate::win_security`], this site
/// only owns the SDDL policy.
fn owner_only_descriptor() -> io::Result<SecurityDescriptor> {
    let sid = crate::registry::current_user_sid_string()?;
    SecurityDescriptor::from_sddl(&format!("D:P(A;;FA;;;{sid})"))
}

/// Connect to a runner's named-pipe endpoint. A pipe whose every instance is busy
/// serving other clients returns `ERROR_PIPE_BUSY`; retry briefly (the caller's
/// connect deadline bounds the total wait).
pub async fn connect(endpoint: &str) -> io::Result<NamedPipeClient> {
    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return Ok(client),
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err) => return Err(err),
        }
    }
}
