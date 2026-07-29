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
//!   PID, container members (enriched with `ppid`/executable `name`/`start_time`
//!   wherever the platform can report them — the same shape as the JSONL
//!   `members_snapshot`), and start time.
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
//! ## Dead runner / unreachable entry — a distinguishable result, never a hang
//!
//! A client can lose the runner three ways, and every one of them is reported as the
//! reserved [`exit::CONTROL`] code (103, "could not reach the target run" — see
//! `docs/exit-codes.md`) with an explanatory message, **never** a generic error and
//! **never** a hang:
//!
//! - **Stale registry entry.** The runner died abruptly, leaving its record behind;
//!   the released liveness lock makes the entry [`registry::Health::Stale`]
//!   ([`registry::Registry::entries`], T-007). The client detects this *before*
//!   connecting and reports the run as gone.
//! - **Unprobeable registry entry.** The liveness probe could not run at all — the
//!   lock file would not open (a directory in its place, a permission error, a
//!   rejected symlink/reparse point) or the lock call itself errored — so the entry
//!   is [`registry::Health::Unprobed`] (T-206) and liveness is *unknown*. The client
//!   refuses exactly as it does for a stale entry (it acts only on a confirmed
//!   [`registry::Health::Live`] match, and this is not one), but it deliberately does
//!   **not** report the runner as gone: that is a confirmed death the probe never
//!   established, and asserting it here would contradict what `list`, `prune`, and
//!   `wait` say about the very same record. The refusal names the entry `unprobed`
//!   instead — the vocabulary those three already share for this case.
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
//! by-`run-id` verb — `inspect`, `cancel`, and `kill` alike — reports the same reserved
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
//! information, not a wrong-target action. The aggregate `cancel --all` / `kill
//! --all` forms are deliberately different: their snapshot is keyed by the unique
//! registry-record path and dispatches directly to each record's endpoint, so two
//! duplicate ids are two independent targets rather than an ambiguity.
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
//!
//! Both directions of that one line are read under the same [`MAX_LINE_BYTES`]
//! ceiling: [`serve_one`] reading the request verb, and [`converse`] reading the
//! reply. Owner-only does not mean trusted-not-to-misbehave — a broken or hostile
//! local client that never sends a `\n` must not be able to make the live runner
//! buffer an unbounded amount of memory just to find one out. Exceeding the ceiling
//! is a protocol violation, not silent truncation: the server answers with the same
//! structured-error closing path an unrecognized verb gets, and the client surfaces
//! the same `io::Error` shape an unparsable reply already does.

use std::convert::Infallible;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    split,
};

use crate::events::{self, Member};
use crate::exit::{self, RunnerError};
use crate::registry::{self, Health};

mod render;

pub use imp::ControlServer;
use render::inspect_all_output_lines;
#[cfg(test)]
use render::render_snapshot_human;
use render::snapshot_output_lines;

/// Control-plane snapshot format version. Independent of the JSONL event
/// [`schema_version`](crate::events::SCHEMA_VERSION) and the
/// [`registry_version`](crate::registry::REGISTRY_VERSION): the `inspect` response is
/// the control plane's own private client/runner contract, so it versions on its own
/// axis.
pub const SNAPSHOT_VERSION: u32 = 2;

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

/// Which verb a request line names — the exact classification [`serve_one`]
/// applies to the one line it reads before deciding what to write back. `None`
/// covers everything else (an unrecognized verb gets the structured error
/// response, see [`serve_one`]). Pure — matches on the trimmed text only, no
/// I/O — so it can be driven directly with arbitrary bytes without a live
/// transport, which is what lets it double as half of the control-plane wire
/// fuzz target (`fuzz/fuzz_targets/control_wire.rs`, T-186; the other half is
/// the client-side JSON decode of [`Snapshot`]/[`ControlAck`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum RequestVerb {
    /// The read-only request. Named explicitly or by an empty line.
    Inspect,
    /// The mutating soft-stop → grace → hard-kill request.
    Cancel,
    /// The mutating immediate hard-kill request.
    Kill,
}

/// Classify a request line's trimmed text into the [`RequestVerb`] it names, or
/// `None` for anything unrecognized. See [`RequestVerb`] for why this is a
/// standalone pure function.
#[doc(hidden)]
pub fn classify_request(line: &str) -> Option<RequestVerb> {
    match line.trim() {
        INSPECT_REQUEST | "" => Some(RequestVerb::Inspect),
        CANCEL_REQUEST => Some(RequestVerb::Cancel),
        KILL_REQUEST => Some(RequestVerb::Kill),
        _ => None,
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

/// How long the *server* spends on a single client exchange before dropping it.
/// Both platform accept loops ([`ControlServer::serve`], unix and Windows alike)
/// serve connections strictly **sequentially**: one [`handle_connection`] call is
/// awaited to completion (or this timeout) before the loop accepts or services the
/// next, so a client that connects and then stalls blocks every client queued behind
/// it, with no bound on how long a queue of stalled clients can grow (the run's own
/// path is already independent of this and never waits on a control client). This is
/// not merely an added-latency bound for the client stuck behind the stall: behind
/// exactly **one** stalled peer, a client usually still gets served on both
/// platforms, just later — with added latency up to roughly this deadline (the
/// margin is how much later it connected than the stalled peer; if the two connect
/// almost simultaneously, it is a race). Where the threshold at which a queued
/// client actually fails *without* ever being served sits, and which deadline fires,
/// differ by platform, because unix and Windows queue waiting clients differently:
///
/// - **unix:** the kernel backlog admits a connecting client's `accept()`
///   immediately no matter how deep the queue is, so every queued client's
///   [`CONVERSATION_DEADLINE`] window starts at roughly the same time as its peers',
///   independent of how far the server has gotten through the queue. Starting from a
///   **second** stalled peer already queued ahead, the accumulated wait before the
///   server reaches this client exceeds its own 5 seconds, and it is dropped without
///   ever being serviced. This surfaces as [`CONVERSATION_DEADLINE`] firing
///   ([`CONNECT_DEADLINE`] never triggers on unix in this scenario, since the connect
///   itself never blocks).
/// - **Windows:** only one pipe instance is ever free at a time, and the next
///   instance is created exactly when the server starts servicing the current one
///   (`server.connect().await` immediately followed by `create_instance` in
///   [`ControlServer::serve`]'s Windows impl), so a client at queue position *k+1*
///   cannot even connect until the server begins servicing position *k*. Its
///   [`CONVERSATION_DEADLINE`] window therefore only starts once it connects, and
///   that window covers the ≤5-second service of just the *one* peer immediately
///   ahead of it — so, unlike unix, a client behind even a second stalled peer is
///   normally still serviced once it does connect. Windows' actual failure mode is
///   on the *connect* side instead: a client that cannot yet reach a free instance
///   retries against `ERROR_PIPE_BUSY`, bounded by its own [`CONNECT_DEADLINE`]
///   (`connect`, this module's Windows impl) — and because each queue position's
///   connect window only opens roughly this deadline later than the position ahead
///   of it, that failure starts at a materially deeper queue position than unix's
///   two, not at the same numeric threshold.
///
/// Either way the failure surfaces as the reserved [`exit::CONTROL`] (103) exit
/// code; for `cancel`/`kill` that means the command was never delivered
/// (fail-closed, but a real failure, not a delay). The channel is owner-only, not
/// exposed to an untrusted network peer, so this is not a network-facing
/// vulnerability: what this deadline guarantees is only that *one* stalled client
/// cannot wedge the loop forever, not that other clients are served concurrently or
/// reliably reach the server once a deep-enough queue of stalled clients is ahead of
/// them.
const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);

/// The byte ceiling on the *one* line either side of the wire protocol reads: the
/// request verb ([`serve_one`]) and the JSON reply ([`converse`]). The protocol is
/// deliberately tiny (module doc, "Wire protocol") — a request line is `inspect` /
/// `cancel` / `kill` / empty plus `\n` (a handful of bytes), and a reply line is a
/// [`Snapshot`] (JSON of a handful of scalar fields plus an enriched `members` array),
/// a [`ControlAck`], or an error object. `64 KiB` sits comfortably above even a
/// generously large `members` list — the sole field with unbounded real-world size —
/// while staying nowhere near "unbounded": a peer that never sends a `\n` (an
/// owner-local client gone rogue, or a wedged runner on the reply side) is capped
/// here instead of growing the live runner's — or the client's — memory without
/// limit, which is the vulnerability this constant closes. It is not tuned to the
/// wire's typical size (bytes to low kilobytes); it is tuned to be small relative to
/// "no limit at all" while leaving generous headroom over anything the protocol
/// legitimately sends.
const MAX_LINE_BYTES: usize = 64 * 1024;

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
    /// Absolute path to the run's JSONL lifecycle stream.
    #[serde(default)]
    pub jsonl: Option<String>,
    /// Absolute output-capture directory, or `null` when capture is disabled.
    #[serde(default)]
    pub capture_dir: Option<String>,
    /// A point-in-time snapshot of the container's members, enriched with
    /// `ppid`/executable `name`/`start_time` wherever `members_info()` can report
    /// them, mirroring the JSONL `members_snapshot` (`docs/schema.md`, "Enriched
    /// member fields"). Queried at request time, so it reflects the container's
    /// composition *when inspected*, not at start.
    pub members: Vec<Member>,
}

/// The error line a server sends for an unrecognized request verb. The `inspect`
/// client never asks for anything else, so it only ever sees a [`Snapshot`]; this
/// exists so a future/foreign client gets a structured answer rather than silence.
///
/// `error: &'a str` is zero-copy on the *serialize* side ([`serialize_error`] never
/// needs to allocate), but that shape cannot be reused to deserialize a reply: serde
/// can only borrow a JSON string field when it contains no escape sequence, so a
/// server's diagnostic that happens to include a quote, backslash, or control
/// character (a Windows named-pipe path, for instance) would fail to parse as this
/// type and fall through to the generic "unreadable response" message the fallback
/// exists to avoid. [`converse`] instead deserializes replies into the owned
/// [`ErrorReply`] sibling below, which always parses regardless of escaping.
#[derive(Debug, Serialize)]
struct ErrorResponse<'a> {
    error: &'a str,
}

/// The owned counterpart to [`ErrorResponse`] that [`converse`] deserializes a reply
/// line into: the client is reading untrusted wire text back (a future/foreign
/// runner's diagnostic, or today's own `serialize_error` output), so it cannot rely
/// on the escape-free borrowing `ErrorResponse` needs on the serialize side (see its
/// doc comment). `#[doc(hidden)] pub` purely so the `control_wire` fuzz target can
/// drive the exact type `converse`'s fallback parses into, the same way it already
/// drives [`Snapshot`]/[`ControlAck`] (see `fuzz/fuzz_targets/control_wire.rs`).
#[derive(Debug, Deserialize)]
#[doc(hidden)]
pub struct ErrorReply {
    pub error: String,
}

/// The live facts the control server answers an `inspect` with. It borrows the run's
/// state (rather than owning a copy) so `members` is queried *at request time* — the
/// snapshot reflects the container's current composition, not a start-of-run census.
pub struct SnapshotSource<'a> {
    run_id: &'a str,
    mechanism: &'static str,
    root_pid: Option<u32>,
    started: SystemTime,
    jsonl: &'a str,
    capture_dir: Option<&'a str>,
    /// Produces the current enriched member list on demand. Kept as a borrowed
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
        jsonl: &'a str,
        capture_dir: Option<&'a str>,
        members: &'a (dyn Fn() -> Vec<Member> + 'a),
    ) -> Self {
        Self {
            run_id,
            mechanism,
            root_pid,
            started,
            jsonl,
            capture_dir,
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
            jsonl: Some(self.jsonl.to_string()),
            capture_dir: self.capture_dir.map(str::to_string),
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

/// Stand up the local control transport for a run. Not tied to the registry
/// directory: the unix implementation binds its socket in its own short-lived
/// directory under `/tmp` (the registry dir's path routinely exceeds
/// `sockaddr_un::sun_path` on macOS/BSD, see K-009), and the Windows
/// implementation lives in the kernel's named-pipe namespace, not the
/// filesystem. **Best-effort:** a failure warns on stderr and returns
/// `None` — the control plane is discovery infrastructure, and losing it only makes
/// this run un-inspectable, never costs the child its exit-code fidelity (the same
/// degradation as the registry itself, `AGENTS.md`, "Exit-code fidelity").
pub fn open_server() -> Option<ControlServer> {
    match ControlServer::bind() {
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
/// close. Bounded by [`CONNECTION_DEADLINE`], so a stalled client cannot wedge the
/// accept loop *forever* — but every caller (both platform `serve` loops) awaits this
/// inline, one connection at a time, so a stalled client still blocks the *next*
/// client's accept and service (see [`CONNECTION_DEADLINE`]'s doc comment for the
/// exact, platform-dependent threshold at which a queued client fails outright with
/// [`exit::CONTROL`] before ever being serviced — it differs between unix and
/// Windows and which deadline fires differs too).
async fn handle_connection<S>(stream: S, source: &SnapshotSource<'_>, commands: &ControlCommandSink)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(CONNECTION_DEADLINE, serve_one(stream, source, commands)).await;
}

/// Read one `\n`-terminated line from `reader`, capped at [`MAX_LINE_BYTES`] total —
/// the shared bound [`serve_one`] and [`converse`] both read their one line under.
///
/// Layered on `AsyncReadExt::take` over the buffered reader: once the cap is
/// exhausted, `take` reports EOF *without ever polling the underlying transport
/// again* (see `tokio::io::Take::poll_fill_buf`), so this returns deterministically
/// — bounded work, not a hang — even if the peer keeps the connection open and never
/// sends more. A line that reaches the cap without a trailing `\n` is the overflow
/// case: it is reported as an `InvalidData` error rather than silently handed back
/// truncated (which a caller could easily mistake for a short, valid line) or grown
/// without bound (the bug this cap exists to close). A shorter unterminated line is
/// a different protocol failure: it proves the peer closed mid-line, not that the
/// bound was exhausted. A clean, empty read (peer closed before sending anything)
/// still comes back as `Ok(0)`, matching `AsyncBufReadExt::read_line`'s own contract.
async fn read_bounded_line<R>(reader: &mut R, line: &mut String) -> io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    let read = reader.take(MAX_LINE_BYTES as u64).read_line(line).await?;
    if read == MAX_LINE_BYTES && !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "line exceeded the {MAX_LINE_BYTES}-byte control-plane limit without a \
                 terminating newline"
            ),
        ));
    }
    if read > 0 && !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the peer closed the connection before terminating its control-plane line",
        ));
    }
    Ok(read)
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
    match read_bounded_line(&mut reader, &mut request).await {
        Ok(_) => match classify_request(&request) {
            Some(RequestVerb::Inspect) => {
                write_response(&mut write_half, &serialize_snapshot(&source.snapshot())).await?;
            }
            Some(RequestVerb::Cancel) => {
                write_response(&mut write_half, &serialize_ack(&source.ack(CANCEL_REQUEST)))
                    .await?;
                // Ack delivered — now ask the run's main loop to tear down. The send is
                // synchronous (unbounded) and best-effort: a dropped receiver only means
                // the run is already ending.
                let _ = commands.send(ControlCommand::Cancel);
            }
            Some(RequestVerb::Kill) => {
                write_response(&mut write_half, &serialize_ack(&source.ack(KILL_REQUEST))).await?;
                let _ = commands.send(ControlCommand::Kill);
            }
            None => {
                let error =
                    serialize_error(&format!("unknown control request `{}`", request.trim()));
                write_response(&mut write_half, &error).await?;
            }
        },
        // Oversized request line: not a transport failure, so it gets the same
        // structured-error closing path an unrecognized verb does, not a bare
        // connection drop.
        Err(err) if err.kind() == io::ErrorKind::InvalidData => {
            let error = serialize_error(&format!("control request rejected: {err}"));
            write_response(&mut write_half, &error).await?;
        }
        Err(err) => return Err(err),
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

/// Build the small current-thread tokio runtime every client entry point (`run`,
/// `inspect`, `cancel`, `kill`) drives its async body on, mapping a build failure to
/// the shared [`exit::SETUP`] shape. `enable_all` arms the I/O, time, and signal
/// drivers each caller's body needs (Cargo unifies every caller's feature set into
/// the one tokio build), so one small runtime is enough for each — a run is one
/// child plus its output pumps, a deadline timer, and the stop-signal listeners
/// (`Ctrl-C`, plus `SIGTERM`/`SIGHUP` on Unix, plus `Ctrl-Break`/console
/// close/logoff/system shutdown on Windows); a control
/// client is one connection under a deadline.
pub fn current_thread_runtime() -> Result<tokio::runtime::Runtime, RunnerError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not start the async runtime: {err}"),
            )
        })
}

/// Client entry for `inspect --run-id <id> [--json]`: find the live runner through
/// the registry, ask it for a snapshot, and print it — as a single JSON line with
/// `--json` (unchanged, byte-for-byte, from before `--json` became optional), or as a
/// human-readable rendering by default (see [`render_snapshot_human`]). Runs on its
/// own small current-thread runtime (the transport client is async). A run that
/// cannot be reached — no such id, a stale entry, an unprobeable one, a
/// dead-mid-conversation runner — returns a [`exit::CONTROL`] error naming which of
/// those applied (see `no_live_entry`), regardless of the output format.
pub fn inspect(run_id: &str, json: bool) -> Result<(), RunnerError> {
    let runtime = current_thread_runtime()?;
    runtime.block_on(inspect_async(run_id, json))
}

/// The async body of [`inspect`]: registry lookup, connect, converse, print.
async fn inspect_async(run_id: &str, json: bool) -> Result<(), RunnerError> {
    let endpoint = resolve_live_endpoint("inspect", run_id).await?;

    // Connect under a deadline: a runner that died between the liveness probe and now
    // fails fast here instead of hanging the client.
    let stream = connect_live(&endpoint, "inspect", run_id).await?;

    // Converse under a deadline: a runner that died mid-write, or accepted but never
    // answers, is bounded here — a distinguishable CONTROL result, not a hang.
    let snapshot: Snapshot =
        converse_under_deadline(stream, INSPECT_REQUEST, "inspect", run_id).await?;
    verify_snapshot_identity(&snapshot, run_id)?;

    for line in snapshot_output_lines(&snapshot, json)? {
        println!("{line}");
    }
    Ok(())
}

/// Inspect every run confirmed live in one registry snapshot, optionally restricted
/// by conjunctive labels. The default report is a terminal-safe human summary with
/// expanded inspected snapshots; `json` retains the original one-array report. Each
/// target is either inspected, already gone, or failed; only a genuine failure makes
/// the aggregate command return [`exit::CONTROL`] after printing the full report.
pub fn inspect_all(labels: &[crate::labels::OperatorLabel], json: bool) -> Result<(), RunnerError> {
    let runtime = current_thread_runtime()?;
    runtime.block_on(inspect_all_async(labels, json))
}

#[derive(Debug, Serialize)]
pub struct InspectAllOutcome {
    pub run_id: String,
    pub status: InspectAllStatus,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
}

/// The honest result of inspecting one aggregate snapshot target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectAllStatus {
    Inspected,
    AlreadyGone,
    Failed,
}

async fn inspect_all_async(
    labels: &[crate::labels::OperatorLabel],
    json: bool,
) -> Result<(), RunnerError> {
    let registry = registry::open_read_only_for_setup()?;
    let targets = snapshot_live_targets(&registry, labels).map_err(registry::setup_read_error)?;

    let mut outcomes = Vec::with_capacity(targets.len());
    for target in targets {
        match inspect_snapshot_target(&registry, &target).await {
            Ok(InspectSnapshot::Inspected(snapshot)) => outcomes.push(InspectAllOutcome {
                run_id: target.run_id,
                status: InspectAllStatus::Inspected,
                snapshot: Some(snapshot),
                error: None,
            }),
            Ok(InspectSnapshot::AlreadyGone) => outcomes.push(InspectAllOutcome {
                run_id: target.run_id,
                status: InspectAllStatus::AlreadyGone,
                snapshot: None,
                error: None,
            }),
            Err(err) => outcomes.push(InspectAllOutcome {
                run_id: target.run_id,
                status: InspectAllStatus::Failed,
                snapshot: None,
                error: Some(err.to_string()),
            }),
        }
    }

    for line in inspect_all_output_lines(&outcomes, json)? {
        println!("{line}");
    }

    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.status == InspectAllStatus::Failed)
        .count();
    if failed > 0 {
        return Err(RunnerError::new(
            exit::CONTROL,
            format!(
                "inspect --all: {failed} of {} target run(s) could not be inspected; see the report above for the per-run reason",
                outcomes.len()
            ),
        ));
    }
    Ok(())
}

async fn inspect_snapshot_target(
    registry: &registry::Registry,
    target: &SnapshotTarget,
) -> Result<InspectSnapshot, RunnerError> {
    if snapshot_target_state(registry, target, "inspect")? == SnapshotTargetState::AlreadyGone {
        return Ok(InspectSnapshot::AlreadyGone);
    }
    let endpoint = target.endpoint.as_deref().ok_or_else(|| {
        unreachable_run(
            "inspect",
            &target.run_id,
            "the run is live but exposes no control endpoint".to_string(),
        )
    })?;
    let stream = match connect_live(endpoint, "inspect", &target.run_id).await {
        Ok(stream) => stream,
        Err(err) => {
            return match snapshot_target_state(registry, target, "inspect")? {
                SnapshotTargetState::AlreadyGone => Ok(InspectSnapshot::AlreadyGone),
                SnapshotTargetState::Live => Err(err),
            };
        }
    };
    if snapshot_target_state(registry, target, "inspect")? == SnapshotTargetState::AlreadyGone {
        return Ok(InspectSnapshot::AlreadyGone);
    }
    let snapshot: Snapshot =
        match converse_under_deadline(stream, INSPECT_REQUEST, "inspect", &target.run_id).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                return match snapshot_target_state(registry, target, "inspect")? {
                    SnapshotTargetState::AlreadyGone => Ok(InspectSnapshot::AlreadyGone),
                    SnapshotTargetState::Live => Err(err),
                };
            }
        };
    verify_snapshot_identity(&snapshot, &target.run_id)?;
    Ok(InspectSnapshot::Inspected(snapshot))
}

fn verify_snapshot_identity(snapshot: &Snapshot, expected_run_id: &str) -> Result<(), RunnerError> {
    if snapshot.run_id == expected_run_id {
        return Ok(());
    }
    Err(unreachable_run(
        "inspect",
        expected_run_id,
        "the runner returned a snapshot for a different run".to_string(),
    ))
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
    let runtime = current_thread_runtime()?;
    runtime.block_on(mutate_async(run_id, command))
}

/// The async body of [`cancel`] / [`kill`]: run [`mutate_one`] and print its ack.
/// Every runner-loss path is a bounded [`exit::CONTROL`] failure, mirroring
/// [`inspect_async`].
async fn mutate_async(run_id: &str, command: ControlCommand) -> Result<(), RunnerError> {
    let ack = mutate_one(run_id, command).await?;
    let json = serde_json::to_string(&ack).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not render the control ack: {err}"),
        )
    })?;
    println!("{json}");
    Ok(())
}

/// The by-`run_id` mutation itself: registry lookup, connect, re-confirm the target
/// is still the sole live match, send the verb, read and verify the ack — the exact
/// exchange the single-run `cancel`/`kill` client drives. Returns the parsed
/// [`ControlAck`] rather than printing it so lookup and rendering stay separate.
async fn mutate_one(run_id: &str, command: ControlCommand) -> Result<ControlAck, RunnerError> {
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
    // check and the `.await`ed write in `converse` (via `converse_under_deadline`)
    // below — closing that
    // fully would need a run_id-keyed lock held across process boundaries, which the
    // registry deliberately does not provide — but it cannot misdirect the verb:
    // `connect_live` already bound this client to `endpoint`'s specific,
    // uniquely-tokened connection, so a duplicate registering in that gap cannot
    // retarget bytes already destined for it (proven by
    // `racing_duplicate_after_reconfirm_does_not_misdirect_the_dispatched_verb`).
    reconfirm_target(&registry, action, run_id, &endpoint)?;

    let ack: ControlAck = converse_under_deadline(stream, command.verb(), action, run_id).await?;

    // A well-behaved runner acks the exact action; a rejected or mismatched reply is a
    // CONTROL failure, never a false success (the same parse-back discipline inspect
    // applies to its snapshot).
    if !ack_matches(&ack, action, run_id) {
        return Err(unreachable_run(
            action,
            run_id,
            "the runner did not acknowledge the command".to_string(),
        ));
    }
    Ok(ack)
}

/// Verify that a mutation reply belongs to the exact command and run the client
/// addressed. Both the by-id and aggregate paths use this single contract so neither
/// can accidentally accept an acknowledgement from a reused endpoint serving a
/// different run.
fn ack_matches(ack: &ControlAck, action: &str, run_id: &str) -> bool {
    ack.accepted && ack.action == action && ack.run_id == run_id
}

/// Client entry for `cancel --all` (T-217): the aggregate counterpart to [`cancel`],
/// reusing the exact same per-run mutation ([`mutate_one`] with
/// [`ControlCommand::Cancel`]) against every run confirmed live in a snapshot taken
/// the moment this call starts. See [`mutate_all`] for the snapshot, per-run report,
/// and aggregate exit-code semantics shared with [`kill_all`].
pub fn cancel_all(labels: &[crate::labels::OperatorLabel]) -> Result<(), RunnerError> {
    mutate_all(ControlCommand::Cancel, labels)
}

/// Client entry for `kill --all` (T-217): the aggregate counterpart to [`kill`]. See
/// [`mutate_all`].
pub fn kill_all(labels: &[crate::labels::OperatorLabel]) -> Result<(), RunnerError> {
    mutate_all(ControlCommand::Kill, labels)
}

/// One target's outcome in the aggregate report `cancel --all` / `kill --all` print.
#[derive(Debug, Serialize)]
pub struct ControlAllOutcome {
    /// The run id the snapshot entry recorded. It is descriptive, not the target key:
    /// aggregate dispatch is keyed by the unique registry-record path.
    pub run_id: String,
    /// Whether the live runner acknowledged the requested mutation.
    pub accepted: bool,
    /// The aggregate result. `already_gone` is successful without claiming an ack.
    pub status: ControlAllStatus,
    /// Why a failed target was not accepted. `None` for both successful statuses.
    pub error: Option<String>,
}

/// The three honest outcomes of dispatching one aggregate snapshot target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAllStatus {
    /// The runner acknowledged the verb.
    Accepted,
    /// The snapshot target finished before dispatch; the desired terminal state is
    /// already reached, but no runner accepted this invocation's verb.
    AlreadyGone,
    /// The target is still potentially live, but could not be safely reached or did
    /// not acknowledge the verb.
    Failed,
}

/// A confirmed-live registry entry captured before aggregate dispatch starts.
///
/// `run_id` is not unique. The record path is the stable key that lets `--all`
/// address duplicate ids independently, while the endpoint is the transport address
/// the same record advertised at snapshot time.
#[derive(Debug)]
struct SnapshotTarget {
    run_id: String,
    record_path: PathBuf,
    endpoint: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum SnapshotTargetState {
    Live,
    AlreadyGone,
}

#[derive(Debug)]
enum SnapshotMutation {
    Accepted(ControlAck),
    AlreadyGone,
}

#[derive(Debug)]
enum InspectSnapshot {
    Inspected(Snapshot),
    AlreadyGone,
}

/// Shared driver for [`cancel_all`] / [`kill_all`]: snapshot the registry's
/// confirmed-live entries once, apply `command` directly to each record's endpoint,
/// print the aggregate report, and turn any per-target failure into the reserved
/// [`exit::CONTROL`] aggregate outcome — never a silent `0` when part of the fan-out
/// failed (see `docs/control-plane.md`, "`cancel --all` / `kill --all`").
///
/// The snapshot is taken **once**, before any mutation is dispatched — the same
/// principle [`crate::wait::run_all`] (T-216) uses for its own target set — so a run
/// that registers mid-fan-out is out of scope for this invocation, and every target
/// this call acts on was confirmed live at one consistent instant. Snapshotting by
/// [`registry::Health::Live`] alone, before looking at any other field, mirrors
/// [`resolve_in_registry`]'s own health bar: a live entry with no endpoint yet is
/// still in scope, and its per-target mutation fails on its own rather than being
/// silently excluded from the snapshot up front. Unlike the by-id form, the target
/// key is the record path, so duplicate `run_id`s remain independently addressable.
fn mutate_all(
    command: ControlCommand,
    labels: &[crate::labels::OperatorLabel],
) -> Result<(), RunnerError> {
    let runtime = current_thread_runtime()?;
    runtime.block_on(mutate_all_async(command, labels))
}

/// The async body of [`mutate_all`]: snapshot, dispatch each record-specific target
/// sequentially, print the report, then map genuine failures onto the aggregate exit
/// code. A target that finishes before its turn is successful as `already_gone`.
async fn mutate_all_async(
    command: ControlCommand,
    labels: &[crate::labels::OperatorLabel],
) -> Result<(), RunnerError> {
    let action = command.verb();
    let registry = registry::open_read_only_for_setup()?;
    let targets = snapshot_live_targets(&registry, labels).map_err(registry::setup_read_error)?;

    let mut outcomes = Vec::with_capacity(targets.len());
    for target in targets {
        match mutate_snapshot_target(&registry, &target, command).await {
            Ok(SnapshotMutation::Accepted(ack)) => outcomes.push(ControlAllOutcome {
                run_id: ack.run_id,
                accepted: ack.accepted,
                status: ControlAllStatus::Accepted,
                error: None,
            }),
            Ok(SnapshotMutation::AlreadyGone) => outcomes.push(ControlAllOutcome {
                run_id: target.run_id,
                accepted: false,
                status: ControlAllStatus::AlreadyGone,
                error: None,
            }),
            Err(err) => outcomes.push(ControlAllOutcome {
                run_id: target.run_id,
                accepted: false,
                status: ControlAllStatus::Failed,
                error: Some(err.to_string()),
            }),
        }
    }

    let line = serde_json::to_string(&outcomes).map_err(|err| {
        RunnerError::new(
            exit::SETUP,
            format!("could not render the {action} --all report: {err}"),
        )
    })?;
    println!("{line}");

    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.status == ControlAllStatus::Failed)
        .count();
    if failed > 0 {
        return Err(RunnerError::new(
            exit::CONTROL,
            format!(
                "{action} --all: {failed} of {} target run(s) could not be reached or did not \
                 acknowledge the {action}; see the report above for the per-run reason",
                outcomes.len()
            ),
        ));
    }
    Ok(())
}

/// Snapshot every entry confirmed live, preserving the unique record path and the
/// endpoint that exact record advertised. `run_id` is deliberately only report data:
/// two live records may share it, and `--all` must still reach both.
fn snapshot_live_targets(
    registry: &registry::Registry,
    labels: &[crate::labels::OperatorLabel],
) -> io::Result<Vec<SnapshotTarget>> {
    Ok(registry
        .snapshot_live_entries()?
        .into_iter()
        .filter(|entry| crate::labels::matches(&entry.record.labels, labels))
        .map(|entry| SnapshotTarget {
            run_id: entry.record.run_id,
            record_path: entry.path,
            endpoint: entry.record.endpoint,
        })
        .collect())
}

/// Dispatch one aggregate target without resolving its non-unique `run_id` again.
///
/// A failed connect or conversation is reclassified only when the record-specific
/// liveness check confirms the target is now absent or stale. That is the desired
/// terminal state for mass teardown, so it is reported as `already_gone`; an
/// unprobeable or identity-changed record remains a hard failure.
async fn mutate_snapshot_target(
    registry: &registry::Registry,
    target: &SnapshotTarget,
    command: ControlCommand,
) -> Result<SnapshotMutation, RunnerError> {
    let action = command.verb();
    if snapshot_target_state(registry, target, action)? == SnapshotTargetState::AlreadyGone {
        return Ok(SnapshotMutation::AlreadyGone);
    }
    let Some(endpoint) = target.endpoint.as_deref() else {
        return Err(unreachable_run(
            action,
            &target.run_id,
            "the run is live but exposes no control endpoint".to_string(),
        ));
    };

    let stream = match connect_live(endpoint, action, &target.run_id).await {
        Ok(stream) => stream,
        Err(err) => {
            return match snapshot_target_state(registry, target, action)? {
                SnapshotTargetState::AlreadyGone => Ok(SnapshotMutation::AlreadyGone),
                SnapshotTargetState::Live => Err(err),
            };
        }
    };

    if snapshot_target_state(registry, target, action)? == SnapshotTargetState::AlreadyGone {
        return Ok(SnapshotMutation::AlreadyGone);
    }

    let ack: ControlAck =
        match converse_under_deadline(stream, command.verb(), action, &target.run_id).await {
            Ok(ack) => ack,
            Err(err) => {
                return match snapshot_target_state(registry, target, action)? {
                    SnapshotTargetState::AlreadyGone => Ok(SnapshotMutation::AlreadyGone),
                    SnapshotTargetState::Live => Err(err),
                };
            }
        };

    if !ack_matches(&ack, action, &target.run_id) {
        return Err(unreachable_run(
            action,
            &target.run_id,
            "the runner did not acknowledge the command for the snapshotted target".to_string(),
        ));
    }
    Ok(SnapshotMutation::Accepted(ack))
}

/// Reconfirm the exact registry record captured by [`snapshot_live_targets`].
/// Aggregate control and aggregate inspect share this step: missing and
/// confirmed-stale records are successful terminal states, while unprobeable records
/// and identity changes are not evidence that the target ended.
fn snapshot_target_state(
    registry: &registry::Registry,
    target: &SnapshotTarget,
    action: &str,
) -> Result<SnapshotTargetState, RunnerError> {
    let entry = registry.probe_entry(&target.record_path).map_err(|err| {
        unreachable_run(
            action,
            &target.run_id,
            format!("could not re-read the snapshotted registry record: {err}"),
        )
    })?;
    let Some(entry) = entry else {
        return Ok(SnapshotTargetState::AlreadyGone);
    };

    match entry.health {
        Health::Stale => Ok(SnapshotTargetState::AlreadyGone),
        Health::Unprobed => Err(unreachable_run(
            action,
            &target.run_id,
            "the snapshotted entry's liveness could not be re-probed, so it is not confirmed gone"
                .to_string(),
        )),
        Health::Live => {
            if entry.record.run_id != target.run_id || entry.record.endpoint != target.endpoint {
                return Err(unreachable_run(
                    action,
                    &target.run_id,
                    "the snapshotted registry record changed identity before dispatch; refusing to act on its replacement"
                        .to_string(),
                ));
            }
            Ok(SnapshotTargetState::Live)
        }
    }
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
    registry::Registry::open_read_only().map_err(|err| {
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
    // endpoint — before ever looking at endpoints. `register` (`src/registry/mod.rs`)
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
    // Say *why* it's unreachable — the run is gone (stale), its liveness could not be
    // probed at all (unprobed), or it predates the transport (live, no endpoint) —
    // rather than a generic failure.
    let Some(entry) = live.into_iter().next() else {
        return Err(no_live_entry(action, run_id, &matches));
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
    if !endpoint_is_valid(endpoint) {
        return Err(unreachable_run(
            action,
            run_id,
            "the registry entry contains an invalid control endpoint".into(),
        ));
    }
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

/// Send one request `verb` and parse the runner's one-line JSON reply as `T` —
/// [`Snapshot`] for `inspect`, [`ControlAck`] for `cancel`/`kill`. The wire exchange
/// is identical for every verb (write the verb line, flush, read one line back); only
/// the verb sent and the reply type parsed differ, which is exactly what `T`
/// parameterizes. A closed connection before a complete line (runner died
/// mid-conversation), a reply over [`MAX_LINE_BYTES`] with no terminating `\n`
/// (bounded by [`read_bounded_line`], the same ceiling [`serve_one`] reads its
/// request under), or an unparsable line all surface as the same `io::Error` shape
/// the caller maps to [`exit::CONTROL`].
///
/// A reply that fails to parse as `T` is not necessarily garbage: `serve_one` answers
/// an unrecognized verb or an oversized request line with a structured
/// [`ErrorResponse`] (`{"error": "..."}`), which this client never asked for and so
/// does not decode as `T`. Before giving up, this retries the same line as the owned
/// [`ErrorReply`] and, if that parses, surfaces its (normalized — see
/// [`normalize_peer_error_text`]) `error` text verbatim — the server's own diagnostic
/// (e.g. "control request rejected: line exceeded ...") rather than a generic
/// "unreadable response" wrapped around a `serde` field-mismatch message. A line that
/// parses as neither `T` nor `ErrorReply` still falls through to that generic message
/// unchanged.
async fn converse<S, T>(stream: S, verb: &str) -> io::Result<T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: serde::de::DeserializeOwned,
{
    let (read_half, mut write_half) = split(stream);
    write_half.write_all(verb.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read = read_bounded_line(&mut reader, &mut line).await?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the runner closed the connection before answering (it may have just exited)",
        ));
    }
    let trimmed = line.trim();
    serde_json::from_str::<T>(trimmed).map_err(|err| {
        match serde_json::from_str::<ErrorReply>(trimmed) {
            Ok(error_reply) => io::Error::new(
                io::ErrorKind::InvalidData,
                normalize_peer_error_text(&error_reply.error),
            ),
            Err(_) => io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the runner sent an unreadable response: {err}"),
            ),
        }
    })
}

/// A cap on how much of a peer's `error` text [`converse`] surfaces verbatim, well
/// under [`MAX_LINE_BYTES`] (the peer's whole reply line, envelope included, is
/// already bounded there) — this only guards against a needlessly huge single
/// diagnostic dominating the CLI's output.
const MAX_PEER_ERROR_CHARS: usize = 500;

/// Make a peer-supplied error string safe to splice verbatim into a one-line CLI
/// diagnostic: since [`ErrorReply`] now accepts any escaping the sender's JSON
/// encoder produced, the text can contain newlines or other control characters that
/// would otherwise let a peer reformat the CLI's output. Collapses control
/// characters (including `\n`/`\r`/`\t`) to spaces, trims the result, and truncates
/// to [`MAX_PEER_ERROR_CHARS`].
fn normalize_peer_error_text(text: &str) -> String {
    let single_line = crate::text::terminal_safe(text);
    let trimmed = single_line.trim();
    if trimmed.chars().count() > MAX_PEER_ERROR_CHARS {
        let truncated: String = trimmed.chars().take(MAX_PEER_ERROR_CHARS).collect();
        format!("{truncated}... (truncated)")
    } else {
        trimmed.to_string()
    }
}

/// Run [`converse`] under [`CONVERSATION_DEADLINE`] and map both ways a runner can be
/// lost mid-exchange — it never answers in time, or it answers with something
/// [`converse`] cannot parse back — onto the same distinguishable [`unreachable_run`]
/// failure every other runner-loss path in this module uses. Shared by
/// [`inspect_async`] (`T` = [`Snapshot`]) and [`mutate_async`] (`T` = [`ControlAck`]).
async fn converse_under_deadline<S, T>(
    stream: S,
    verb: &str,
    action: &str,
    run_id: &str,
) -> Result<T, RunnerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: serde::de::DeserializeOwned,
{
    tokio::time::timeout(CONVERSATION_DEADLINE, converse::<S, T>(stream, verb))
        .await
        .map_err(|_| unreachable_run(action, run_id, "the runner did not answer in time".into()))?
        .map_err(|err| unreachable_run(action, run_id, err.to_string()))
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

/// The "no confirmed-live entry matches `run_id`" verdict, worded by *which* non-live
/// health the matching records actually carry — the single place the control clients
/// choose between "the runner is gone" and "liveness is unknown".
///
/// [`registry::Registry::entries`] keeps a **confirmed**-stale record
/// ([`registry::Health::Stale`] — the probe ran and found no holder) apart from one
/// whose probe could not run at all ([`registry::Health::Unprobed`], T-206), and the
/// distinction is load-bearing for the operator even though it changes nothing about
/// what the client *does*: every verb here still refuses, because it acts only on
/// [`registry::Health::Live`]. What it must not do is *assert* the runner exited when
/// nothing established that. Saying "the runner is gone" for an unprobeable entry
/// would be the very unconfirmed positive claim `list` stopped making (see
/// `docs/registry.md`, "Discovery — `list`"), and would send an operator following
/// `docs/troubleshooting.md`'s "cross-check with `list`" advice to a record `list`
/// prints as `unprobed` — two surfaces contradicting each other about one record.
///
/// A single unprobeable record among the matches is enough to withhold the stronger
/// claim, mirroring [`registry::Registry::probe_run`], which reports
/// [`registry::RunStatus::Unprobed`] rather than `Finished` whenever any matching
/// record could not be probed: an unprobeable entry is not evidence of anything, so it
/// cannot be outvoted by a confirmed-stale sibling.
fn no_live_entry(action: &str, run_id: &str, matches: &[registry::Entry]) -> RunnerError {
    if matches.iter().any(|entry| entry.health == Health::Unprobed) {
        return unreachable_run(
            action,
            run_id,
            "its liveness could not be probed — the entry's lock file would not open, or the \
             lock call itself failed — so the runner is not confirmed gone; `list` reports \
             this entry as `unprobed`"
                .to_string(),
        );
    }
    unreachable_run(
        action,
        run_id,
        "its registry entry is stale — the runner is gone (it exited without cleaning up)"
            .to_string(),
    )
}

/// An "ambiguous run id" error: `count` distinct live registry entries share
/// `run_id`, so the client refuses to guess which one `action` means — reserving the
/// same [`exit::CONTROL`] code as every other unreachable-run result (still "could
/// not reach *the* target run": there is no single one to reach). See
/// `docs/registry.md`, "Run id resolution — ambiguity is a hard failure".
///
/// Shared, as the one place this verdict is worded, by both kinds of by-`run-id`
/// client: the control-plane verbs here (through [`resolve_in_registry`]) and the
/// registry-only [`crate::wait`], which reaches the identical conclusion from its own
/// scan without ever contacting a runner. `pub(crate)` for that second caller only —
/// it is not part of any exported surface.
pub(crate) fn ambiguous_run(action: &str, run_id: &str, count: usize) -> RunnerError {
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

/// The fixed final component of a unix control endpoint: the socket file itself,
/// inside its own private directory. Deliberately short — the whole path has to fit
/// `sockaddr_un::sun_path` on the *shortest* platform (see [`imp`]).
///
/// Public to the crate because the endpoint shape is no longer known only to its
/// producer: [`crate::registry::Registry::prune`] reaps the socket a
/// confirmed-stale record published, and validates the record's `endpoint` against
/// exactly this shape before deleting anything (T-207). Both sides read the shape
/// from these two constants and [`socket_base_dirs`], so the producer and the reaper
/// cannot drift apart.
#[cfg(unix)]
pub(crate) const SOCKET_FILE_NAME: &str = "c.sock";

/// The fixed prefix of the per-run private directory that holds a unix control
/// socket ([`SOCKET_FILE_NAME`]); the rest of the name is a [`unique_token`].
#[cfg(unix)]
pub(crate) const SOCKET_DIR_PREFIX: &str = "pkc-";

/// Parse the platform-produced lexical shape of a unix control endpoint and return
/// its private socket directory. This intentionally does not constrain the base
/// directory: a client can inherit a different `TMPDIR` from the live runner that
/// published the record, while the endpoint itself remains legitimate.
#[cfg(unix)]
pub(crate) fn unix_control_endpoint_dir(endpoint: &str) -> Option<std::path::PathBuf> {
    if endpoint.chars().any(char::is_control) {
        return None;
    }

    let mut segments = endpoint.split('/');
    if segments.next() != Some("") {
        return None;
    }
    let segments: Vec<&str> = segments.collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return None;
    }

    let [base_segments @ .., dir_name, file_name] = segments.as_slice() else {
        return None;
    };
    if *file_name != SOCKET_FILE_NAME {
        return None;
    }
    let token = dir_name.strip_prefix(SOCKET_DIR_PREFIX)?;
    if token.is_empty()
        || !token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return None;
    }

    let base = std::path::PathBuf::from(format!("/{}", base_segments.join("/")));
    Some(base.join(dir_name))
}

#[cfg(unix)]
fn endpoint_is_valid(endpoint: &str) -> bool {
    unix_control_endpoint_dir(endpoint).is_some()
}

#[cfg(windows)]
const PIPE_ENDPOINT_PREFIX: &str = r"\\.\pipe\processkit-cli-";

#[cfg(windows)]
fn endpoint_is_valid(endpoint: &str) -> bool {
    let Some(token) = endpoint.strip_prefix(PIPE_ENDPOINT_PREFIX) else {
        return false;
    };
    !token.is_empty()
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

/// The base directories a unix control socket's private directory is created in, in
/// preference order: `/tmp` first (short enough to keep the advertised socket path
/// comfortably below `SUN_LEN` even when the registry itself lives under a deeply
/// nested CI workspace), then the platform temp directory when that differs.
///
/// Shared by the producer ([`imp::ControlServer::bind`], via
/// `create_private_socket_dir`) and by the reaper
/// ([`crate::registry::Registry::prune`], which refuses to delete a published
/// endpoint that does not sit directly inside one of these).
#[cfg(unix)]
pub(crate) fn socket_base_dirs() -> Vec<std::path::PathBuf> {
    let mut bases = vec![std::path::PathBuf::from("/tmp")];
    let platform_temp = std::env::temp_dir();
    if platform_temp != bases[0] {
        bases.push(platform_temp);
    }
    bases
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
#[path = "platform/unix.rs"]
mod imp;

#[cfg(windows)]
#[path = "platform/windows.rs"]
mod imp;

#[cfg(test)]
mod tests;
