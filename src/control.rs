//! The live-run control plane: a per-run local transport and the `inspect` client.
//!
//! ProcessKit-cli's control plane lives in the *live* `run` process, not in named
//! kernel objects (`AGENTS.md`, "The control plane lives in the live runner
//! process"). This module is the first working brick of that plane on top of the
//! run registry ([`crate::registry`]):
//!
//! - **Server side (inside `run`).** Each run stands up a local IPC transport — a
//!   **unix domain socket** on unix, a **named pipe** on Windows — restricted to the
//!   current user, and publishes its address in the run's registry record
//!   ([`registry::Record::endpoint`]). The server is served *concurrently* with the
//!   child's output pump on the same runtime (see [`serve`]); it never blocks the
//!   happy path — a live run that no one inspects pays only an idle accept, and the
//!   run's exit and teardown do not wait on any control client.
//! - **Client side (`inspect`).** [`inspect`] finds the live runner through the
//!   registry (matching `run_id`, never a PID), connects to its endpoint, and prints
//!   a machine-readable [`Snapshot`] of the run: its id, containment mechanism, root
//!   PID, container members (PID-only, the scope the public `processkit` API exposes
//!   today — the same shape as the JSONL `members_snapshot`), and start time.
//!
//! ## Owner-only transport
//!
//! An endpoint is a control channel, so it must not be reachable by other local
//! users. Access is restricted to the current user, mirroring the owner-only
//! registry:
//!
//! - **Unix:** the socket is created in a short, per-run `0700` directory below
//!   `/tmp` (falling back to the platform temp directory) and its own mode is
//!   tightened to `0600`. Keeping it separate from a potentially long registry path
//!   stays within `sockaddr_un::sun_path` on macOS without weakening owner isolation.
//! - **Windows:** the pipe is created with a **protected** DACL granting full access
//!   to the current user alone (`D:P(A;;FA;;;<current-user-SID>)`, built from the
//!   same SID the registry restricts to — see
//!   [`registry::current_user_sid_string`]), and rejects remote clients.
//!
//! ## Dead runner / stale entry — a distinguishable result, never a hang
//!
//! A client can lose the runner two ways, and both are reported as the reserved
//! [`exit::CONTROL`] code (103, "could not reach the target run" — see
//! `docs/exit-codes.md`) with an explanatory message, **never** a generic error and
//! **never** a hang:
//!
//! - **Stale registry entry.** The runner died abruptly, leaving its record behind;
//!   the released liveness lock makes the entry [`registry::Health::Stale`]
//!   ([`registry::Registry::entries`], T-007). The client detects this *before*
//!   connecting and reports the run as gone.
//! - **Died mid-conversation.** The entry read live, but the runner exited between
//!   the liveness probe and the reply: the connect fails, or the connection closes
//!   before a complete response arrives. Every socket/pipe wait is bounded by a
//!   deadline, so a runner that accepted but never answers cannot wedge the client
//!   either.
//!
//! ## Wire protocol
//!
//! Line-oriented and deliberately tiny. A client writes one request verb line
//! (`inspect\n`; an empty line is also treated as `inspect`) and reads back one JSON
//! line, then the server closes the connection. Today `inspect` is the only verb;
//! `cancel`/`kill` (T-009) add verbs to the same framing without reshaping it.

use std::convert::Infallible;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, split};

use crate::events::{self, Member};
use crate::exit::{self, RunnerError};
use crate::registry::{self, Health};

pub use imp::ControlServer;

/// Control-plane snapshot format version. Independent of the JSONL event
/// [`schema_version`](crate::events::SCHEMA_VERSION) and the
/// [`registry_version`](crate::registry::REGISTRY_VERSION): the `inspect` response is
/// the control plane's own private client/runner contract, so it versions on its own
/// axis.
pub const SNAPSHOT_VERSION: u32 = 1;

/// The only request verb today. An empty request line is treated as this too, so a
/// bare connect-and-read probe still gets a snapshot.
const INSPECT_REQUEST: &str = "inspect";

/// How long the client waits to *connect* to a runner's endpoint before giving up —
/// a runner that has just died cannot make the client hang.
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);

/// How long the client waits for the whole request/response exchange once connected.
/// A live-but-wedged runner is bounded by this instead of hanging the client.
const CONVERSATION_DEADLINE: Duration = Duration::from_secs(5);

/// How long the *server* spends on a single client exchange before dropping it, so a
/// client that connects and then stalls cannot wedge the accept loop for other
/// inspect clients (the run's own path is already independent of this).
const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);

/// The machine-readable state `inspect` prints: what a control-plane client can learn
/// about a live run. `Serialize` on the server side, `Deserialize` on the client side
/// (which parses the reply back before printing it, so a truncated/garbled response
/// from a runner dying mid-write is caught rather than echoed).
#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// Snapshot format version ([`SNAPSHOT_VERSION`]).
    pub snapshot_version: u32,
    /// The run's identifier — the key the client matched in the registry. Not a PID.
    pub run_id: String,
    /// Containment mechanism: `job_object` | `cgroup_v2` | `process_group` (same
    /// vocabulary as the JSONL `run_started`, see [`events::mechanism_str`]).
    pub mechanism: String,
    /// The root child's PID, or `null` if the backend exposed none.
    pub root_pid: Option<u32>,
    /// Run start time, RFC 3339 UTC with millisecond precision (same formatter as the
    /// JSONL events and the registry record).
    pub started_at: String,
    /// A point-in-time snapshot of the container's members, PID-only — the scope the
    /// public `processkit` API exposes today, mirroring the JSONL `members_snapshot`
    /// (the enriched fields stay reserved-`null`). Queried at request time, so it
    /// reflects the container's composition *when inspected*, not at start.
    pub members: Vec<Member>,
}

/// The error line a server sends for an unrecognized request verb. The `inspect`
/// client never asks for anything else, so it only ever sees a [`Snapshot`]; this
/// exists so a future/foreign client gets a structured answer rather than silence.
#[derive(Debug, Serialize)]
struct ErrorResponse<'a> {
    error: &'a str,
}

/// The live facts the control server answers an `inspect` with. It borrows the run's
/// state (rather than owning a copy) so `members` is queried *at request time* — the
/// snapshot reflects the container's current composition, not a start-of-run census.
pub struct SnapshotSource<'a> {
    run_id: &'a str,
    mechanism: &'static str,
    root_pid: Option<u32>,
    started: SystemTime,
    /// Produces the current PID-only member list on demand. Kept as a borrowed
    /// closure so this module never has to depend on `processkit` directly — `run`
    /// supplies one that queries the owning `ProcessGroup`.
    members: &'a (dyn Fn() -> Vec<Member> + 'a),
}

impl<'a> SnapshotSource<'a> {
    /// Assemble a snapshot source from the run's settled facts and a live members
    /// provider.
    pub fn new(
        run_id: &'a str,
        mechanism: &'static str,
        root_pid: Option<u32>,
        started: SystemTime,
        members: &'a (dyn Fn() -> Vec<Member> + 'a),
    ) -> Self {
        Self {
            run_id,
            mechanism,
            root_pid,
            started,
            members,
        }
    }

    /// Build the current [`Snapshot`], querying members live.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            snapshot_version: SNAPSHOT_VERSION,
            run_id: self.run_id.to_string(),
            mechanism: self.mechanism.to_string(),
            root_pid: self.root_pid,
            started_at: events::format_rfc3339_utc(self.started),
            members: (self.members)(),
        }
    }
}

/// Stand up the local control transport for a run, bound in `dir` (the owner-only
/// registry directory). **Best-effort:** a failure warns on stderr and returns
/// `None` — the control plane is discovery infrastructure, and losing it only makes
/// this run un-inspectable, never costs the child its exit-code fidelity (the same
/// degradation as the registry itself, `AGENTS.md`, "Exit-code fidelity").
pub fn open_server(dir: &Path) -> Option<ControlServer> {
    match ControlServer::bind(dir) {
        Ok(server) => Some(server),
        Err(err) => {
            eprintln!("processkit-cli: warning: could not open the control transport: {err}");
            None
        }
    }
}

/// Serve the control transport for the run's whole life, concurrently with the
/// output pump. Its output type is [`Infallible`]: it **never resolves** (it loops
/// accepting clients forever, and parks on an unrecoverable transport error rather
/// than returning), so a caller can drop it in a `select!` alongside the child's exit
/// without it ever winning the race — the run ends when the *child* does, and this
/// future is dropped (tearing the transport down) at that point. With no transport it
/// parks forever, so the caller's race is unaffected.
pub async fn serve(server: Option<ControlServer>, source: &SnapshotSource<'_>) -> Infallible {
    match server {
        Some(server) => server.serve(source).await,
        None => std::future::pending().await,
    }
}

/// Handle one accepted connection: read the request verb, write the JSON response,
/// close. Bounded by [`CONNECTION_DEADLINE`] so a client that connects and stalls
/// cannot wedge the accept loop. Errors are swallowed — a broken client connection is
/// never the run's problem.
async fn handle_connection<S>(stream: S, source: &SnapshotSource<'_>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(CONNECTION_DEADLINE, serve_one(stream, source)).await;
}

/// The request/response exchange for one connection.
async fn serve_one<S>(stream: S, source: &SnapshotSource<'_>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = split(stream);
    let mut reader = BufReader::new(read_half);
    let mut request = String::new();
    reader.read_line(&mut request).await?;
    let response = match request.trim() {
        INSPECT_REQUEST | "" => serialize_snapshot(&source.snapshot()),
        other => serialize_error(&format!("unknown control request `{other}`")),
    };
    write_half.write_all(response.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
    // Signal end-of-response; best-effort (some transports have no half-close).
    let _ = write_half.shutdown().await;
    Ok(())
}

/// Serialize a snapshot for the wire. A struct of owned strings and numbers cannot
/// fail to serialize; the fallback is defensive only.
fn serialize_snapshot(snapshot: &Snapshot) -> String {
    serde_json::to_string(snapshot)
        .unwrap_or_else(|_| String::from(r#"{"error":"could not render the snapshot"}"#))
}

/// Serialize an error response for an unrecognized request.
fn serialize_error(message: &str) -> String {
    serde_json::to_string(&ErrorResponse { error: message })
        .unwrap_or_else(|_| String::from(r#"{"error":"control error"}"#))
}

/// Client entry for `inspect --run-id <id> --json`: find the live runner through the
/// registry, ask it for a snapshot, and print it. Runs on its own small current-thread
/// runtime (the transport client is async). A run that cannot be reached — no such id,
/// a stale entry, a dead-mid-conversation runner — returns a [`exit::CONTROL`] error.
pub fn inspect(run_id: &str) -> Result<(), RunnerError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            RunnerError::new(
                exit::INTERNAL,
                format!("could not start the async runtime: {err}"),
            )
        })?;
    runtime.block_on(inspect_async(run_id))
}

/// The async body of [`inspect`]: registry lookup, connect, converse, print.
async fn inspect_async(run_id: &str) -> Result<(), RunnerError> {
    let registry = registry::Registry::open().map_err(|err| {
        unreachable_run(run_id, format!("could not open the run registry: {err}"))
    })?;
    let entries = registry.entries().map_err(|err| {
        unreachable_run(run_id, format!("could not read the run registry: {err}"))
    })?;

    let matches: Vec<registry::Entry> = entries
        .into_iter()
        .filter(|entry| entry.record.run_id == run_id)
        .collect();
    if matches.is_empty() {
        return Err(unreachable_run(
            run_id,
            "no run with that id is registered".to_string(),
        ));
    }

    // Reach only a live entry that actually advertises an endpoint. If none does, say
    // *why* — the run is gone (stale) or predates the transport (live, no endpoint) —
    // rather than a generic failure.
    let Some(entry) = matches
        .iter()
        .find(|entry| entry.health == Health::Live && entry.record.endpoint.is_some())
    else {
        if matches.iter().any(|entry| entry.health == Health::Live) {
            return Err(unreachable_run(
                run_id,
                "the run is live but exposes no control endpoint".to_string(),
            ));
        }
        return Err(unreachable_run(
            run_id,
            "its registry entry is stale — the runner is gone (it exited without cleaning up)"
                .to_string(),
        ));
    };
    let endpoint = entry
        .record
        .endpoint
        .as_deref()
        .expect("filtered for an entry whose endpoint is Some");

    // Connect under a deadline: a runner that died between the liveness probe and now
    // fails fast here instead of hanging the client.
    let stream = tokio::time::timeout(CONNECT_DEADLINE, imp::connect(endpoint))
        .await
        .map_err(|_| {
            unreachable_run(
                run_id,
                "timed out connecting to the live runner".to_string(),
            )
        })?
        .map_err(|err| {
            unreachable_run(
                run_id,
                format!("could not reach the live runner (it may have just exited): {err}"),
            )
        })?;

    // Converse under a deadline: a runner that died mid-write, or accepted but never
    // answers, is bounded here — a distinguishable CONTROL result, not a hang.
    let snapshot = tokio::time::timeout(CONVERSATION_DEADLINE, converse(stream))
        .await
        .map_err(|_| unreachable_run(run_id, "the runner did not answer in time".to_string()))?
        .map_err(|err| unreachable_run(run_id, err.to_string()))?;

    let json = serde_json::to_string(&snapshot).map_err(|err| {
        RunnerError::new(
            exit::INTERNAL,
            format!("could not render the inspect snapshot: {err}"),
        )
    })?;
    println!("{json}");
    Ok(())
}

/// Ask the connected runner for a snapshot and parse its reply. A closed connection
/// before a complete line (runner died mid-conversation) or an unparseable line
/// surfaces as an error the caller maps to [`exit::CONTROL`].
async fn converse<S>(stream: S) -> io::Result<Snapshot>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = split(stream);
    write_half.write_all(INSPECT_REQUEST.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the runner closed the connection before answering (it may have just exited)",
        ));
    }
    serde_json::from_str::<Snapshot>(line.trim()).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the runner sent an unreadable response: {err}"),
        )
    })
}

/// A "cannot reach the target run" error carrying the reserved [`exit::CONTROL`] code
/// and a message naming the run and the reason.
fn unreachable_run(run_id: &str, detail: String) -> RunnerError {
    RunnerError::new(
        exit::CONTROL,
        format!("cannot inspect run `{run_id}`: {detail}"),
    )
}

/// A unique, PID-free-collision-proof token for a transport endpoint name: the
/// process id, the current time in nanoseconds, and a per-process counter. Used to
/// name the unix socket / windows pipe so concurrent runs never collide.
fn unique_token() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos:x}-{sequence:x}", std::process::id())
}

#[cfg(unix)]
mod imp {
    //! Unix transport: a `0600` socket inside a short per-run `0700` directory.

    use std::fs::DirBuilder;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    use super::{Infallible, SnapshotSource, handle_connection, io, unique_token};

    /// A run's bound control transport: a listening unix socket. Holds the socket
    /// path so it can be removed on a clean teardown (when this is dropped).
    pub struct ControlServer {
        listener: UnixListener,
        dir: PathBuf,
        path: PathBuf,
        endpoint: String,
    }

    impl ControlServer {
        /// Bind a fresh socket in a short owner-only directory. The registry path is
        /// deliberately not used: test/project paths routinely exceed macOS's
        /// `sockaddr_un::sun_path` limit before a socket filename is appended.
        pub fn bind(_registry_dir: &Path) -> io::Result<Self> {
            let dir = create_private_socket_dir()?;
            let path = dir.join("c.sock");
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
        pub async fn serve(self, source: &SnapshotSource<'_>) -> Infallible {
            loop {
                match self.listener.accept().await {
                    Ok((stream, _addr)) => handle_connection(stream, source).await,
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
            // skips this and leaks the socket, exactly like the registry record/lock —
            // a client detects that run as stale via the registry and never connects.
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    /// Atomically reserve a short owner-only directory. A pre-created path is never
    /// trusted: `create` must succeed for this process, otherwise a fresh unique token
    /// is tried. `/tmp` keeps the advertised socket comfortably below SUN_LEN even
    /// when the registry lives under a deeply nested CI workspace.
    fn create_private_socket_dir() -> io::Result<PathBuf> {
        let mut bases = vec![PathBuf::from("/tmp")];
        let platform_temp = std::env::temp_dir();
        if platform_temp != bases[0] {
            bases.push(platform_temp);
        }
        let mut last_error = None;
        for base in bases {
            if !base.is_dir() {
                continue;
            }
            for _ in 0..16 {
                let dir = base.join(format!("pkc-{}", unique_token()));
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
}

#[cfg(windows)]
mod imp {
    //! Windows transport: a named pipe with an owner-only protected DACL.

    use core::ffi::c_void;
    use std::path::Path;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    use super::{Infallible, SnapshotSource, handle_connection, io, unique_token};

    /// A run's bound control transport: an owner-only named pipe. Holds the owner-only
    /// security descriptor (kept alive for every instance it creates) and the first
    /// pipe instance created at bind, so the name exists the moment the endpoint is
    /// published.
    pub struct ControlServer {
        endpoint: String,
        security: OwnerOnlySecurityDescriptor,
        first: Option<NamedPipeServer>,
    }

    impl ControlServer {
        /// Create the pipe name and its first instance, restricted to the current
        /// user. `dir` is unused on Windows — the pipe lives in the kernel namespace,
        /// not the filesystem — but taken for a uniform signature with the unix side.
        pub fn bind(_dir: &Path) -> io::Result<Self> {
            let endpoint = format!(r"\\.\pipe\processkit-cli-{}", unique_token());
            let security = OwnerOnlySecurityDescriptor::new()?;
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
        pub async fn serve(mut self, source: &SnapshotSource<'_>) -> Infallible {
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
                        handle_connection(server, source).await;
                        park_forever().await
                    }
                };
                let connected = std::mem::replace(&mut server, next);
                handle_connection(connected, source).await;
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
        security: &OwnerOnlySecurityDescriptor,
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

    /// An owner-only security descriptor (`D:P(A;;FA;;;<current-user-SID>)`): a
    /// **P**rotected DACL with a single allow-**F**ull-**A**ccess ACE for the current
    /// user and nothing else. Built from the same SID the registry restricts to, so
    /// the pipe and the registry are locked to one identity. Frees the LocalAlloc'd
    /// descriptor on drop.
    struct OwnerOnlySecurityDescriptor {
        descriptor: *mut c_void,
    }

    // SAFETY: `descriptor` is a heap security descriptor (LocalAlloc) with no thread
    // affinity — moving ownership across threads is sound, and it is freed exactly
    // once, in `Drop`. `Send` lets the control server ride the async runtime.
    unsafe impl Send for OwnerOnlySecurityDescriptor {}

    impl OwnerOnlySecurityDescriptor {
        fn new() -> io::Result<Self> {
            let sid = crate::registry::current_user_sid_string()?;
            let sddl = to_wide(&format!("D:P(A;;FA;;;{sid})"));
            let mut descriptor: *mut c_void = core::ptr::null_mut();
            // SAFETY: `sddl` is a valid NUL-terminated UTF-16 SDDL string; on success
            // `descriptor` receives a LocalAlloc'd security descriptor freed in Drop.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    core::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { descriptor })
        }

        fn as_ptr(&self) -> *mut c_void {
            self.descriptor
        }
    }

    impl Drop for OwnerOnlySecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: `descriptor` came from ConvertStringSecurityDescriptorToSecurity
            // DescriptorW (LocalAlloc'd), freed exactly once here.
            unsafe { LocalFree(self.descriptor as HLOCAL) };
        }
    }

    /// Encode a string as a NUL-terminated UTF-16 buffer for the wide Win32 APIs.
    fn to_wide(value: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot round-trips through JSON: the client parses exactly what the server
    /// serialized, members included. This is the wire contract `inspect` depends on.
    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = Snapshot {
            snapshot_version: SNAPSHOT_VERSION,
            run_id: "run-42".to_string(),
            mechanism: "job_object".to_string(),
            root_pid: Some(4242),
            started_at: "2026-07-21T00:00:00.000Z".to_string(),
            members: vec![Member::from_pid(4242), Member::from_pid(4243)],
        };
        let line = serialize_snapshot(&snapshot);
        let parsed: Snapshot = serde_json::from_str(&line).expect("a snapshot line parses back");
        assert_eq!(parsed.snapshot_version, SNAPSHOT_VERSION);
        assert_eq!(parsed.run_id, "run-42");
        assert_eq!(parsed.mechanism, "job_object");
        assert_eq!(parsed.root_pid, Some(4242));
        assert_eq!(parsed.members.len(), 2);
        assert_eq!(parsed.members[0].pid, 4242);
        // The enriched member fields stay reserved (null), like the JSONL snapshot.
        assert!(parsed.members[0].ppid.is_none());
    }

    /// The source builds a snapshot from its facts and queries members live each time.
    #[test]
    fn snapshot_source_queries_members_live() {
        use std::cell::Cell;
        let calls = Cell::new(0u32);
        let members = || {
            calls.set(calls.get() + 1);
            vec![Member::from_pid(7)]
        };
        let started = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let source = SnapshotSource::new("run-x", "process_group", Some(7), started, &members);

        let first = source.snapshot();
        assert_eq!(first.run_id, "run-x");
        assert_eq!(first.mechanism, "process_group");
        assert_eq!(first.root_pid, Some(7));
        assert_eq!(first.members.len(), 1);
        assert_eq!(
            first.started_at,
            events::format_rfc3339_utc(started),
            "the snapshot stamps the run's start time with the shared formatter"
        );
        // A second snapshot re-queries members — it is a live view, not a cached one.
        let _ = source.snapshot();
        assert_eq!(calls.get(), 2, "members are queried on every snapshot");
    }

    /// An unrecognized request gets a structured error line, never a snapshot.
    #[test]
    fn unknown_request_serializes_a_structured_error() {
        let line = serialize_error("unknown control request `cancel`");
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert!(
            value.get("error").and_then(|v| v.as_str()).is_some(),
            "an error response carries a string `error` field: {line}"
        );
        // It is not mistakable for a snapshot (no run_id / snapshot_version).
        assert!(value.get("snapshot_version").is_none());
    }

    /// A "cannot reach the run" error takes the reserved CONTROL code and names the
    /// run — the distinguishable result for a stale/dead runner.
    #[test]
    fn unreachable_run_uses_the_control_code() {
        let err = unreachable_run("run-9", "its registry entry is stale".to_string());
        assert_eq!(err.code(), exit::CONTROL);
        let message = err.to_string();
        assert!(message.contains("run-9"), "names the run: {message}");
        assert!(message.contains("stale"), "carries the reason: {message}");
    }

    /// Endpoint tokens are unique per call, so concurrent runs never collide on a
    /// socket/pipe name.
    #[test]
    fn endpoint_tokens_are_unique() {
        let a = unique_token();
        let b = unique_token();
        assert_ne!(a, b, "each transport endpoint gets a distinct name");
    }

    /// A deeply nested registry must not leak into the Unix socket address: macOS
    /// allows only a short `sun_path`, which is much smaller than normal CI paths.
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_path_stays_short_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let long_registry = std::env::temp_dir().join("r".repeat(180));
        let server = imp::ControlServer::bind(&long_registry)
            .expect("a long registry path does not prevent binding the control socket");
        let endpoint = std::path::Path::new(server.endpoint());
        assert!(
            endpoint.as_os_str().as_encoded_bytes().len() < 100,
            "endpoint stays below the portable macOS sun_path budget: {endpoint:?}"
        );
        assert_eq!(
            std::fs::metadata(endpoint.parent().expect("socket has a parent"))
                .expect("private control directory exists")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(endpoint)
                .expect("control socket exists")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
