//! Unix transport: a `0600` socket inside a short per-run `0700` directory.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

use tokio::net::{UnixListener, UnixStream};

use super::{
    ControlCommandSink, Infallible, PeerIdentity, SOCKET_DIR_PREFIX, SOCKET_FILE_NAME,
    SnapshotSource, handle_connection, io, socket_base_dirs, unique_token,
};

/// The connected client stream type on this platform — a unix domain socket
/// stream. Named so the platform-agnostic client can hold it without a `cfg`.
pub type Stream = UnixStream;

/// A run's bound control transport: a listening unix socket. Holds the socket
/// path so it can be removed on a clean teardown (when this is dropped).
pub struct ControlServer {
    listener: UnixListener,
    dir: PathBuf,
    path: PathBuf,
    endpoint: String,
}

impl ControlServer {
    /// Bind a fresh socket in a short owner-only directory of its own —
    /// deliberately not the registry directory: test/project paths routinely
    /// exceed macOS's `sockaddr_un::sun_path` limit before a socket filename is
    /// appended.
    pub fn bind() -> io::Result<Self> {
        let dir = create_private_socket_dir()?;
        let path = dir.join(SOCKET_FILE_NAME);
        let endpoint = match path.to_str() {
            Some(endpoint) => endpoint.to_string(),
            None => {
                let _ = std::fs::remove_dir(&dir);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the control socket path is not valid UTF-8",
                ));
            }
        };
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                let _ = std::fs::remove_dir(&dir);
                return Err(err);
            }
        };
        // Restrict the socket itself to the owner (connect needs write on the
        // socket + search on the directory). The directory was atomically created
        // as 0700, so it already gates the chmod window.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        Ok(Self {
            listener,
            dir,
            path,
            endpoint,
        })
    }

    /// The socket path a client connects to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Accept and serve clients forever (never returns — see [`super::serve`]).
    pub async fn serve(
        self,
        source: &SnapshotSource<'_>,
        commands: &ControlCommandSink,
    ) -> Infallible {
        loop {
            match self.listener.accept().await {
                Ok((stream, _addr)) => {
                    // Read the peer's identity off the accepted socket *before*
                    // the connection is served, while it is unquestionably still
                    // open: this is what makes the answer about the process on
                    // the other end of *this* socket rather than about whoever
                    // happens to hold that pid number later (see
                    // [`peer_identity`]).
                    let peer = peer_identity(&stream);
                    handle_connection(stream, peer, source, commands).await
                }
                // A transient accept error (e.g. a fd limit) must not spin the
                // loop; pause briefly, then keep serving.
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        // Clean teardown removes the socket file (best-effort). An abrupt death
        // skips this and strands the socket, exactly like the registry record/lock
        // — a client detects that run as stale via the registry and never
        // connects, and `Registry::prune` reaps this directory along with that
        // record's two files when it confirms the entry stale (T-207), so the
        // leftover does not accumulate.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Whether *this target* is one where a unix domain socket is guaranteed to name
/// the connecting client's process — the compile-time half of the capability
/// [`super::PEER_IDENTITY_SUPPORTED`] advertises through `probe`.
///
/// The list is the set of targets whose `SO_PEERCRED`-equivalent **unconditionally**
/// carries a pid, verified against `tokio`'s own per-target implementations
/// (`tokio::net::unix::ucred`, 1.53), not assumed from the Linux spelling:
///
/// - **Linux / Android / OpenBSD** — `getsockopt(SOL_SOCKET, SO_PEERCRED)` filling a
///   `ucred` (`sockpeercred` on OpenBSD), whose `pid` field the kernel sets.
/// - **macOS / iOS** — `SO_PEERCRED` does **not** exist here, and the portable
///   `getpeereid(3)` this platform does have returns only the effective uid/gid,
///   which is not an identity a membership check can use. The pid comes from a
///   second, Darwin-specific call: `getsockopt(SOL_LOCAL, LOCAL_PEEREPID)`, which
///   answers with the peer's **effective** pid. `tokio` issues both (`LOCAL_PEEREPID`
///   for the pid, `getpeereid` for uid/gid), so `UCred::pid()` is `Some` here exactly
///   as it is on Linux — the mechanism differs, the guarantee does not.
/// - **NetBSD** — `getsockopt(SOL_SOCKET, LOCAL_PEEREID)` filling a `unpcbid`
///   (`unp_pid`).
/// - **Solaris / illumos** — `getpeerucred(3C)` plus `ucred_getpid`.
///
/// Deliberately **absent**, and each for a checked reason rather than an oversight:
///
/// - **FreeBSD** — `LOCAL_PEERCRED` yields an `xucred` whose `cr_pid` is populated
///   only since FreeBSD 13, so whether a pid arrives is a property of the *running
///   kernel*, not of the target this was compiled for. A compile-time claim would
///   therefore be an over-claim on an older kernel, and this constant is a
///   guarantee: it under-claims instead. `attest` still works there whenever the
///   kernel does supply one — see [`super::PEER_IDENTITY_SUPPORTED`] for why the
///   advertisement and the runtime path are deliberately allowed to differ in that
///   direction and never the other.
/// - **DragonFly BSD / AIX / QNX Neutrino** — only `getpeereid(3)`: uid and gid, no
///   pid at all.
/// - Every other unix target — no pid path in `tokio` at all (`UCred::pid()` is
///   hard-wired `None`), or one this project has not verified.
pub const PEER_IDENTITY_SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "ios",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos",
));

/// The kernel's answer to "which process is on the other end of this socket?",
/// read from the connected socket itself.
///
/// Nothing the client sent is involved: the pid comes from the kernel's own
/// record of who connected (see [`PEER_IDENTITY_SUPPORTED`] for the per-target
/// system call), so a client cannot name a process other than itself. It is read
/// while the connection is open, which is what keeps pid reuse out of the answer:
/// a process that owns an open socket has not exited, so its pid cannot yet have
/// been recycled onto some other process.
///
/// Anything short of a positive pid is [`PeerIdentity::Unavailable`] — never a
/// fabricated or guessed identity:
///
/// - the call failed (a target whose `getpeereid`-only path errors, an unusual
///   socket state);
/// - the target has no pid path at all, so `tokio` reports `None`;
/// - the pid is `0` or negative. On Linux `SO_PEERCRED` translates the peer's pid
///   into the *reader's* pid namespace and reports `0` when it has no
///   representation there — precisely the case where a number would be meaningless
///   to compare against this container's members — and no real userland peer is
///   pid `0` on any of these targets anyway.
pub fn peer_identity(stream: &UnixStream) -> PeerIdentity {
    let Ok(credentials) = stream.peer_cred() else {
        return PeerIdentity::Unavailable;
    };
    match credentials.pid() {
        Some(pid) if pid > 0 => u32::try_from(pid)
            .map(PeerIdentity::Pid)
            .unwrap_or(PeerIdentity::Unavailable),
        _ => PeerIdentity::Unavailable,
    }
}

/// Atomically reserve a short owner-only directory. A pre-created path is never
/// trusted: `create` must succeed for this process, otherwise a fresh unique token
/// is tried. `/tmp` (the first of [`socket_base_dirs`]) keeps the advertised
/// socket comfortably below SUN_LEN even when the registry lives under a deeply
/// nested CI workspace.
///
/// The name is built from [`SOCKET_DIR_PREFIX`] and the base list from
/// [`socket_base_dirs`] — the same two the registry's reaper validates a published
/// endpoint against before deleting anything (T-207), so the shape can never drift
/// between the side that creates it and the side that cleans it up.
fn create_private_socket_dir() -> io::Result<PathBuf> {
    let mut last_error = None;
    for base in socket_base_dirs() {
        if !base.is_dir() {
            continue;
        }
        for _ in 0..16 {
            let dir = base.join(format!("{SOCKET_DIR_PREFIX}{}", unique_token()));
            match DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    last_error = Some(err);
                }
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no usable temporary directory for the control socket",
        )
    }))
}

/// Connect to a runner's unix socket endpoint.
pub async fn connect(endpoint: &str) -> io::Result<UnixStream> {
    UnixStream::connect(endpoint).await
}
