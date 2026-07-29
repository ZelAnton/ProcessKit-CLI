//! Unix transport: a `0600` socket inside a short per-run `0700` directory.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

use tokio::net::{UnixListener, UnixStream};

use super::{
    ControlCommandSink, Infallible, SOCKET_DIR_PREFIX, SOCKET_FILE_NAME, SnapshotSource,
    handle_connection, io, socket_base_dirs, unique_token,
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
                Ok((stream, _addr)) => handle_connection(stream, source, commands).await,
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
