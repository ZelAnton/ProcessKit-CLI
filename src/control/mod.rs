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
//! by-`run-id` verb — `inspect`, `attest`, `cancel`, and `kill` alike — reports the
//! same reserved [`exit::CONTROL`] (103) "ambiguous run id" failure rather than
//! acting on whichever
//! entry the directory scan happens to return first. For the mutating verbs this is
//! load-bearing (a wrong guess cancels or kills the *other* run); the read-only
//! `inspect` and `attest` get the identical hard failure rather than a softer
//! fallback, because a snapshot — or a membership verdict — that names the wrong run
//! is exactly as misleading as acting on it. See
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
//! ## The snapshot version a runner declares — checked, and acted on
//!
//! `inspect`'s reply is one of the two that carry a version — `attest`'s is the
//! other, on the separate and deliberately stricter [`ATTESTATION_VERSION`] axis
//! documented there — and the client does not merely print it. A reply is rendered
//! only if it declares a version this build can
//! actually decode — the range [`MIN_READABLE_SNAPSHOT_VERSION`]`..=`[`SNAPSHOT_VERSION`]
//! — and anything outside it is refused with the same reserved [`exit::CONTROL`]
//! (103) result every other "no snapshot you can trust" outcome reports, without
//! reaching the rendering step at all.
//!
//! **The refusal is one-sided on purpose, and that is the decision this policy
//! settles rather than leaves implicit:**
//!
//! - A version **above** [`SNAPSHOT_VERSION`] is refused. Its number is the runner's
//!   statement that the shape moved on in some way this build predates, and this
//!   build cannot know which way — there is no decoder here for a contract written
//!   after it. Rendering it anyway would present a payload interpreted under
//!   semantics its sender never promised, and would do so quietly: [`Snapshot`] is
//!   not `deny_unknown_fields`, so whatever a newer runner added is dropped at
//!   deserialization and never reaches stdout ([K-092]) — a confident rendering with
//!   no marker of what was lost. This is the mixed-binaries case a mid-upgrade user
//!   really has, and the one this check exists for: an older `inspect` pointed at a
//!   newer runner.
//! - A version **below** it, down to [`MIN_READABLE_SNAPSHOT_VERSION`], is **read**,
//!   because this build genuinely decodes it. That is not an assumption about what a
//!   bump means in general; it is a fact about the only bump this contract has had.
//!   1 → 2 (`7bed80c824b1`) added [`Snapshot::jsonl`] and [`Snapshot::capture_dir`],
//!   both `Option` + `#[serde(default)]`, and changed nothing else, so a version-1
//!   reply parses into exactly what version 1 promised with those two `None` — "not
//!   reported", which is precisely what a version-1 runner meant. Refusing it would
//!   delete a working, previously documented capability — every binary this project
//!   has released so far (v0.1.0 … v0.3.1) writes version 1 — and would justify the
//!   deletion with a claim ("no decoder for that form") this build's own
//!   `#[serde(default)]` contradicts.
//!
//! The asymmetry is the honest reading of what a version number can tell a reader:
//! it can say "I am newer than you", which is unknowable here and so must fail
//! closed, but it cannot make a payload this build demonstrably parses unreadable.
//! Where an older version *does* become unreadable — a bump that removes or renames
//! a field, or changes an existing one's meaning — the floor moves in that same
//! change (see [`MIN_READABLE_SNAPSHOT_VERSION`]). The range is where that judgement
//! is recorded explicitly, instead of being inferred from the number.
//!
//! This is a narrower refusal than the registry read side's, which skips any record
//! whose [`registry::REGISTRY_VERSION`] is not exactly its own, and the difference is
//! earned rather than a softening: that check gates *destructive* action — probing a
//! lock file and reaping the record behind it — on liveness semantics an unknown
//! version may have redefined, so a record it cannot vouch for must not be acted on
//! at all. A snapshot is read-only output whose only failure mode is being *misread*,
//! which is exactly what the range check prevents.
//!
//! Both consumers of a *snapshot* reply — [`inspect_endpoint`] for `inspect --run-id`
//! and [`inspect_snapshot_target`] for `inspect --all` — go through the one shared
//! acceptance step ([`SnapshotReply::accept`], which also carries the run-identity
//! check the two call sites already shared), so neither path can drift into a weaker
//! bar than the other. The version verdict is reached **before** the payload's shape
//! is parsed (see [`SnapshotReply`]'s `Deserialize`), so a newer runner whose shape
//! this build cannot even deserialize still gets the version diagnostic — the
//! actionable one — instead of a `serde` field complaint.
//!
//! ## Wire protocol
//!
//! Line-oriented and deliberately tiny. A client writes one request verb line
//! (`inspect\n`; an empty line is also treated as `inspect`) and reads back one JSON
//! line, then the server closes the connection. Four verbs share this one framing —
//! two read-only, two mutating:
//!
//! - **`inspect`** — read-only; the reply is a [`Snapshot`].
//! - **`attest`** — read-only; the reply is an [`Attestation`]. Like the others it
//!   carries no argument, and here that is load-bearing rather than incidental: the
//!   identity the verdict is about is the one the transport reports for the
//!   connecting client, never one the request could name.
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
//! the same `io::Error` shape an unparsable reply already does. The runner applies
//! the same bound before writing an `inspect` snapshot, counting the terminating
//! newline; a snapshot that cannot fit becomes a bounded structured error instead
//! of an oversized or truncated reply.

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

use crate::error_envelope::ErrorKind;
use crate::events::{self, Member};
use crate::exit::{self, RunnerError};
use crate::registry::{self, Health};

mod render;

pub use imp::ControlServer;
use render::attestation_output_lines;
use render::inspect_all_output_lines;
#[cfg(test)]
use render::render_snapshot_human;
use render::snapshot_output_lines;

/// Control-plane snapshot format version. Independent of the JSONL event
/// [`schema_version`](crate::events::SCHEMA_VERSION) and the
/// [`registry_version`](crate::registry::REGISTRY_VERSION): the `inspect` response is
/// the control plane's own private client/runner contract, so it versions on its own
/// axis.
///
/// **What a bump says — and what it does not.** It says the snapshot's shape changed
/// in a way a reader should know about. It does **not**, on its own, say "breaking":
/// the only bump this contract has had, 1 → 2 (`7bed80c824b1`), was purely additive —
/// it introduced [`Snapshot::jsonl`] and [`Snapshot::capture_dir`], both `Option` +
/// `#[serde(default)]`, and touched no existing field. A reader must therefore not
/// infer "I cannot decode this" from the mere fact that a number differs; how far
/// back this build really decodes is stated outright in
/// [`MIN_READABLE_SNAPSHOT_VERSION`] and enforced by
/// [`snapshot_version_is_readable`], which is the whole point of keeping that second
/// constant next to this one.
///
/// **The read side acts on it (T-292).** The client refuses a reply declaring a
/// version **newer** than this one — unknown semantics it holds no decoder for — and
/// reads one declaring an older version down to [`MIN_READABLE_SNAPSHOT_VERSION`].
/// See the module doc, "The snapshot version a runner declares — checked, and acted
/// on", for why the refusal is one-sided, and `docs/control-plane.md`, "Snapshot
/// version: a newer runner's reply is refused, an older one is read", for the
/// operator-facing statement.
///
/// **What bumping it costs.** Every client older than the new number stops being
/// able to inspect a runner that writes it — loudly, with `CONTROL` (103), rather
/// than by misreading it. Clients *newer* than a given runner keep working as long
/// as the floor below allows, so a bump is not automatically a fleet-wide outage;
/// deciding whether the floor moves with it is part of making the bump.
pub const SNAPSHOT_VERSION: u32 = 2;

/// The oldest [`Snapshot::snapshot_version`] this build still decodes **correctly** —
/// the explicit floor of the read side's accepted range, and the record of a
/// judgement that is otherwise invisible in the code.
///
/// **Why it is 1.** Version 1's shape is exactly this build's minus
/// [`Snapshot::jsonl`] and [`Snapshot::capture_dir`]: the 1 → 2 bump added those two
/// and nothing else, leaving every other field's name, type, and meaning untouched
/// (verified against the bump's own parent, `0661733425fc` → `7bed80c824b1`). Both
/// are `Option` + `#[serde(default)]`, so a version-1 reply reads back with them
/// `None` — "not reported", the same value this build publishes for a run whose
/// capture is disabled — rather than failing to parse or, worse, being silently
/// filled in with something its sender never said. That is the identical "old
/// record, new reader" argument [`registry::REGISTRY_VERSION`]'s doc makes for its
/// own additive fields, and it is a checkable fact about this repository rather than
/// a policy about numbers. It matters in practice: every binary released so far
/// (v0.1.0 … v0.3.1) writes version 1, so this floor is what keeps a freshly
/// upgraded client able to inspect the runs its predecessor started.
///
/// **When to raise it.** In the same change as any bump that makes the older shape
/// undecodable *or* misleading — a removed or renamed field this build still
/// requires, a changed type, or, most importantly, an existing field whose *meaning*
/// changed (that one still parses, which is precisely why the floor, not the parser,
/// has to catch it). Raising it is a user-visible tightening: it is announced in
/// `CHANGELOG.md` and it narrows the `snapshot_version` values
/// `fixtures/schema/cli/inspect.schema.json` allows on stdout, so those two move with
/// it.
pub const MIN_READABLE_SNAPSHOT_VERSION: u32 = 1;

/// Version of the `attest` reply's field set **and of what its verdicts mean** — an
/// axis of its own, exactly as [`SNAPSHOT_VERSION`] is for `inspect` (the two verbs
/// answer with different contracts and there is no reason a change to one should
/// invalidate the other).
///
/// **The read side is strict, and that is a decision, not a copied shape.** A client
/// renders a reply only when it declares exactly this number
/// ([`AttestationReply::accept`]); anything else is refused with [`exit::CONTROL`]
/// (103) and [`ErrorKind::IncompatibleContract`], never interpreted. `inspect`'s
/// version check is deliberately *not* strict — it reads down to
/// [`MIN_READABLE_SNAPSHOT_VERSION`], because refusing a version-1 snapshot would
/// delete a capability every released binary still provides — and the difference
/// between the two is the difference in what a misread costs and in what history
/// each contract has:
///
/// - a misread **snapshot** is a diagnostic rendered under the wrong semantics; a
///   misread **attestation** is a security verdict — an adapter gating a lease on it
///   would grant or deny access on a sentence its sender never said. The honest
///   answer to "I do not know this contract" is to decline to answer;
/// - and there is nothing to lose by declining: this contract has had exactly one
///   version, so strictness refuses no shape that has ever existed. `inspect`'s floor
///   records a *checked* fact about a bump that really happened; asserting the same
///   tolerance here in advance would be a claim about bumps not yet made — the
///   premise that has to be verified rather than assumed (`fixtures/schema/cli/`'s
///   own "pick `const` vs a range per whether the reader is strict", and why
///   `error.schema.json` pins `error_version` with `const`).
///
/// A later additive bump can widen this to a range in the same change that makes the
/// widening true, exactly as `inspect`'s floor was written when its bump landed.
pub const ATTESTATION_VERSION: u32 = 1;

/// The read-only request verb. An empty request line is treated as this too, so a
/// bare connect-and-read probe still gets a snapshot.
const INSPECT_REQUEST: &str = "inspect";

/// The mutating verb that ends a run through the shared soft-stop → grace →
/// hard-kill teardown (the network analogue of a `Ctrl-C`).
const CANCEL_REQUEST: &str = "cancel";

/// The mutating verb that hard-kills a run's whole tree immediately (no grace).
const KILL_REQUEST: &str = "kill";

/// The read-only verb that asks the runner whether **the asking process itself** is
/// inside this run's container. It carries no argument at all — deliberately: the
/// identity it is answered about is the one the transport reports, never one the
/// request could name (see [`Attestation`]).
const ATTEST_REQUEST: &str = "attest";

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
/// the client-side JSON decode of the reply shapes a client parses, which since
/// T-306 include [`AttestationReply`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum RequestVerb {
    /// The read-only request. Named explicitly or by an empty line.
    Inspect,
    /// The mutating soft-stop → grace → hard-kill request.
    Cancel,
    /// The mutating immediate hard-kill request.
    Kill,
    /// The read-only containment-membership request (see [`Attestation`]).
    Attest,
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
        ATTEST_REQUEST => Some(RequestVerb::Attest),
        _ => None,
    }
}

/// Who is on the other end of one accepted control connection, as the **kernel**
/// reports it — the whole basis of [`Attestation`], and the reason `attest` takes no
/// pid argument.
///
/// It is obtained from the transport itself (unix peer credentials, or
/// `GetNamedPipeClientProcessId` on the Windows named pipe — see each platform
/// module's `peer_identity`) at accept time, before the request line is even read,
/// so:
///
/// - **the client cannot choose it.** Nothing a request could contain reaches this
///   value. A pid a caller supplied would only ever prove that *some* process is a
///   member, which is not the question an adapter gating on "the caller is inside
///   run X" is actually asking;
/// - **pid reuse cannot turn a departed client into a false positive.** The
///   connection is open when this is read and stays open until the reply is written,
///   and a process with an open socket/pipe has not exited, so the number cannot yet
///   have been recycled onto a different process.
///
/// [`Self::Unavailable`] is a first-class outcome rather than an error to swallow:
/// the platforms that cannot answer this question must **say so** (and are answered
/// with [`AttestVerdict::PeerIdentityUnsupported`]), never fall back to an
/// unauthenticated identity or to an unproven "ok".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerIdentity {
    /// The kernel named the connecting process, and the number is one that can be
    /// compared against this container's members.
    Pid(u32),
    /// No kernel-authenticated pid could be obtained for this connection — either
    /// the platform has no such facility, or the call failed. Never a guess.
    Unavailable,
}

/// Whether this build obtains a kernel-authenticated peer identity on **this**
/// target, as a compile-time guarantee — the capability `attest` rests on, and the
/// fact `probe --json` publishes as the surface token `attest:peer-identity` so a
/// consumer can establish it at preflight instead of discovering it mid-run (see
/// [`crate::probe`], `docs/integration.md`).
///
/// The per-target reasoning lives in each platform module's own
/// `PEER_IDENTITY_SUPPORTED` (`src/control/platform/{unix,windows}.rs`), because
/// that is where the system calls it describes are made.
///
/// **The claim is one-directional, on purpose.** `true` is a guarantee: this target
/// always names the peer, so a `not_a_member` answer from it is a real verdict rather
/// than a missing capability in disguise. `false` withholds that guarantee — it does
/// **not** predict that `attest` will fail. One target (FreeBSD) supplies a peer pid
/// on a new enough kernel and not on an older one, which no compile-time constant can
/// tell apart, so it is excluded here rather than over-claimed; `attest` there still
/// answers from whatever the kernel actually provides, and fails closed with
/// [`AttestVerdict::PeerIdentityUnsupported`] when that is nothing. Under-claiming is
/// the safe direction for a fail-closed preflight: a consumer that requires the token
/// declines to rely on attestation, which is never worse than relying on one that
/// might not be there.
pub const PEER_IDENTITY_SUPPORTED: bool = imp::PEER_IDENTITY_SUPPORTED;

/// The name of the local transport this build binds — `unix_socket` or
/// `windows_named_pipe`.
///
/// Owned here, next to the [`imp`] module that implements both, so the one place a
/// report names the transport ([`crate::doctor`]) reads it from the transport rather
/// than restating it. Not part of any wire contract: it is a fact about this build,
/// published only in the human/JSON doctor report
/// (`fixtures/schema/cli/doctor.schema.json`).
#[cfg(unix)]
pub(crate) const TRANSPORT: &str = "unix_socket";
#[cfg(windows)]
pub(crate) const TRANSPORT: &str = "windows_named_pipe";

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
/// request verb ([`serve_one`]) and the JSON reply ([`converse`]). The terminating
/// newline counts against this ceiling, so a response payload may occupy at most
/// `MAX_LINE_BYTES - 1` bytes. The protocol is deliberately tiny (module doc,
/// "Wire protocol") — a request line is `inspect` / `cancel` / `kill` / empty plus
/// `\n` (a handful of bytes), and a reply line is a [`Snapshot`] (JSON of a handful
/// of scalar fields plus an enriched `members` array), a [`ControlAck`], or an error
/// object. A snapshot whose complete JSON payload cannot fit is represented by the
/// bounded [`SNAPSHOT_TOO_LARGE_ERROR`] response; it is never truncated or emitted
/// over the reader's limit.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// The stable diagnostic returned when the complete `inspect` snapshot cannot fit
/// in the one-line control-plane response. Keep this short and fixed: the fallback
/// itself must always fit the same bound it protects.
const SNAPSHOT_TOO_LARGE_ERROR: &str =
    "control-plane inspect snapshot exceeds the 65536-byte response limit";

/// The machine-readable state `inspect` prints: what a control-plane client can learn
/// about a live run. `Serialize` on the server side, `Deserialize` on the client side
/// (which parses the reply back before printing it, so a truncated/garbled response
/// from a runner dying mid-write is caught rather than echoed).
#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// Snapshot format version — [`SNAPSHOT_VERSION`] when this build is the runner.
    ///
    /// The one field here whose value genuinely originates on the *far* side of the
    /// wire rather than in this client's own re-serialization ([K-092]): the runner
    /// declares which contract its reply follows, and the client both acts on that
    /// declaration and reports it unchanged. Acting on it means the range check in
    /// [`snapshot_version_is_readable`] — a newer version is refused before the
    /// snapshot reaches rendering, an older one down to
    /// [`MIN_READABLE_SNAPSHOT_VERSION`] is read — so a rendered value is always
    /// inside that range, and is the *runner's* number, not necessarily this
    /// build's. See the module doc, "The snapshot version a runner declares —
    /// checked, and acted on".
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
    /// Absolute path to the run's JSONL lifecycle stream, or `null` when reading a
    /// version-1 snapshot, which had no such field.
    ///
    /// A runner of this [`SNAPSHOT_VERSION`] always publishes a path. The `Option` +
    /// `#[serde(default)]` is what makes the older contract readable at all — it is
    /// the decoder [`MIN_READABLE_SNAPSHOT_VERSION`] rests on, not a formality — and
    /// `None` reports exactly what a version-1 runner meant: the field was never
    /// declared, so nothing is claimed about where the stream is.
    #[serde(default)]
    pub jsonl: Option<String>,
    /// Absolute output-capture directory, or `null` when capture is disabled.
    #[serde(default)]
    pub capture_dir: Option<String>,
    /// A point-in-time snapshot of the container's members, enriched with
    /// `ppid`/executable `name`/`start_time` wherever `members_info()` can report
    /// them, mirroring the JSONL `members_snapshot`'s own `members` entries
    /// (`docs/schema.md`, "Enriched member fields") — the shared contract is the
    /// [`Member`] shape, not that event's envelope (its `reason` is the event's
    /// alone). Queried at request time, so it reflects the container's
    /// composition *when inspected*, not at start.
    pub members: Vec<Member>,
}

/// An `inspect` reply as it comes off the wire, decoded in the order its contract is
/// decided in: the declared `snapshot_version` first, the payload's shape second.
///
/// Deserializing straight into [`Snapshot`] settles those two in the opposite order,
/// and the difference is precisely the case the version check exists for. Every field
/// but [`Snapshot::jsonl`]/[`Snapshot::capture_dir`] is required, so a newer runner's
/// genuinely breaking change — a removed, renamed, or retyped field — fails `serde`
/// first, and the operator gets "the runner sent an unreadable response: missing
/// field `mechanism`": a parser complaining about a payload this client was never
/// entitled to read, instead of "the runner answered with control-plane snapshot
/// version 3, and this client reads versions 1 to 2", the one diagnostic that names
/// the real problem and its fix. Reading the declared version off the raw JSON first
/// makes the version verdict unconditional, which is what lets `docs/control-plane.md`
/// promise that *every* refused version — not only the ones that happen to still
/// parse — is reported by naming the version that arrived and the range this build
/// reads.
///
/// A reply whose `snapshot_version` is missing, negative, or not a whole number
/// carries no version verdict to reach: it is a malformed snapshot, so it falls
/// through to the full parse and surfaces `serde`'s own diagnostic exactly as before.
///
/// `#[doc(hidden)] pub` for the same reason [`ErrorReply`] is: the `control_wire`
/// fuzz target drives the exact types [`converse`] parses a reply line into, and
/// this is now that type for the `inspect` verb.
#[derive(Debug)]
#[doc(hidden)]
pub enum SnapshotReply {
    /// A reply declaring a version inside this build's readable range
    /// ([`snapshot_version_is_readable`]), parsed into the shape this build
    /// implements.
    Readable(Snapshot),
    /// A reply declaring a version outside that range, carrying only the number the
    /// runner declared — the payload was deliberately *not* interpreted, and that
    /// number is the whole actionable content of the refusal.
    Unreadable(u64),
}

impl<'de> Deserialize<'de> for SnapshotReply {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(declared) = value
            .get("snapshot_version")
            .and_then(serde_json::Value::as_u64)
            && !snapshot_version_is_readable(declared)
        {
            return Ok(Self::Unreadable(declared));
        }
        serde_json::from_value(value)
            .map(Self::Readable)
            .map_err(serde::de::Error::custom)
    }
}

/// Whether this build decodes a snapshot declaring `declared` — the read side's whole
/// version policy, in one place both `inspect` forms reach through
/// [`SnapshotReply`]. See [`SNAPSHOT_VERSION`] and
/// [`MIN_READABLE_SNAPSHOT_VERSION`] for what each end of the range means and when it
/// moves. Takes the `u64` a JSON number yields rather than a `u32`, so a wildly
/// out-of-range declaration is refused by *this* rule, with the number the runner
/// actually sent in the message, instead of being lost in a cast.
fn snapshot_version_is_readable(declared: u64) -> bool {
    (u64::from(MIN_READABLE_SNAPSHOT_VERSION)..=u64::from(SNAPSHOT_VERSION)).contains(&declared)
}

/// The error line a server sends for an unrecognized request verb. The `inspect`
/// client never asks for anything else, so it only ever sees a [`Snapshot`]; this
/// exists so a future/foreign client gets a structured answer rather than silence.
///
/// `error: &'a str` borrows the diagnostic on the *serialize* side, but that shape
/// cannot be reused to deserialize a reply: serde can only borrow a JSON string field
/// when it contains no escape sequence, so a server's diagnostic that happens to
/// include a quote, backslash, or control character (a Windows named-pipe path, for
/// instance) would fail to parse as this type and fall through to the generic
/// "unreadable response" message the fallback exists to avoid. [`converse`] instead
/// deserializes replies into the owned [`ErrorReply`] sibling below, which always
/// parses regardless of escaping. [`serialize_error`] also bounds the serialized
/// envelope, so a long diagnostic cannot make the response line unreadable.
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
    ///
    /// `None` means the read itself **failed**, and it is deliberately not the same
    /// value as `Some(vec![])`. The two consumers need that distinction differently:
    /// a [`Snapshot`] degrades a failed read to an empty list exactly as it always
    /// has (a diagnostic that reports nothing rather than not being produced), while
    /// [`SnapshotSource::attest`] must not — "I could not read my members" is not
    /// "you are not one of them", and reporting the second for the first would be a
    /// verdict nothing established (the honest-degradation discipline the JSONL
    /// `members_snapshot`'s own `read_error` flag follows).
    members: &'a (dyn Fn() -> Option<Vec<Member>> + 'a),
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
        members: &'a (dyn Fn() -> Option<Vec<Member>> + 'a),
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
            // A failed member read degrades to an empty list here, unchanged from
            // before this closure could report the failure at all: an `inspect`
            // snapshot is a diagnostic, and one that reports no members is still a
            // snapshot. `attest` treats the same `None` quite differently — see
            // [`SnapshotSource::members`].
            members: (self.members)().unwrap_or_default(),
        }
    }

    /// Decide, right now, whether `peer` is inside this run's container, and build
    /// the [`Attestation`] that says so.
    ///
    /// The member list is queried **live**, through the very same closure the
    /// `inspect` snapshot uses — one `ProcessGroup::members_info()` enrichment path
    /// for the whole binary, so what counts as "a container member" cannot mean one
    /// thing here and another in the JSONL `members_snapshot` or an `inspect` reply.
    /// It is queried *while the peer's connection is open*, which is what makes the
    /// answer about the process that asked rather than about a pid that has since
    /// been recycled.
    fn attest(&self, peer: PeerIdentity) -> Result<Attestation, &'static str> {
        let (verdict, peer_pid) = match peer {
            // No member read is even attempted: without an identity there is
            // nothing to look for, and the refusal is about the identity.
            PeerIdentity::Unavailable => (AttestVerdict::PeerIdentityUnsupported, None),
            PeerIdentity::Pid(pid) => {
                let Some(members) = (self.members)() else {
                    // The peer *was* named; the container's own membership could not
                    // be read. There is no honest verdict here — `not_a_member`
                    // would claim something nothing established — so no attestation
                    // is produced at all and the client is told why (it reports the
                    // established `CONTROL` "no answer you can act on", carrying this
                    // text).
                    return Err(
                        "the runner could not read its own container membership, so it \
                         refused to decide whether this client is a member",
                    );
                };
                let verdict = if peer_is_member(pid, self.mechanism, &members) {
                    AttestVerdict::Member
                } else {
                    AttestVerdict::NotAMember
                };
                (verdict, Some(pid))
            }
        };
        Ok(Attestation {
            attestation_version: ATTESTATION_VERSION,
            run_id: self.run_id.to_string(),
            verdict,
            peer_pid,
            mechanism: self.mechanism.to_string(),
            checked_at: events::format_rfc3339_utc(SystemTime::now()),
        })
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

/// The [`events::mechanism_str`] spelling of the POSIX process-group fallback — the
/// one mechanism whose member list is *not* the whole contained tree, which
/// [`peer_is_member`] has to account for. [`crate::doctor`] is its second consumer,
/// for the neighbouring consequence: on this one mechanism a *post-teardown* member
/// snapshot cannot tell a survivor from a just-exited child nobody has reaped yet, so
/// a qualification reports such a count rather than calling it a failed teardown.
///
/// Compared as a string because this module deliberately does not depend on
/// `processkit` directly (see [`SnapshotSource::members`]); that the string is the
/// right one is not left to inspection either — `mechanism_names_stay_in_step` in
/// this module's tests holds it against `events::mechanism_str`'s own output for that
/// variant, so a rename there fails here rather than silently turning this branch
/// off.
pub(crate) const PROCESS_GROUP_MECHANISM: &str = "process_group";

/// Whether `peer_pid` is inside a container that reports `members` under `mechanism`
/// — the single membership predicate `attest` answers with.
///
/// **The identity-safe list is the whole basis.** `members` comes from the run's own
/// live `members_info()` read ([`SnapshotSource::attest`]), never from a registry
/// record, a file, or anything else on disk: the question is whether the kernel
/// currently places this pid in *this* container, and only the container can answer
/// that.
///
/// **Why the mechanism matters.** `members` is the whole contained tree on the
/// mechanisms that enumerate one — a Windows Job Object lists every pid assigned to
/// the job, a Linux cgroup lists every pid in `cgroup.procs`, a FreeBSD process
/// reaper lists the whole descendant subtree — so a plain pid comparison is exact
/// there, and this function does nothing else. The POSIX process-group fallback
/// (macOS and the other non-FreeBSD BSDs, and Linux with no usable cgroup) is
/// different in kind: it *contains* a whole tree but *enumerates* only the tracked
/// group leaders. Comparing pids alone there would answer `not_a_member` for a
/// genuinely contained grandchild — a wrong answer, not a conservative one, and it
/// would make the command mean something different on macOS than on Linux and
/// Windows for the same process tree.
///
/// So on that mechanism, and only there, membership is decided against the peer's
/// **process group**: the leaders `members` reports are process-group leaders, and
/// "is in the process group of a tracked leader" is precisely the predicate that
/// mechanism enforces — it is what `killpg` reaches at teardown. This is not a
/// loosening: a process that left the group (`setsid`) has genuinely escaped this
/// mechanism's containment, and reads `not_a_member`, which is the honest answer.
/// The extra step is deliberately *not* applied to the whole-tree mechanisms, where
/// membership is already exact and sharing a process group with a member would not
/// prove containment.
///
/// `getpgid` is asked about a pid the kernel just named on an open connection, so the
/// process is alive and the number cannot have been recycled; a failure to answer (a
/// target that refuses it across sessions, a process that vanished in the meantime)
/// degrades to the plain comparison rather than to a guess.
fn peer_is_member(peer_pid: u32, mechanism: &str, members: &[Member]) -> bool {
    let listed = |pid: u32| members.iter().any(|member| member.pid == pid);
    listed(peer_pid)
        || (mechanism == PROCESS_GROUP_MECHANISM && process_group_of(peer_pid).is_some_and(listed))
}

/// The process-group id the kernel reports for `pid`, on the platforms that have
/// process groups at all.
///
/// A pure query of kernel state — see [`peer_is_member`] for why the answer is only
/// consulted on the process-group mechanism. `None` for every way there is no answer
/// to trust: a pid that does not fit `pid_t`, a process that has already gone, or a
/// call the platform refused.
#[cfg(unix)]
fn process_group_of(pid: u32) -> Option<u32> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: `getpgid` takes a pid by value and returns a pid — no pointers, no
    // allocation, no shared state; it is a read of kernel state that cannot fail
    // other than by returning `-1`.
    let group = unsafe { libc::getpgid(pid) };
    u32::try_from(group).ok()
}

/// The Windows counterpart: there are no POSIX process groups here, and none are
/// needed — the Job Object mechanism enumerates the whole contained tree, so
/// [`peer_is_member`] never consults this (it is reached only for the
/// [`PROCESS_GROUP_MECHANISM`], which this platform never reports). A separate
/// top-level function rather than a `cfg`-gated arm inside the caller, matching how
/// the rest of the crate splits platform behavior.
#[cfg(not(unix))]
fn process_group_of(_pid: u32) -> Option<u32> {
    None
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
///
/// `peer` is the identity the platform read off this very connection *before* calling
/// this (each `serve` loop's own `peer_identity`), which is why it arrives as a
/// parameter rather than being looked up here: it belongs to the concrete transport
/// type the accept loop still holds, and it must be taken while the connection is
/// unquestionably open (see [`PeerIdentity`]). This function is generic over the
/// stream precisely so both platforms share one protocol implementation, and the
/// identity is the one thing that cannot be generic.
async fn handle_connection<S>(
    stream: S,
    peer: PeerIdentity,
    source: &SnapshotSource<'_>,
    commands: &ControlCommandSink,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(
        CONNECTION_DEADLINE,
        serve_one(stream, peer, source, commands),
    )
    .await;
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
    peer: PeerIdentity,
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
            // Read-only, like `inspect`: it answers about the connection it arrived
            // on and changes nothing. The verdict is decided here, with the client
            // still attached, rather than being derived from anything the request
            // carried — the request carries nothing.
            Some(RequestVerb::Attest) => {
                // An attestation, or — when the container's membership could not be
                // read at all — the same structured-error closing path an
                // unrecognized verb gets, which the client surfaces verbatim rather
                // than turning into a verdict.
                let response = match source.attest(peer) {
                    Ok(attestation) => serialize_attestation(&attestation),
                    Err(reason) => serialize_error(reason),
                };
                write_response(&mut write_half, &response).await?;
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
/// read completes at once. The final size check is deliberately here as well as in
/// the individual serializers: an unexpectedly large acknowledgement or attestation
/// must still fail closed to a bounded structured error rather than violate the
/// reader's framing contract.
async fn write_response<W>(write_half: &mut W, response: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let fallback = if response.len() >= MAX_LINE_BYTES {
        Some(serialize_error(RESPONSE_TOO_LARGE_ERROR))
    } else {
        None
    };
    let response = fallback.as_deref().unwrap_or(response);
    write_half.write_all(response.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
    let _ = write_half.shutdown().await;
    Ok(())
}

/// Serialize a snapshot for the wire, including the response-size check required by
/// [`MAX_LINE_BYTES`]. The terminating newline is written by [`write_response`], so
/// the JSON payload must be strictly shorter than the reader's byte ceiling. An
/// oversized snapshot is refused with the bounded structured error used by other
/// control-plane failures; it is never truncated, fragmented, or sent oversized.
fn serialize_snapshot(snapshot: &Snapshot) -> String {
    let response = serde_json::to_string(snapshot)
        .unwrap_or_else(|_| String::from(r#"{"error":"could not render the snapshot"}"#));
    if response.len() < MAX_LINE_BYTES {
        response
    } else {
        serialize_error(SNAPSHOT_TOO_LARGE_ERROR)
    }
}

/// Serialize an attestation for the wire. A struct of owned strings, a unit enum and
/// an optional number cannot fail to serialize; the fallback is defensive only, and
/// it is deliberately *not* a fabricated verdict — an unserializable answer becomes
/// the same structured error an unknown verb gets, which the client surfaces as a
/// `CONTROL` failure rather than reading as `member` or `not_a_member`.
fn serialize_attestation(attestation: &Attestation) -> String {
    serde_json::to_string(attestation)
        .unwrap_or_else(|_| String::from(r#"{"error":"could not render the attestation"}"#))
}

/// A fixed fallback for a response that was unexpectedly too large after its own
/// serializer ran. This is also short enough to be passed through [`serialize_error`]
/// without triggering its truncation path.
const RESPONSE_TOO_LARGE_ERROR: &str =
    "control-plane response exceeds the 65536-byte response limit";

/// The suffix used when a peer-controlled diagnostic is shortened to preserve the
/// response-line bound. It is part of the diagnostic contract, not a second framing
/// marker: the JSON newline remains the only wire terminator.
const ERROR_TRUNCATION_SUFFIX: &str = "... (truncated)";

/// Serialize an error response for an unrecognized request. Short diagnostics retain
/// their existing text. If JSON escaping would make the complete envelope reach the
/// line ceiling, keep the longest valid UTF-8 prefix whose envelope plus the writer's
/// newline still fits and mark the diagnostic as truncated.
fn serialize_error(message: &str) -> String {
    let response = serialize_error_message(message);
    if response.len() < MAX_LINE_BYTES {
        return response;
    }

    let chars: Vec<char> = message.chars().collect();
    let mut low = 0;
    let mut high = chars.len();
    while low < high {
        let midpoint = low + (high - low).div_ceil(2);
        let candidate = error_message_prefix(&chars, midpoint);
        if serialize_error_message(&candidate).len() < MAX_LINE_BYTES {
            low = midpoint;
        } else {
            high = midpoint - 1;
        }
    }

    serialize_error_message(&error_message_prefix(&chars, low))
}

fn serialize_error_message(message: &str) -> String {
    serde_json::to_string(&ErrorResponse { error: message })
        .unwrap_or_else(|_| String::from(r#"{"error":"control error"}"#))
}

fn error_message_prefix(chars: &[char], prefix_len: usize) -> String {
    let mut message = String::new();
    message.extend(chars.iter().take(prefix_len).copied());
    message.push_str(ERROR_TRUNCATION_SUFFIX);
    message
}

/// Serialize a `cancel`/`kill` acknowledgement for the wire. A struct of owned
/// strings and a bool cannot fail to serialize; the fallback is defensive only.
fn serialize_ack(ack: &ControlAck) -> String {
    serde_json::to_string(ack)
        .unwrap_or_else(|_| String::from(r#"{"accepted":false,"action":"error","run_id":""}"#))
}

/// Build the small current-thread tokio runtime every client entry point (`run`,
/// `inspect`, `attest`, `cancel`, `kill`) drives its async body on, mapping a build
/// failure to the shared [`exit::SETUP`] shape. `enable_all` arms the I/O, time, and
/// signal drivers each caller's body needs (Cargo unifies every caller's feature set into
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

/// The async body of [`inspect`]: registry lookup, the exchange itself
/// ([`inspect_endpoint`]), print.
async fn inspect_async(run_id: &str, json: bool) -> Result<(), RunnerError> {
    let endpoint = resolve_live_endpoint("inspect", run_id).await?;
    let snapshot = inspect_endpoint(&endpoint, run_id).await?;

    for line in snapshot_output_lines(&snapshot, json)? {
        println!("{line}");
    }
    Ok(())
}

/// The single-run `inspect`'s whole exchange with one already-resolved endpoint:
/// connect, converse, and accept the reply only if [`SnapshotReply::accept`] does.
/// Returns the accepted snapshot rather than printing it, exactly as [`mutate_one`]
/// returns its parsed ack, so lookup, exchange, and rendering stay separate steps.
///
/// Split out of [`inspect_async`] so the whole path from the wire to the
/// accept/refuse decision is drivable against a *specific* endpoint: [`inspect_async`]
/// resolves through the process-wide, env-resolved registry, which a unit test cannot
/// point at a scratch directory without racing every other test in the binary. The
/// aggregate path's equivalent step ([`inspect_snapshot_target`]) already takes its
/// registry and target explicitly, so both consumers of a [`SnapshotReply`] can now be
/// covered by the same regression test with a real transport in front of them.
///
/// [`crate::doctor`] is the third consumer, and for the same reason as the tests: it
/// has already resolved its own scratch run's endpoint and wants the round-trip
/// against *that* endpoint, without the rendering [`inspect_async`] adds.
pub(crate) async fn inspect_endpoint(
    endpoint: &str,
    run_id: &str,
) -> Result<Snapshot, RunnerError> {
    // Connect under a deadline: a runner that died between the liveness probe and now
    // fails fast here instead of hanging the client.
    let stream = connect_live(endpoint, "inspect", run_id).await?;

    // Converse under a deadline: a runner that died mid-write, or accepted but never
    // answers, is bounded here — a distinguishable CONTROL result, not a hang.
    let reply: SnapshotReply =
        converse_under_deadline(stream, INSPECT_REQUEST, "inspect", run_id).await?;
    reply.accept(run_id)
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
            Ok(SnapshotDispatch::Dispatched(snapshot)) => outcomes.push(InspectAllOutcome {
                run_id: target.run_id,
                status: InspectAllStatus::Inspected,
                snapshot: Some(snapshot),
                error: None,
            }),
            Ok(SnapshotDispatch::AlreadyGone) => outcomes.push(InspectAllOutcome {
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

/// Drive one aggregate inspect target through [`dispatch_snapshot_target`] — the same
/// ladder `cancel --all` / `kill --all` run per target (see [`mutate_snapshot_target`])
/// — adding only the read-only verb's own final step: [`SnapshotReply::accept`], the
/// same acceptance test the single-run [`inspect_endpoint`] applies.
async fn inspect_snapshot_target(
    registry: &registry::Registry,
    target: &SnapshotTarget,
) -> Result<SnapshotDispatch<Snapshot>, RunnerError> {
    dispatch_snapshot_target(
        registry,
        target,
        "inspect",
        INSPECT_REQUEST,
        |reply: SnapshotReply| reply.accept(&target.run_id),
    )
    .await
}

impl SnapshotReply {
    /// Everything a client must establish about a reply before it may render it: the
    /// runner's declared contract is one this build decodes, and the snapshot
    /// describes the run that was addressed. **The** acceptance test for an `inspect`
    /// reply — both consumers call exactly this one method ([`inspect_endpoint`] for
    /// `inspect --run-id`, the closure in [`inspect_snapshot_target`] for `inspect
    /// --all`) — so neither path can drift into a weaker bar than the other, the way
    /// both already shared the identity half alone. Consuming `self` and returning the
    /// [`Snapshot`] is what makes that structural: an unaccepted reply has no path to
    /// the renderer, because the renderer's input only exists on the other side of
    /// this call.
    ///
    /// Version first — and, by [`SnapshotReply`]'s own decoding, already decided
    /// before the shape was parsed at all: a reply this build cannot interpret is a
    /// more fundamental failure than one it can read but which names the wrong run,
    /// and on an uninterpretable payload "the `run_id` field disagrees" would itself
    /// be a conclusion drawn under semantics the peer never promised.
    fn accept(self, expected_run_id: &str) -> Result<Snapshot, RunnerError> {
        match self {
            Self::Unreadable(declared) => Err(refuse_snapshot_version(declared, expected_run_id)),
            Self::Readable(snapshot) => {
                verify_snapshot_identity(&snapshot, expected_run_id)?;
                Ok(snapshot)
            }
        }
    }
}

/// Refuse a reply whose declared `snapshot_version` falls outside the range this build
/// decodes (T-292) — see [`snapshot_version_is_readable`] for the range itself and the
/// module doc for why it is one-sided.
///
/// The refusal reuses the established [`unreachable_run`] wording and the reserved
/// [`exit::CONTROL`] (103) code, exactly as [`verify_snapshot_identity`] does for a
/// snapshot naming the wrong run: from the operator's side, "this runner cannot give
/// me a snapshot I can trust" is the same class of outcome, and it must not be
/// reported as success. The message names the number that arrived, the range this
/// build reads, and which way the runner falls outside it, because the fix is to pick
/// a build that speaks the runner's version rather than to retry.
///
/// Nothing peer-supplied is spliced into the message except that declared version, an
/// integer — so, unlike a peer's free-text error ([`normalize_peer_error_text`]), this
/// diagnostic needs no sanitizing to stay one honest line.
fn refuse_snapshot_version(declared: u64, expected_run_id: &str) -> RunnerError {
    let side = if declared > u64::from(SNAPSHOT_VERSION) {
        "the runner is a newer build than this client, so what its version changed is unknown here"
    } else {
        "the runner is older than any build this client still decodes"
    };
    unreachable_run(
        "inspect",
        expected_run_id,
        format!(
            "the runner answered with control-plane snapshot version {declared}, and this client \
             reads versions {MIN_READABLE_SNAPSHOT_VERSION} to {SNAPSHOT_VERSION} ({side}); the \
             reply was refused rather than rendered under semantics its sender never promised — \
             inspect this run with a processkit-cli build that implements its snapshot version \
             (for a newer runner, one at least as new as the binary that started the run)"
        ),
    )
    // A `CONTROL` failure that says nothing about the run's liveness: the target is
    // registered, live, reachable, and it answered. Two of the eight kinds that exist
    // to split 103 have that shape — this one and `peer_identity_unsupported`, which
    // `attest_outcome` mints when the runner answers but cannot name its peer — and
    // two sites mint *this* one: `AttestationReply::accept` refuses an
    // `attestation_version` this build does not read exactly as this refuses a
    // `snapshot_version`. The `kind` is what keeps that reading apart from every
    // "could not reach it" one for a machine consumer, exactly as this message does
    // for a human.
    .with_kind(ErrorKind::IncompatibleContract)
}

/// Refuse a snapshot describing a run other than the one that was addressed — the
/// identity half of [`SnapshotReply::accept`].
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

/// The runner's answer to `attest`: whether the process that opened **this
/// connection** is inside this run's ProcessKit container, decided by the runner
/// itself while that connection is open.
///
/// This is what turns a convention into a checkable fact. An adapter that gates work
/// on "the caller belongs to run X" can otherwise only inspect an environment string
/// the caller carries, which proves nothing about containment — any process can hold
/// any string. Here the identity is the kernel's ([`PeerIdentity`]) and the
/// membership list is the container's own ([`SnapshotSource::members`], the same
/// `members_info()` read `inspect` and the JSONL `members_snapshot` use), so a
/// positive answer is a statement the runner is in a position to make.
///
/// **What it is not.** It is not authentication between mutually hostile parties.
/// Everything here lives inside this project's existing same-OS-user trust boundary
/// (`docs/threat-model.md`): the transport is owner-only, and a process running as
/// that same user is already inside the boundary. What this closes is the *forgeable
/// correlation* — a non-member claiming membership through a string it copied — not
/// an attack by a principal who could already reach the run's control plane by
/// definition.
#[derive(Debug, Serialize, Deserialize)]
pub struct Attestation {
    /// Attestation format version — [`ATTESTATION_VERSION`] when this build is the
    /// runner. Like [`Snapshot::snapshot_version`], its value genuinely originates on
    /// the far side of the wire, and the client acts on it before reading anything
    /// else (see [`AttestationReply`]).
    pub attestation_version: u32,
    /// The run that answered — the id the client matched in the registry, echoed so a
    /// reply describing some other run is refused rather than believed.
    pub run_id: String,
    /// The verdict itself. See [`AttestVerdict`] for why the three outcomes are kept
    /// apart rather than collapsed into a boolean.
    pub verdict: AttestVerdict,
    /// The pid the kernel gave for the connecting client, or `null` when the platform
    /// could not name it (the [`AttestVerdict::PeerIdentityUnsupported`] case).
    ///
    /// Reported for correlation, never as an input: the client did not send it and
    /// cannot influence it. On a system where the runner and the client see different
    /// pid namespaces this is the runner's view, which is the one the verdict was
    /// decided in.
    pub peer_pid: Option<u32>,
    /// The containment mechanism the verdict is *about* — `job_object` | `cgroup_v2` |
    /// `process_group` (the same vocabulary as the JSONL `run_started` and the
    /// `inspect` snapshot, [`events::mechanism_str`]).
    ///
    /// Present because it is what fixes the **scope** of the fact, and that scope
    /// genuinely differs by mechanism: a Job Object or a cgroup enumerates the whole
    /// tree, while the POSIX process-group fallback contains a tree but enumerates
    /// only the group leaders — which is exactly why membership is decided against
    /// the process group there rather than against the leader list alone (see
    /// [`peer_is_member`]). A consumer that needs to know how strong the containment
    /// behind a `member` answer is reads this rather than guessing from the platform.
    pub mechanism: String,
    /// When the runner decided this, RFC 3339 UTC with millisecond precision (the
    /// same formatter as the JSONL events and the registry record).
    ///
    /// An attestation is a *point-in-time* fact, not a token: it says the peer was a
    /// member when asked, and it says nothing about any later moment. Carrying the
    /// instant makes that explicit instead of implied.
    pub checked_at: String,
}

/// The three outcomes of an attestation — deliberately three, not a boolean.
///
/// "Not a member" and "this platform cannot tell you" are different facts with
/// different consequences for a caller: the first is a decided, stable verdict a
/// retry cannot change; the second is a missing capability, and treating it as a
/// negative would understate it just as treating it as a positive would be the
/// unproven "ok" the whole design exists to prevent. Each maps onto its own exit code
/// and [`ErrorKind`] (see [`attest_outcome`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestVerdict {
    /// The connecting process is inside this run's container.
    Member,
    /// The connecting process was named by the kernel and is **not** inside this
    /// run's container. A positive negative: the runner knows who asked and knows the
    /// answer is no.
    NotAMember,
    /// The runner could not obtain a kernel-authenticated identity for the connecting
    /// process at all, so it refuses to answer either way (see [`PeerIdentity`] and
    /// [`PEER_IDENTITY_SUPPORTED`]).
    PeerIdentityUnsupported,
}

/// An `attest` reply as it comes off the wire, decoded in the order its contract is
/// decided in: the declared `attestation_version` first, the payload's shape second —
/// the same construction (and for the same reason) as [`SnapshotReply`], so a runner
/// whose shape this build cannot even deserialize still gets the *version*
/// diagnostic, which is the actionable one.
///
/// `#[doc(hidden)] pub` so the `control_wire` fuzz target can drive the exact type
/// this verb's client parses a reply into, alongside the ones it already drives.
#[derive(Debug)]
#[doc(hidden)]
pub enum AttestationReply {
    /// A reply declaring exactly [`ATTESTATION_VERSION`], parsed into the shape this
    /// build implements.
    Readable(Attestation),
    /// A reply declaring any other version, carrying only the number the runner
    /// declared — the payload was deliberately *not* interpreted.
    Unreadable(u64),
}

impl<'de> Deserialize<'de> for AttestationReply {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(declared) = value
            .get("attestation_version")
            .and_then(serde_json::Value::as_u64)
            && declared != u64::from(ATTESTATION_VERSION)
        {
            return Ok(Self::Unreadable(declared));
        }
        serde_json::from_value(value)
            .map(Self::Readable)
            .map_err(serde::de::Error::custom)
    }
}

impl AttestationReply {
    /// Everything a client must establish before it may act on an attestation: the
    /// runner's declared contract is the one this build implements, and the answer
    /// describes the run that was addressed. **The** acceptance test for an `attest`
    /// reply, consuming `self` so an unaccepted reply has no path to the renderer or
    /// to the exit-code decision — the same structural guarantee
    /// [`SnapshotReply::accept`] gives the read-only snapshot.
    fn accept(self, expected_run_id: &str) -> Result<Attestation, RunnerError> {
        match self {
            Self::Unreadable(declared) => Err(unreachable_run(
                ATTEST_REQUEST,
                expected_run_id,
                format!(
                    "the runner answered with control-plane attestation version {declared}, and \
                     this client reads version {ATTESTATION_VERSION}; the reply was refused rather \
                     than read as a membership verdict its sender never promised — attest this \
                     run with a processkit-cli build that implements its attestation version"
                ),
            )
            .with_kind(ErrorKind::IncompatibleContract)),
            Self::Readable(attestation) => {
                if attestation.run_id != expected_run_id {
                    return Err(unreachable_run(
                        ATTEST_REQUEST,
                        expected_run_id,
                        "the runner returned an attestation for a different run".to_string(),
                    ));
                }
                Ok(attestation)
            }
        }
    }
}

/// Client entry for `attest --run-id <id> [--json]`: ask the live runner named
/// `run_id` whether **this very process** is inside its container, print the answer,
/// and turn it into an exit code (see [`attest_outcome`]).
///
/// There is deliberately no way to ask about a different process: the identity comes
/// from the connection this client itself opens, so the only question the command can
/// pose is "am I in it?". Runs on the same small current-thread runtime every other
/// control client uses.
pub fn attest(run_id: &str, json: bool) -> Result<(), RunnerError> {
    let runtime = current_thread_runtime()?;
    runtime.block_on(attest_async(run_id, json))
}

/// The async body of [`attest`]: registry lookup, the exchange itself, print, then
/// the verdict's own outcome.
///
/// The report is printed **before** the verdict is turned into a failure, matching
/// every other command here that reports and then fails (`probe --json` with an unmet
/// `--require-*`, `inspect --all --json` with an unreachable target): a caller reading
/// stdout gets the machine-readable answer in every case where the runner produced
/// one, and the exit code says what to do about it.
async fn attest_async(run_id: &str, json: bool) -> Result<(), RunnerError> {
    let endpoint = resolve_live_endpoint(ATTEST_REQUEST, run_id).await?;
    let attestation = attest_endpoint(&endpoint, run_id).await?;

    for line in attestation_output_lines(&attestation, json)? {
        println!("{line}");
    }
    attest_outcome(&attestation, run_id)
}

/// The `attest` exchange with one already-resolved endpoint: connect, converse, and
/// accept the reply only if [`AttestationReply::accept`] does. Split out of
/// [`attest_async`] for the same reason [`inspect_endpoint`] is: the whole path from
/// the wire to the accept/refuse decision stays drivable against a *specific*
/// endpoint in a test, without the process-wide env-resolved registry.
async fn attest_endpoint(endpoint: &str, run_id: &str) -> Result<Attestation, RunnerError> {
    let stream = connect_live(endpoint, ATTEST_REQUEST, run_id).await?;
    let reply: AttestationReply =
        converse_under_deadline(stream, ATTEST_REQUEST, ATTEST_REQUEST, run_id).await?;
    reply.accept(run_id)
}

/// Turn an accepted attestation into this invocation's outcome — the one place the
/// three verdicts become exit codes, so the CLI and the wire cannot drift.
///
/// - [`AttestVerdict::Member`] is the only success.
/// - [`AttestVerdict::NotAMember`] takes the reserved [`exit::NOT_A_MEMBER`] (115) it
///   was minted for: a *decided* negative, which no existing code could carry without
///   claiming something else happened (see that code's own documentation).
/// - [`AttestVerdict::PeerIdentityUnsupported`] fails closed on the established
///   [`exit::CONTROL`] (103) — the runner was reached and answered, but no answer this
///   client may act on came back, the same shape a refused `snapshot_version` takes —
///   and is told apart from every other 103 by its [`ErrorKind`].
fn attest_outcome(attestation: &Attestation, run_id: &str) -> Result<(), RunnerError> {
    match attestation.verdict {
        AttestVerdict::Member => Ok(()),
        AttestVerdict::NotAMember => Err(RunnerError::new(
            exit::NOT_A_MEMBER,
            format!(
                "this process is not a member of run `{run_id}`: the runner named the connecting \
                 process (pid {}) from the control transport and it is not in that run's {} \
                 container",
                attestation
                    .peer_pid
                    .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
                // The one peer-supplied string spliced into this diagnostic, so it
                // crosses the terminal barrier bounded — the same treatment the
                // human renderer gives it, and the same reason a peer's own error
                // text is normalized before being surfaced.
                crate::text::terminal_safe_bounded(&attestation.mechanism)
            ),
        )
        .with_kind(ErrorKind::NotAMember)),
        AttestVerdict::PeerIdentityUnsupported => Err(unreachable_run(
            ATTEST_REQUEST,
            run_id,
            "the runner could not obtain a kernel-authenticated identity for this client from \
             the control transport, so it refused to answer either way rather than report an \
             unproven membership; check `probe --json --require-surface attest:peer-identity` \
             against the runner's own binary before relying on attestation on this platform"
                .to_string(),
        )
        .with_kind(ErrorKind::PeerIdentityUnsupported)),
    }
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
/// [`ControlAck`] rather than printing it so lookup and rendering stay separate —
/// which is also what lets [`crate::doctor`] drive the same mutating round-trip
/// against its own scratch run without printing an ack nobody asked for.
pub(crate) async fn mutate_one(
    run_id: &str,
    command: ControlCommand,
) -> Result<ControlAck, RunnerError> {
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

/// What driving one aggregate snapshot target through [`dispatch_snapshot_target`]
/// produced: the runner's own reply to the verb (`T` — a [`Snapshot`] for `inspect
/// --all`, a [`ControlAck`] for `cancel --all` / `kill --all`), or the record-specific
/// verdict that the target finished before the exchange completed.
///
/// One type for both call sites rather than a per-verb pair: `already_gone` is the
/// same reclassification decision in both, and the aggregate reports differ only in
/// how they render it (see [`InspectAllStatus::AlreadyGone`] and
/// [`ControlAllStatus::AlreadyGone`]).
#[derive(Debug)]
enum SnapshotDispatch<T> {
    /// The runner answered and the caller's final verification accepted the reply.
    Dispatched(T),
    /// The snapshotted record is confirmed absent or stale — the desired terminal
    /// state for mass teardown, and honest "nothing left to inspect" for the
    /// read-only verb.
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
            Ok(SnapshotDispatch::Dispatched(ack)) => outcomes.push(ControlAllOutcome {
                run_id: ack.run_id,
                accepted: ack.accepted,
                status: ControlAllStatus::Accepted,
                error: None,
            }),
            Ok(SnapshotDispatch::AlreadyGone) => outcomes.push(ControlAllOutcome {
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

/// Dispatch one aggregate snapshot target without resolving its non-unique `run_id`
/// again — the single ladder every `--all` verb runs per target, generic over the
/// reply type exactly as [`converse_under_deadline`] already is.
///
/// The sequence is the whole contract, and it is deliberately one implementation
/// rather than one per verb: the reclassification policy below is load-bearing for the
/// aggregate exit-code contract, so a second copy could silently drift into a
/// different policy for one command (the failure mode the hard-teardown tail had
/// before [`crate::run::teardown`]'s `emit_hard_teardown` unified it).
///
/// 1. Re-confirm the snapshotted record ([`snapshot_target_state`]) *before* touching
///    the transport: a target that finished since the snapshot is `already_gone`, not
///    a failure.
/// 2. Refuse a live target that advertises no endpoint — it cannot be reached, and
///    "still live" is not "already gone".
/// 3. [`connect_live`], then [`converse_under_deadline`] with `verb`, each with a
///    record-specific re-probe on error: a failure is reclassified as `already_gone`
///    **only** when the re-probe confirms the record is now absent or stale. An
///    unprobeable or identity-changed record stays a hard failure — unknown liveness
///    is not evidence the target ended (see [`snapshot_target_state`]).
/// 4. Hand the parsed reply to the caller's `accept` step (version and identity for an
///    `inspect` reply, ack matching for a mutation), which alone decides whether the
///    reply may be used at all, and yields what the verb actually wanted from it.
///
/// The reply type off the wire (`T`) and the accepted value (`U`) are separate
/// parameters because for `inspect` they genuinely differ: what `converse` parses is a
/// [`SnapshotReply`], which still carries an undecided version verdict, and only
/// [`SnapshotReply::accept`] turns it into a [`Snapshot`] a caller may render. Letting
/// the accept step *produce* that value, rather than merely inspect it, is what keeps
/// the refusal impossible to bypass here without also bypassing this ladder.
///
/// `action` names the verb in every operator-facing message, while `verb` is the text
/// actually written to the wire; they read the same today for both call sites, but the
/// module keeps the diagnostic label and the protocol token separate everywhere else
/// ([`converse_under_deadline`] takes both too), so this driver does not fuse them.
async fn dispatch_snapshot_target<T, U, A>(
    registry: &registry::Registry,
    target: &SnapshotTarget,
    action: &str,
    verb: &str,
    accept: A,
) -> Result<SnapshotDispatch<U>, RunnerError>
where
    T: serde::de::DeserializeOwned,
    A: FnOnce(T) -> Result<U, RunnerError>,
{
    if snapshot_target_state(registry, target, action)? == SnapshotTargetState::AlreadyGone {
        return Ok(SnapshotDispatch::AlreadyGone);
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
        Err(err) => return reclassify_target_failure(registry, target, action, err),
    };

    if snapshot_target_state(registry, target, action)? == SnapshotTargetState::AlreadyGone {
        return Ok(SnapshotDispatch::AlreadyGone);
    }

    let reply: T = match converse_under_deadline(stream, verb, action, &target.run_id).await {
        Ok(reply) => reply,
        Err(err) => return reclassify_target_failure(registry, target, action, err),
    };

    Ok(SnapshotDispatch::Dispatched(accept(reply)?))
}

/// Decide what a transport-level failure against one snapshot target *means*: it is
/// `already_gone` only when the record-specific re-probe confirms the target is now
/// absent or stale, and otherwise the original failure stands. A re-probe that cannot
/// answer at all replaces `err` with its own [`exit::CONTROL`] refusal — unknown
/// liveness must not be laundered into either verdict.
fn reclassify_target_failure<T>(
    registry: &registry::Registry,
    target: &SnapshotTarget,
    action: &str,
    err: RunnerError,
) -> Result<SnapshotDispatch<T>, RunnerError> {
    match snapshot_target_state(registry, target, action)? {
        SnapshotTargetState::AlreadyGone => Ok(SnapshotDispatch::AlreadyGone),
        SnapshotTargetState::Live => Err(err),
    }
}

/// Drive one aggregate mutation target through [`dispatch_snapshot_target`], adding
/// only the mutating verbs' own final step: a well-behaved runner acks the exact
/// action and run it was asked about, and anything else is a failure rather than a
/// false success (the same [`ack_matches`] contract the by-id form applies).
async fn mutate_snapshot_target(
    registry: &registry::Registry,
    target: &SnapshotTarget,
    command: ControlCommand,
) -> Result<SnapshotDispatch<ControlAck>, RunnerError> {
    let action = command.verb();
    dispatch_snapshot_target(
        registry,
        target,
        action,
        command.verb(),
        |ack: ControlAck| {
            if ack_matches(&ack, action, &target.run_id) {
                return Ok(ack);
            }
            Err(unreachable_run(
                action,
                &target.run_id,
                "the runner did not acknowledge the command for the snapshotted target".to_string(),
            ))
        },
    )
    .await
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
/// client (`inspect`/`cancel`/`kill`/`attest`) and by [`crate::doctor`], which drives
/// the same resolution against its own scratch run rather than a second one written
/// to pass; `action` names the verb in the message. Opens the env/platform-resolved registry and delegates the scan to
/// [`resolve_in_registry`], which the mutating verbs' pre-dispatch re-check
/// ([`reconfirm_target`]) also drives, against the same open [`registry::Registry`].
pub(crate) async fn resolve_live_endpoint(
    action: &str,
    run_id: &str,
) -> Result<String, RunnerError> {
    let registry = open_registry(action, run_id)?;
    resolve_in_registry(&registry, action, run_id)
}

/// Open the env/platform-resolved registry, mapping a failure to the same
/// distinguishable [`exit::CONTROL`] shape every other unreachable-run result uses.
fn open_registry(action: &str, run_id: &str) -> Result<registry::Registry, RunnerError> {
    registry::Registry::open_read_only().map_err(|err| {
        // The code stays `CONTROL` — from the caller's side the target could not be
        // resolved — while the machine-readable kind names the actual cause, which
        // is the registry itself rather than this run (`docs/registry.md` is where
        // an operator goes next, not the run's own logs).
        unreachable_run(
            action,
            run_id,
            format!("could not open the run registry: {err}"),
        )
        .with_kind(ErrorKind::Registry)
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
        .with_kind(ErrorKind::Registry)
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
        )
        .with_kind(ErrorKind::NotFound));
    }

    // Count *live* entries first — regardless of whether they advertise an
    // endpoint — before ever looking at endpoints. `register` (`src/registry/mod.rs`)
    // never enforces `run_id` uniqueness, so two concurrent runs started with the
    // same explicit `--run-id` can both be live at once, and one of them may not
    // (yet, or ever) have published an endpoint (disconnected/failed transport).
    // Counting only endpoint-having entries would let such a duplicate evade
    // detection and have the sole endpoint-having entry acted on as if it were
    // unambiguous. Every verb (`inspect`/`cancel`/`kill`/`attest`) shares this
    // resolver and treats *any* live duplicate as a hard, documented failure rather
    // than silently acting on whichever entry the directory scan happens to return
    // first: for the mutating verbs, guessing wrong means cancelling or killing
    // the *other* run instead of the intended one; the read-only `inspect` and
    // `attest` get the same treatment rather than a softer fallback because a
    // snapshot — or a membership verdict — that names the wrong run is just as
    // misleading as acting on it (see `docs/registry.md`, "Run id resolution —
    // ambiguity is a hard failure").
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
///
/// [`crate::doctor`] calls it for the opposite outcome from every other caller: after
/// its scratch run has ended it expects the connect to **fail**, which is how it
/// establishes that nothing still answers on the endpoint that run published.
pub(crate) async fn connect_live(
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
            .with_kind(ErrorKind::IpcDeadline)
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
        .map_err(|_| {
            // A bounded window elapsed against a runner that was there — the one
            // control-plane failure where nothing established the target is
            // unreachable, and so the one the envelope reports as retryable.
            unreachable_run(action, run_id, "the runner did not answer in time".into())
                .with_kind(ErrorKind::IpcDeadline)
        })?
        .map_err(|err| unreachable_run(action, run_id, err.to_string()))
}

/// A "cannot reach the target run" error carrying the reserved [`exit::CONTROL`] code
/// and a message naming the `action` (`inspect`/`cancel`/`kill`/`attest`), the run,
/// and the reason.
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
/// [`registry::Health::Live`]. It is load-bearing for a *machine* too, and no longer
/// only in prose: the two branches carry different
/// [`ErrorKind`]s (`stale` versus `unprobed`, the latter the one this file calls
/// retryable), so an adapter reading `--error-format json` gets the same distinction
/// this message spells out. What it must not do is *assert* the runner exited when
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
        )
        .with_kind(ErrorKind::Unprobed);
    }
    unreachable_run(
        action,
        run_id,
        "its registry entry is stale — the runner is gone (it exited without cleaning up)"
            .to_string(),
    )
    .with_kind(ErrorKind::Stale)
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
    .with_kind(ErrorKind::AmbiguousRunId)
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
