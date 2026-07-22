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
//! ## Ambiguous run id — a hard failure, never a guess
//!
//! [`registry::Registry::register`] does not enforce `run_id` uniqueness, so two
//! concurrent runs started with the same explicit `--run-id` can both be live at
//! once. [`resolve_live_endpoint`] (via [`resolve_in_registry`]) detects that (more
//! than one *live* entry matches the requested `run_id`, counted regardless of
//! whether each one has published an endpoint yet) and refuses to pick one: every
//! verb — `inspect`, `cancel`, and `kill` alike — reports the same reserved
//! [`exit::CONTROL`] (103) "ambiguous run id" failure rather than acting on whichever
//! entry the directory scan happens to return first. For the mutating verbs this is
//! load-bearing (a wrong guess cancels or kills the *other* run); the read-only
//! `inspect` gets the identical hard failure rather than a softer fallback, because a
//! snapshot of the wrong run is exactly as misleading as acting on it. See
//! `docs/registry.md`, "Run id resolution — ambiguity is a hard failure".
//!
//! That initial check alone is a TOCTOU race for `cancel`/`kill`: a duplicate can
//! register under the same `run_id` in the window between the scan and the
//! destructive verb reaching the wire (the `connect_live` round trip in between).
//! [`mutate_async`] narrows that window as tightly as the registry's decentralized,
//! no-locking-across-processes design allows: it re-runs [`resolve_in_registry`]
//! ([`reconfirm_target`]) immediately before writing the verb, and aborts — without
//! ever writing — unless it resolves back to the exact endpoint already connected
//! to. A sub-instruction gap remains between that synchronous re-check and the
//! `.await`ed write itself; closing it fully would need a `run_id`-keyed lock held
//! across process boundaries, which this design deliberately does not attempt. It
//! cannot misdirect the verb, though: the client is already connected to the
//! target's specific, uniquely-tokened transport endpoint by the time the re-check
//! runs, and no later registry write can retarget bytes already destined for an
//! open connection — see
//! `racing_duplicate_after_reconfirm_does_not_misdirect_the_dispatched_verb` in this
//! module's tests. `inspect` does not repeat this re-check: it is read-only, so a
//! race that surfaces a snapshot from just before a duplicate registered is stale
//! information, not a wrong-target action.
//!
//! ## Wire protocol
//!
//! Line-oriented and deliberately tiny. A client writes one request verb line
//! (`inspect\n`; an empty line is also treated as `inspect`) and reads back one JSON
//! line, then the server closes the connection. Three verbs share this one framing:
//!
//! - **`inspect`** — read-only; the reply is a [`Snapshot`].
//! - **`cancel`** — mutating; the runner runs its shared soft-stop → grace →
//!   hard-kill teardown (the same one a `Ctrl-C` uses) and the run exits with the
//!   reserved [`exit::CONTROL_CANCELLED`] (108). The reply is a [`ControlAck`].
//! - **`kill`** — mutating; the runner hard-kills the whole tree immediately (no
//!   soft stop, no grace) and the run exits with [`exit::CONTROL_KILLED`] (109). The
//!   reply is a [`ControlAck`].
//!
//! The mutating verbs never reshape the framing: the runner writes its ack line and
//! only **then** signals its main loop to tear down (via a [`ControlCommandSink`]),
//! so a `cancel`/`kill` client always receives its confirmation even though the run
//! ends at once. Everything the outside world needs is also in the JSONL stream — a
//! `cancelled` / `killed` event and a terminal `runner_exit` with the matching
//! `source` — so an observer reading `--jsonl` sees the external command, not just
//! the control client.

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

/// The read-only request verb. An empty request line is treated as this too, so a
/// bare connect-and-read probe still gets a snapshot.
const INSPECT_REQUEST: &str = "inspect";

/// The mutating verb that ends a run through the shared soft-stop → grace →
/// hard-kill teardown (the network analogue of a `Ctrl-C`).
const CANCEL_REQUEST: &str = "cancel";

/// The mutating verb that hard-kills a run's whole tree immediately (no grace).
const KILL_REQUEST: &str = "kill";

/// A mutating control-plane command, delivered from a `cancel`/`kill` client to the
/// live run's own `select!` loop (which owns teardown — this module never tears a
/// run down itself). The verb text is the on-the-wire request and the ack `action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// Soft-stop → grace → hard-kill teardown, over the network instead of a signal.
    Cancel,
    /// Immediate hard kill of the whole tree — no soft stop and no grace.
    Kill,
}

impl ControlCommand {
    /// The verb this command is spelled as on the wire and echoed in the ack.
    fn verb(self) -> &'static str {
        match self {
            ControlCommand::Cancel => CANCEL_REQUEST,
            ControlCommand::Kill => KILL_REQUEST,
        }
    }
}

/// The channel the control server pushes a mutating command into, handed to the
/// run's main loop. An **unbounded** sender so the server's send is synchronous and
/// cannot yield or block between writing its ack and signaling teardown: the ack is
/// fully flushed first, then the run tears down at once. The run holds the sole
/// receiver for its whole life, so a send from a live serve loop always lands.
pub type ControlCommandSink = tokio::sync::mpsc::UnboundedSender<ControlCommand>;

/// The one-line reply to a `cancel`/`kill` verb: the runner accepted the command and
/// began tearing the run down. `Serialize` on the server, `Deserialize` on the
/// client (which parses it back and checks it names the action it asked for, so a
/// garbled or foreign reply is a distinguishable failure rather than a false
/// success — the same discipline `inspect` applies to its [`Snapshot`]).
#[derive(Debug, Serialize, Deserialize)]
pub struct ControlAck {
    /// Whether the runner accepted the command and started teardown.
    pub accepted: bool,
    /// The action taken — `cancel` or `kill` — echoed so the client can confirm the
    /// runner answered the verb it sent.
    pub action: String,
    /// The run the command targeted (the id the client matched in the registry).
    pub run_id: String,
}

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

    /// The acknowledgement for a mutating verb: the runner accepted `action` for this
    /// run. Built from the same settled `run_id` the snapshot names.
    fn ack(&self, action: &str) -> ControlAck {
        ControlAck {
            accepted: true,
            action: action.to_string(),
            run_id: self.run_id.to_string(),
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
pub async fn serve(
    server: Option<ControlServer>,
    source: &SnapshotSource<'_>,
    commands: &ControlCommandSink,
) -> Infallible {
    match server {
        Some(server) => server.serve(source, commands).await,
        None => std::future::pending().await,
    }
}

/// Handle one accepted connection: read the request verb, write the JSON response,
/// close. Bounded by [`CONNECTION_DEADLINE`] so a client that connects and stalls
/// cannot wedge the accept loop. Errors are swallowed — a broken client connection is
/// never the run's problem.
async fn handle_connection<S>(stream: S, source: &SnapshotSource<'_>, commands: &ControlCommandSink)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(CONNECTION_DEADLINE, serve_one(stream, source, commands)).await;
}

/// The request/response exchange for one connection.
///
/// A mutating verb (`cancel`/`kill`) writes its ack **before** it signals the run's
/// main loop through `commands`: the ack is fully flushed and the write half
/// half-closed first, so the client always receives its confirmation even though the
/// run tears down the moment the signal lands. If the ack cannot even be written (a
/// broken client), no command is signaled — an unconfirmed cancel does not silently
/// end the run.
async fn serve_one<S>(
    stream: S,
    source: &SnapshotSource<'_>,
    commands: &ControlCommandSink,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = split(stream);
    let mut reader = BufReader::new(read_half);
    let mut request = String::new();
    reader.read_line(&mut request).await?;
    match request.trim() {
        INSPECT_REQUEST | "" => {
            write_response(&mut write_half, &serialize_snapshot(&source.snapshot())).await?;
        }
        CANCEL_REQUEST => {
            write_response(&mut write_half, &serialize_ack(&source.ack(CANCEL_REQUEST))).await?;
            // Ack delivered — now ask the run's main loop to tear down. The send is
            // synchronous (unbounded) and best-effort: a dropped receiver only means
            // the run is already ending.
            let _ = commands.send(ControlCommand::Cancel);
        }
        KILL_REQUEST => {
            write_response(&mut write_half, &serialize_ack(&source.ack(KILL_REQUEST))).await?;
            let _ = commands.send(ControlCommand::Kill);
        }
        other => {
            let error = serialize_error(&format!("unknown control request `{other}`"));
            write_response(&mut write_half, &error).await?;
        }
    }
    Ok(())
}

/// Write one JSON response line and end the response: flush it, then half-close the
/// write side (best-effort — some transports have no half-close) so the client's
/// read completes at once.
async fn write_response<W>(write_half: &mut W, response: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_half.write_all(response.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
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

/// Serialize a `cancel`/`kill` acknowledgement for the wire. A struct of owned
/// strings and a bool cannot fail to serialize; the fallback is defensive only.
fn serialize_ack(ack: &ControlAck) -> String {
    serde_json::to_string(ack)
        .unwrap_or_else(|_| String::from(r#"{"accepted":false,"action":"error","run_id":""}"#))
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
    let endpoint = resolve_live_endpoint("inspect", run_id).await?;

    // Connect under a deadline: a runner that died between the liveness probe and now
    // fails fast here instead of hanging the client.
    let stream = connect_live(&endpoint, "inspect", run_id).await?;

    // Converse under a deadline: a runner that died mid-write, or accepted but never
    // answers, is bounded here — a distinguishable CONTROL result, not a hang.
    let snapshot = tokio::time::timeout(CONVERSATION_DEADLINE, converse(stream))
        .await
        .map_err(|_| {
            unreachable_run(
                "inspect",
                run_id,
                "the runner did not answer in time".into(),
            )
        })?
        .map_err(|err| unreachable_run("inspect", run_id, err.to_string()))?;

    let json = serde_json::to_string(&snapshot).map_err(|err| {
        RunnerError::new(
            exit::INTERNAL,
            format!("could not render the inspect snapshot: {err}"),
        )
    })?;
    println!("{json}");
    Ok(())
}

/// Client entry for `cancel --run-id <id>`: reach the live runner through the
/// registry and ask it to end the run through its shared soft-stop → grace →
/// hard-kill teardown. On success the runner acks and its run exits with
/// [`exit::CONTROL_CANCELLED`] (108); the outcome is also written to the run's JSONL
/// stream. An unreachable/stale runner is the same distinguishable [`exit::CONTROL`]
/// (103) failure `inspect` reports — never a hang.
pub fn cancel(run_id: &str) -> Result<(), RunnerError> {
    run_mutation(run_id, ControlCommand::Cancel)
}

/// Client entry for `kill --run-id <id>`: reach the live runner and ask it to
/// hard-kill the whole tree immediately (no grace). On success the run exits with
/// [`exit::CONTROL_KILLED`] (109). An unreachable runner is an [`exit::CONTROL`]
/// (103) failure, exactly like [`cancel`] and [`inspect`].
pub fn kill(run_id: &str) -> Result<(), RunnerError> {
    run_mutation(run_id, ControlCommand::Kill)
}

/// Shared driver for the mutating clients ([`cancel`] / [`kill`]): stand up the same
/// small current-thread runtime `inspect` uses and run the exchange.
fn run_mutation(run_id: &str, command: ControlCommand) -> Result<(), RunnerError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            RunnerError::new(
                exit::INTERNAL,
                format!("could not start the async runtime: {err}"),
            )
        })?;
    runtime.block_on(mutate_async(run_id, command))
}

/// The async body of [`cancel`] / [`kill`]: registry lookup, connect, re-confirm the
/// target is still the sole live match, send the verb, read and verify the ack,
/// print it. Every runner-loss path is a bounded [`exit::CONTROL`] failure,
/// mirroring [`inspect_async`].
async fn mutate_async(run_id: &str, command: ControlCommand) -> Result<(), RunnerError> {
    let action = command.verb();
    let registry = open_registry(action, run_id)?;
    let endpoint = resolve_in_registry(&registry, action, run_id)?;
    let stream = connect_live(&endpoint, action, run_id).await?;

    // Close the resolve-to-dispatch race as tightly as the registry's decentralized,
    // no-locking-across-processes design allows (`AGENTS.md`, "No PID addressing";
    // `docs/registry.md`, "Run id resolution"): a duplicate run can register under
    // the same `run_id` at any point during `resolve_in_registry`'s scan or
    // `connect_live`'s round trip. Re-scan and re-resolve right before writing the
    // verb and abort on any outcome other than resolving back to the exact endpoint
    // already connected to. A sub-instruction gap remains between this synchronous
    // check and the `.await`ed write in `converse_mutation` below — closing that
    // fully would need a run_id-keyed lock held across process boundaries, which the
    // registry deliberately does not provide — but it cannot misdirect the verb:
    // `connect_live` already bound this client to `endpoint`'s specific,
    // uniquely-tokened connection, so a duplicate registering in that gap cannot
    // retarget bytes already destined for it (proven by
    // `racing_duplicate_after_reconfirm_does_not_misdirect_the_dispatched_verb`).
    reconfirm_target(&registry, action, run_id, &endpoint)?;

    let ack = tokio::time::timeout(CONVERSATION_DEADLINE, converse_mutation(stream, command))
        .await
        .map_err(|_| unreachable_run(action, run_id, "the runner did not answer in time".into()))?
        .map_err(|err| unreachable_run(action, run_id, err.to_string()))?;

    // A well-behaved runner acks the exact action; a rejected or mismatched reply is a
    // CONTROL failure, never a false success (the same parse-back discipline inspect
    // applies to its snapshot).
    if !ack.accepted || ack.action != action {
        return Err(unreachable_run(
            action,
            run_id,
            "the runner did not acknowledge the command".to_string(),
        ));
    }

    let json = serde_json::to_string(&ack).map_err(|err| {
        RunnerError::new(
            exit::INTERNAL,
            format!("could not render the control ack: {err}"),
        )
    })?;
    println!("{json}");
    Ok(())
}

/// Find the endpoint of the *live* run named `run_id`, or a distinguishable
/// [`exit::CONTROL`] failure that says *why* it cannot be reached. Shared by every
/// client (`inspect`/`cancel`/`kill`); `action` names the verb in the message. Opens
/// the env/platform-resolved registry and delegates the scan to
/// [`resolve_in_registry`], which the mutating verbs' pre-dispatch re-check
/// ([`reconfirm_target`]) also drives, against the same open [`registry::Registry`].
async fn resolve_live_endpoint(action: &str, run_id: &str) -> Result<String, RunnerError> {
    let registry = open_registry(action, run_id)?;
    resolve_in_registry(&registry, action, run_id)
}

/// Open the env/platform-resolved registry, mapping a failure to the same
/// distinguishable [`exit::CONTROL`] shape every other unreachable-run result uses.
fn open_registry(action: &str, run_id: &str) -> Result<registry::Registry, RunnerError> {
    registry::Registry::open().map_err(|err| {
        unreachable_run(
            action,
            run_id,
            format!("could not open the run registry: {err}"),
        )
    })
}

/// Scan `registry` for the *live* run named `run_id` and resolve its endpoint, or a
/// distinguishable [`exit::CONTROL`] failure that says why it cannot be reached — a
/// synchronous, no-`.await` scan+match so it can be re-run right before dispatch
/// (see [`reconfirm_target`]) with a minimal window between the check and the write
/// that follows, and driven directly against a scratch [`registry::Registry`] in
/// unit tests without touching the process-wide env-resolved registry.
fn resolve_in_registry(
    registry: &registry::Registry,
    action: &str,
    run_id: &str,
) -> Result<String, RunnerError> {
    let entries = registry.entries().map_err(|err| {
        unreachable_run(
            action,
            run_id,
            format!("could not read the run registry: {err}"),
        )
    })?;

    let matches: Vec<registry::Entry> = entries
        .into_iter()
        .filter(|entry| entry.record.run_id == run_id)
        .collect();
    if matches.is_empty() {
        return Err(unreachable_run(
            action,
            run_id,
            "no run with that id is registered".to_string(),
        ));
    }

    // Count *live* entries first — regardless of whether they advertise an
    // endpoint — before ever looking at endpoints. `register` (`src/registry.rs`)
    // never enforces `run_id` uniqueness, so two concurrent runs started with the
    // same explicit `--run-id` can both be live at once, and one of them may not
    // (yet, or ever) have published an endpoint (disconnected/failed transport).
    // Counting only endpoint-having entries would let such a duplicate evade
    // detection and have the sole endpoint-having entry acted on as if it were
    // unambiguous. Every verb (`inspect`/`cancel`/`kill`) shares this resolver and
    // treats *any* live duplicate as a hard, documented failure rather than
    // silently acting on whichever entry the directory scan happens to return
    // first: for the mutating verbs, guessing wrong means cancelling or killing
    // the *other* run instead of the intended one; `inspect` gets the same
    // treatment rather than a softer fallback because a snapshot of the wrong run
    // is just as misleading as acting on it (see `docs/registry.md`, "Run id
    // resolution — ambiguity is a hard failure").
    let live: Vec<&registry::Entry> = matches
        .iter()
        .filter(|entry| entry.health == Health::Live)
        .collect();
    if live.len() > 1 {
        return Err(ambiguous_run(action, run_id, live.len()));
    }

    // Exactly one live entry (or none) — now it's safe to look at its endpoint.
    // Say *why* it's unreachable — the run is gone (stale) or predates the
    // transport (live, no endpoint) — rather than a generic failure.
    let Some(entry) = live.into_iter().next() else {
        return Err(unreachable_run(
            action,
            run_id,
            "its registry entry is stale — the runner is gone (it exited without cleaning up)"
                .to_string(),
        ));
    };
    if entry.record.endpoint.is_none() {
        return Err(unreachable_run(
            action,
            run_id,
            "the run is live but exposes no control endpoint".to_string(),
        ));
    }
    Ok(entry
        .record
        .endpoint
        .as_deref()
        .expect("filtered for an entry whose endpoint is Some")
        .to_string())
}

/// Connect to a live runner's endpoint under [`CONNECT_DEADLINE`]: a runner that
/// died between the liveness probe and now fails fast as a bounded [`exit::CONTROL`]
/// error instead of hanging the client.
async fn connect_live(
    endpoint: &str,
    action: &str,
    run_id: &str,
) -> Result<imp::Stream, RunnerError> {
    tokio::time::timeout(CONNECT_DEADLINE, imp::connect(endpoint))
        .await
        .map_err(|_| {
            unreachable_run(
                action,
                run_id,
                "timed out connecting to the live runner".into(),
            )
        })?
        .map_err(|err| {
            unreachable_run(
                action,
                run_id,
                format!("could not reach the live runner (it may have just exited): {err}"),
            )
        })
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

/// Send a mutating verb (`cancel`/`kill`) and parse the runner's [`ControlAck`]. A
/// closed connection before a complete line (runner died mid-conversation) or an
/// unparseable line surfaces as an error the caller maps to [`exit::CONTROL`] — the
/// same shape as [`converse`], but for the ack rather than a snapshot.
async fn converse_mutation<S>(stream: S, command: ControlCommand) -> io::Result<ControlAck>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = split(stream);
    write_half.write_all(command.verb().as_bytes()).await?;
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
    serde_json::from_str::<ControlAck>(line.trim()).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the runner sent an unreadable response: {err}"),
        )
    })
}

/// A "cannot reach the target run" error carrying the reserved [`exit::CONTROL`] code
/// and a message naming the `action` (`inspect`/`cancel`/`kill`), the run, and the
/// reason.
fn unreachable_run(action: &str, run_id: &str, detail: String) -> RunnerError {
    RunnerError::new(
        exit::CONTROL,
        format!("cannot {action} run `{run_id}`: {detail}"),
    )
}

/// An "ambiguous run id" error: `count` distinct live, reachable registry entries
/// share `run_id`, so [`resolve_live_endpoint`] refuses to guess which one `action`
/// means — reserving the same [`exit::CONTROL`] code as every other unreachable-run
/// result (still "could not reach *the* target run": there is no single one to
/// reach). See `docs/registry.md`, "Run id resolution — ambiguity is a hard
/// failure".
fn ambiguous_run(action: &str, run_id: &str, count: usize) -> RunnerError {
    RunnerError::new(
        exit::CONTROL,
        format!(
            "cannot {action} run `{run_id}`: ambiguous run id — {count} live runs are \
             registered under it; re-run with a run id that is unique among live runs"
        ),
    )
}

/// Re-run [`resolve_in_registry`] against the same open `registry` right before a
/// mutating verb (`cancel`/`kill`) is dispatched, and require it to resolve back to
/// the exact `expected_endpoint` [`mutate_async`] already connected to. Closes the
/// window between the initial resolution and this re-check: a duplicate that
/// registered under `run_id` during the scan or the connect round trip now makes the
/// id ambiguous again and surfaces that ambiguity here (or, in the vanishingly
/// unlikely case the original entry went stale *and* a single different entry now
/// resolves instead, a dedicated "changed during dispatch" failure) — either way the
/// verb is never written to the wire *for that outcome*.
///
/// This check is synchronous and cannot itself be made atomic with the `.await`ed
/// write that follows in [`mutate_async`], so a duplicate could in principle still
/// register in the residual gap between this function returning and that write. That
/// gap cannot **misdirect** the verb, though: by the time this runs, `connect_live`
/// has already bound the client to `expected_endpoint`'s specific, uniquely-tokened
/// transport connection, and no later registry write can retarget bytes already
/// destined for an open connection — see
/// `racing_duplicate_after_reconfirm_does_not_misdirect_the_dispatched_verb` below
/// and `docs/registry.md`, "Run id resolution".
fn reconfirm_target(
    registry: &registry::Registry,
    action: &str,
    run_id: &str,
    expected_endpoint: &str,
) -> Result<(), RunnerError> {
    let endpoint = resolve_in_registry(registry, action, run_id)?;
    if endpoint != expected_endpoint {
        return Err(unreachable_run(
            action,
            run_id,
            "the resolved run changed identity between resolution and dispatch; refusing to \
             guess which one to act on"
                .to_string(),
        ));
    }
    Ok(())
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

    use super::{
        ControlCommandSink, Infallible, SnapshotSource, handle_connection, io, unique_token,
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

    use super::{
        ControlCommandSink, Infallible, SnapshotSource, handle_connection, io, unique_token,
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

    /// A control ack round-trips through JSON: the server serializes exactly what the
    /// `cancel`/`kill` client parses back to confirm the runner answered its verb.
    #[test]
    fn ack_round_trips_through_json() {
        let line = serialize_ack(&ControlAck {
            accepted: true,
            action: "kill".to_string(),
            run_id: "run-k".to_string(),
        });
        let parsed: ControlAck = serde_json::from_str(&line).expect("an ack line parses back");
        assert!(parsed.accepted);
        assert_eq!(parsed.action, "kill");
        assert_eq!(parsed.run_id, "run-k");
    }

    #[test]
    fn command_verbs_are_the_on_the_wire_spelling() {
        assert_eq!(ControlCommand::Cancel.verb(), "cancel");
        assert_eq!(ControlCommand::Kill.verb(), "kill");
    }

    /// Drive one server-side exchange for `verb` over an in-memory duplex stream, and
    /// return the response line the client read plus the command (if any) the server
    /// routed to the run's main loop. The shared harness for the routing tests below.
    async fn serve_verb(verb: &str) -> (String, Option<ControlCommand>) {
        let (mut client, server) = tokio::io::duplex(1024);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let members = || vec![Member::from_pid(1)];
        let source = SnapshotSource::new(
            "run-t",
            "job_object",
            Some(1),
            SystemTime::UNIX_EPOCH,
            &members,
        );

        client
            .write_all(format!("{verb}\n").as_bytes())
            .await
            .expect("write the request verb");
        serve_one(server, &source, &tx)
            .await
            .expect("serve one connection");

        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("read the response");
        (line.trim().to_string(), rx.try_recv().ok())
    }

    /// The server routes a `cancel` verb into a `Cancel` command *and* answers with an
    /// ack naming the run and the action — the wire contract the `cancel` client
    /// depends on.
    #[tokio::test]
    async fn cancel_verb_acks_and_routes_a_command() {
        let (response, command) = serve_verb("cancel").await;
        let ack: ControlAck = serde_json::from_str(&response).expect("the reply is an ack");
        assert!(ack.accepted, "the runner accepts the cancel: {response}");
        assert_eq!(ack.action, "cancel");
        assert_eq!(ack.run_id, "run-t");
        assert_eq!(
            command,
            Some(ControlCommand::Cancel),
            "a cancel verb routes a Cancel command to the run's loop"
        );
    }

    /// The `kill` verb routes a distinct `Kill` command and acks it — distinguishable
    /// from cancel on both the command and the ack's `action`.
    #[tokio::test]
    async fn kill_verb_acks_and_routes_a_distinct_command() {
        let (response, command) = serve_verb("kill").await;
        let ack: ControlAck = serde_json::from_str(&response).expect("the reply is an ack");
        assert!(ack.accepted);
        assert_eq!(ack.action, "kill");
        assert_eq!(
            command,
            Some(ControlCommand::Kill),
            "a kill verb routes a Kill command, distinct from cancel"
        );
    }

    /// `inspect` stays read-only: it answers with a snapshot and routes **no**
    /// teardown command — the mutating verbs did not regress the query path.
    #[tokio::test]
    async fn inspect_verb_answers_a_snapshot_and_routes_no_command() {
        let (response, command) = serve_verb("inspect").await;
        let snapshot: Snapshot = serde_json::from_str(&response).expect("the reply is a snapshot");
        assert_eq!(snapshot.run_id, "run-t");
        assert!(
            command.is_none(),
            "inspect must never signal a teardown command"
        );
    }

    /// An unrecognized verb is a structured error and routes no command — a foreign
    /// client cannot end a run by sending garbage.
    #[tokio::test]
    async fn unknown_verb_errors_and_routes_no_command() {
        let (response, command) = serve_verb("frobnicate").await;
        let value: serde_json::Value = serde_json::from_str(&response).expect("valid JSON");
        assert!(
            value.get("error").and_then(|v| v.as_str()).is_some(),
            "an unknown verb yields an error object: {response}"
        );
        assert!(
            command.is_none(),
            "an unknown verb must never signal a teardown command"
        );
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
    /// action and the run — the distinguishable result for a stale/dead runner, the
    /// same for every client verb.
    #[test]
    fn unreachable_run_uses_the_control_code() {
        let err = unreachable_run("cancel", "run-9", "its registry entry is stale".to_string());
        assert_eq!(err.code(), exit::CONTROL);
        let message = err.to_string();
        assert!(message.contains("cancel"), "names the action: {message}");
        assert!(message.contains("run-9"), "names the run: {message}");
        assert!(message.contains("stale"), "carries the reason: {message}");
    }

    /// An "ambiguous run id" error also takes the reserved CONTROL code, and names
    /// the action, the run, and how many live entries collided — distinguishable
    /// from every other unreachable-run reason.
    #[test]
    fn ambiguous_run_uses_the_control_code() {
        let err = ambiguous_run("kill", "dup-id", 2);
        assert_eq!(err.code(), exit::CONTROL);
        let message = err.to_string();
        assert!(message.contains("kill"), "names the action: {message}");
        assert!(message.contains("dup-id"), "names the run: {message}");
        assert!(message.contains("ambiguous"), "names the reason: {message}");
        assert!(
            message.contains('2'),
            "carries how many entries collided: {message}"
        );
    }

    /// A unique, empty scratch directory for a test registry — mirrors
    /// `registry::tests::scratch`, kept local here so these tests drive
    /// `resolve_in_registry`/`reconfirm_target` against an isolated
    /// `registry::Registry::open_in` handle and never touch the process-wide
    /// env-resolved registry `resolve_live_endpoint` uses in production (which would
    /// be racy across parallel test threads).
    fn scratch_registry_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-control-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The resolve-to-dispatch TOCTOU window this task closes (see the module doc
    /// comment, "Ambiguous run id"; `docs/registry.md`, "Run id resolution"): a
    /// duplicate run can register under the same `run_id` after a mutating verb's
    /// client performs its initial resolve but before the verb reaches the wire.
    /// `reconfirm_target` re-scans right before dispatch and must catch exactly
    /// that. This drives the race deterministically — register, resolve, *then*
    /// register the racing duplicate, then re-check — rather than depending on real
    /// thread timing, which would make the test itself flaky.
    #[test]
    fn reconfirm_target_catches_a_duplicate_registered_after_the_initial_resolve() {
        let dir = scratch_registry_dir("reconfirm-race");
        let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

        let first = registry
            .register("dup-race", Some("endpoint-a"), SystemTime::now())
            .expect("register the first run");

        let endpoint = resolve_in_registry(&registry, "kill", "dup-race")
            .expect("the sole live run resolves before the race window opens");
        assert_eq!(endpoint, "endpoint-a");

        // The race: a second run registers under the same run_id in the window
        // between the client's initial resolve and its dispatch.
        let second = registry
            .register("dup-race", Some("endpoint-b"), SystemTime::now())
            .expect("register the second (racing) run");

        let err = reconfirm_target(&registry, "kill", "dup-race", &endpoint)
            .expect_err("the pre-dispatch re-check must catch the now-ambiguous run id");
        assert_eq!(err.code(), exit::CONTROL);
        assert!(
            err.to_string().contains("ambiguous"),
            "names the reason: {err}"
        );

        drop(first);
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (R-02) Ambiguity detection must count *every* live entry, not just the ones
    /// that happen to advertise an endpoint. A live run that has not (yet, or
    /// ever) published an endpoint — a disconnected or failed transport — must
    /// still make the `run_id` ambiguous; it must not be silently skipped in favor
    /// of treating the sole endpoint-having entry as unambiguous.
    #[test]
    fn resolve_in_registry_detects_ambiguity_even_when_one_duplicate_has_no_endpoint() {
        let dir = scratch_registry_dir("dup-endpointless");
        let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

        let with_endpoint = registry
            .register("dup-endpointless", Some("endpoint-a"), SystemTime::now())
            .expect("register the run that has an endpoint");
        let without_endpoint = registry
            .register("dup-endpointless", None, SystemTime::now())
            .expect("register the live run that never published an endpoint");

        let err = resolve_in_registry(&registry, "kill", "dup-endpointless")
            .expect_err("two live entries under the same run_id must be ambiguous");
        assert_eq!(err.code(), exit::CONTROL);
        assert!(
            err.to_string().contains("ambiguous"),
            "names the reason: {err}"
        );

        drop(with_endpoint);
        drop(without_endpoint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a racing registration, the pre-dispatch re-check resolves back to the
    /// same endpoint and passes — a mutating verb with no duplicate in flight is
    /// never blocked by this defense.
    #[test]
    fn reconfirm_target_passes_when_no_duplicate_registers() {
        let dir = scratch_registry_dir("reconfirm-clean");
        let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

        let first = registry
            .register("solo-run", Some("endpoint-solo"), SystemTime::now())
            .expect("register the run");

        let endpoint = resolve_in_registry(&registry, "cancel", "solo-run")
            .expect("the sole live run resolves");

        reconfirm_target(&registry, "cancel", "solo-run", &endpoint)
            .expect("no racing registration occurred, so the re-check passes");

        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `reconfirm_target` closes the window between the client's initial resolve and
    /// its dispatch, but re-review (see `docs/registry.md`, "Run id resolution") kept
    /// finding a further residual gap: `reconfirm_target` is a synchronous scan, while
    /// the verb itself goes out through a later `.await` on the write half
    /// (`converse_mutation`), so a duplicate could in principle register in between.
    /// This test proves that residual gap cannot **misdirect** the verb, which is the
    /// actual hazard the finding cares about (a destructive command landing on the
    /// wrong run) — `connect_live` already bound the client to run A's specific,
    /// uniquely-tokened transport connection *before* `reconfirm_target` ran, so a
    /// later registry write cannot retarget bytes already destined for that open
    /// connection. It drives the race deterministically — reconfirm, *then* register
    /// the racing duplicate, *then* dispatch — rather than depending on real thread
    /// timing.
    #[tokio::test]
    async fn racing_duplicate_after_reconfirm_does_not_misdirect_the_dispatched_verb() {
        let dir = scratch_registry_dir("reconfirm-post-race");
        let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

        // Stand in for the real transport connection `connect_live` would already hold
        // by this point: an in-memory duplex, one side owned by the client
        // (`converse_mutation`), the other by run A's server loop (`serve_one`).
        let (client_stream, server_stream) = tokio::io::duplex(1024);

        let first = registry
            .register("dup-post-race", Some("endpoint-a"), SystemTime::now())
            .expect("register the run the client is connected to");

        let endpoint = resolve_in_registry(&registry, "cancel", "dup-post-race")
            .expect("the sole live run resolves before the race window opens");
        assert_eq!(endpoint, "endpoint-a");

        reconfirm_target(&registry, "cancel", "dup-post-race", &endpoint)
            .expect("no duplicate has registered yet, so the re-check passes");

        // The race, landing *after* the re-check passes — the residual window this
        // test targets: a second run registers under the same run_id while the verb is
        // still in flight to the connection already established with run A.
        let second = registry
            .register("dup-post-race", Some("endpoint-b"), SystemTime::now())
            .expect("register the racing duplicate after the re-check passed");

        let members = || vec![Member::from_pid(1)];
        let source = SnapshotSource::new(
            "dup-post-race",
            "job_object",
            Some(1),
            SystemTime::UNIX_EPOCH,
            &members,
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Drive both sides of the already-open connection concurrently, exactly as
        // `mutate_async` does after `reconfirm_target` returns.
        let (serve_result, ack) = tokio::join!(
            serve_one(server_stream, &source, &tx),
            converse_mutation(client_stream, ControlCommand::Cancel),
        );
        serve_result.expect("run A answers the one connection it actually received");
        let ack = ack.expect("the verb reaches run A over its already-open connection");

        assert!(ack.accepted, "run A acks the cancel it actually received");
        assert_eq!(ack.action, "cancel");
        assert_eq!(
            ack.run_id, "dup-post-race",
            "the ack comes from the pre-reconfirmed run"
        );
        assert_eq!(
            rx.try_recv().ok(),
            Some(ControlCommand::Cancel),
            "the routed command came from run A's connection, never the racing \
             duplicate registered under \"endpoint-b\" after the re-check — a \
             transport connection cannot be retargeted by a later registry write, so \
             the verb reaches exactly the run that was reconfirmed regardless of the \
             now-ambiguous run_id bookkeeping"
        );

        drop(first);
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
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
