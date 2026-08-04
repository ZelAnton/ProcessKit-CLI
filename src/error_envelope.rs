//! The machine-readable error envelope printed under `--error-format json` — the
//! *machine-error* rendering of [`crate::exit`]'s codes.
//!
//! # What this is for
//!
//! Without this flag, a failed invocation gives a machine consumer exactly two
//! things: a reserved-band exit code ([`crate::exit`]) and a line of free-text prose
//! on stderr. The code is a coarse verdict — eight genuinely different situations all
//! exit [`exit::CONTROL`] (103) — and the prose is not a contract, so an adapter
//! that must tell a stale registry entry from an unprobeable one, or an ambiguous
//! run id from a runner that died mid-conversation, has to parse English or
//! reimplement this binary's internals.
//!
//! `--error-format json` replaces that prose line with **exactly one** JSON object
//! on stderr:
//!
//! ```text
//! {"error_version":1,"code":103,"kind":"stale","operation":"inspect",
//!  "run_id":"build-42","retryable":false,"message":"cannot inspect run …"}
//! ```
//!
//! The object is *bounded*: seven fields, all scalars, no nesting, no arrays, one
//! line. [`ErrorEnvelope::code`] and [`ErrorEnvelope::kind`] carry the decision,
//! [`ErrorEnvelope::message`] stays explanatory and is explicitly **not** part of
//! the stable contract (it is free to be reworded in any release — the published
//! golden fixture deliberately does not pin its text).
//!
//! # Two axes, not one
//!
//! `code` and `kind` are deliberately different axes over the same failure. `code`
//! is the process's exit status, coarse by necessity (a shell has one byte to read);
//! `kind` is the finer name of what actually happened, and it is *never coarser*
//! than the code:
//!
//! - it splits [`exit::CONTROL`] (103) **eight** ways — [`ErrorKind::NotFound`],
//!   [`ErrorKind::Stale`], [`ErrorKind::Unprobed`], [`ErrorKind::AmbiguousRunId`],
//!   [`ErrorKind::ControlUnreachable`], [`ErrorKind::IpcDeadline`],
//!   [`ErrorKind::IncompatibleContract`], and
//!   [`ErrorKind::PeerIdentityUnsupported`] — which are exactly the distinctions
//!   `docs/integration.md` §6 ("Typical errors") already draws in prose, and
//!   [`crate::control`]'s `no_live_entry` / `ambiguous_run` /
//!   `refuse_snapshot_version` / `AttestationReply::accept` / `attest_outcome`
//!   already make in code — `refuse_snapshot_version` and
//!   `AttestationReply::accept` being the only two sites that mint
//!   [`ErrorKind::IncompatibleContract`], one per version axis
//!   (`snapshot_version` and `attestation_version`), and `attest_outcome` the only
//!   source of [`ErrorKind::PeerIdentityUnsupported`] (that count is the kinds that
//!   exist *to split* 103, which
//!   is what `fixtures/schema/cli/error.schema.json` conditions on; the one further
//!   kind that can arrive under 103 is [`ErrorKind::Registry`], below);
//! - it splits [`exit::SETUP`] (111) into [`ErrorKind::Registry`] (the per-user run
//!   registry itself could not be opened or scanned) and [`ErrorKind::Setup`]
//!   (every other prerequisite: an unwritable output, a runtime that would not
//!   build, a reply that would not serialize) — [`ErrorKind::Registry`] being the one
//!   kind reachable under two codes, because an unreadable registry is *also* why a
//!   by-`run-id` client reports [`exit::CONTROL`] (103); and
//! - it never merges two codes into one kind, so a consumer that branches on `kind`
//!   alone loses nothing the exit code would have told it.
//!
//! Nothing here mints a new *exit code*: the taxonomy is a finer axis laid over the
//! codes `docs/exit-codes.md` already publishes, not a competing set of them.
//!
//! # The `run` family reuses the event stream's vocabulary
//!
//! A `run` that fails already has a machine-readable account of *why* — the
//! terminal `runner_exit` event's `source` field ([`crate::events`],
//! `fixtures/schema/v1/schema.json`). This envelope therefore does not invent a
//! second vocabulary for those endings: the kinds a failing `run` reports
//! ([`ErrorKind::SpawnError`], [`ErrorKind::ContainerError`], [`ErrorKind::Timeout`],
//! [`ErrorKind::Cancelled`], [`ErrorKind::ControlCancel`], [`ErrorKind::ControlKill`],
//! [`ErrorKind::OutputOverflow`], [`ErrorKind::Setup`], [`ErrorKind::Internal`]) are
//! **the `runner_exit.source` values themselves**, spelled identically, and that
//! schema stays their single source of truth. The envelope is the account available
//! when there is no stream to read — a run started without `--jsonl`, or a
//! `run --detach` that never got far enough to produce one.
//!
//! # Scope: post-parse failures only
//!
//! Everything the binary reports as a [`RunnerError`] — every subcommand, `run`
//! included — is covered, because there is exactly one rule: *wherever this binary
//! would print `processkit-cli: <message>` to stderr and exit with a reserved-band
//! code, `--error-format json` prints the envelope instead.* Two things are
//! deliberately outside it, and both are stated in `docs/exit-codes.md` and
//! `docs/integration.md` rather than left as silent gaps:
//!
//! - **clap's parse-time usage errors** (`USAGE`, 100 — an unknown flag, a bad
//!   duration, a missing subcommand) stay human-readable in v1. They are produced
//!   by the argument parser before this binary has decided what it was asked to do,
//!   so there is no `operation` to name and no run to point at; rendering clap's
//!   own usage/suggestion text as an envelope would distort it into something
//!   neither a human nor a machine reads well. A consumer still gets the reserved
//!   `100` code, and `probe` (`docs/integration.md` §1) is the supported way to
//!   check a flag exists *before* using it.
//! - **`processkit-cli: warning: …` lines** are not failures and keep their prose.
//!   The envelope is printed once, on the way out, for the failure that ends the
//!   process.
//!
//! # Where it is printed
//!
//! Always stderr, never stdout: stdout stays reserved for a command's successful
//! output, so a command that prints a JSON report *and then* fails (`probe --json`
//! with an unmet `--require-*`, `inspect --all --json` with an unreachable target)
//! leaves stdout byte-for-byte unchanged and adds the envelope to stderr. For
//! `run`, note that the child's own echoed stderr shares the stream: the envelope is
//! the runner's own final line there rather than the only line (use `--capture-dir`
//! or `--no-echo` for a clean channel).

use crate::cli::ErrorFormat;
use crate::exit::{self, RunnerError};

/// Version of the envelope's field set and of the [`ErrorKind`] vocabulary's
/// meanings — the pin a consumer reads before trusting the object's shape.
///
/// **Why this contract carries a version when four of the eight *other* families
/// under `fixtures/schema/cli/` do not.** Those are synchronous stdout
/// renderings consumed by whoever just invoked this exact binary, so the caller
/// already knows which version produced them and can pin it with `probe`
/// (`fixtures/schema/cli/README.md`, "Versioning"). A failure envelope is routinely
/// read by a party that did *not* invoke the binary: a captured CI log read back
/// later by a different tool, a wrapper that ran whatever binary was on `PATH`, an
/// incident triage that has the stderr but not the invocation. That is the same
/// "durable artifact / read by a party that did not invoke this binary" test the
/// JSONL `schema_version`, the registry `registry_version`, the control-plane
/// `snapshot_version`, the attestation's `attestation_version`, the qualification
/// report's `doctor_version`, and the probe
/// report's `probe_version` all meet.
///
/// Carrying a version does **not** make this a fourth compatibility surface. The
/// crate's supported surface is still the three `AGENTS.md` fixes — CLI flags, the
/// reserved exit-code band, the JSONL `schema_version` (see [`crate`]) — and this
/// envelope rides on the first two: the global flag that turns it on, and the code
/// it reports. `error_version` pins its *shape* inside the payload, exactly as
/// `probe --json`'s `probe_version` and `inspect`'s `snapshot_version` pin theirs,
/// which is why an adapter has three things to establish before launching and not
/// four (`docs/compatibility.md`, "Machine-output schemas").
///
/// Bumped only by a **breaking** change to the object (a stable field removed or
/// re-typed, or an existing `kind` given a different meaning). Adding a field, or
/// adding a new `kind` value, is additive and does not bump it — a consumer that
/// reads the fields and kinds it knows is unaffected, which is why
/// `error.schema.json` pins this value with `const` (this binary declares exactly
/// one version; it never *reads* an envelope, so it has no tolerance range to
/// express — contrast `inspect`'s `snapshot_version`, which does).
pub const ERROR_VERSION: u32 = 1;

/// What actually failed — the envelope's finer-grained axis over the exit code.
///
/// Every value below names a distinction this project already documents, in
/// `docs/exit-codes.md`, `docs/integration.md` §6, or (for the `run` family)
/// `fixtures/schema/v1/schema.json`'s `runnerExit.source`. Nothing here is a new
/// classification invented for this envelope.
///
/// The vocabulary grows **additively**: a new value is a minor change, and a
/// consumer that does not recognize one should fall back to the numeric `code`,
/// which is always present and always inside the reserved band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// An invalid command line detected *after* parsing — in practice only a
    /// detached start relaying the [`exit::USAGE`] (100) its respawned copy
    /// reported. clap's own parse-time usage errors are outside the envelope's
    /// scope (see this module's "Scope").
    Usage,
    /// The target program could not be started: not found, not executable, a bad
    /// `--cwd`, permission denied ([`exit::SPAWN`], 101). Spelled as the
    /// `runner_exit.source` value for the same ending.
    SpawnError,
    /// ProcessKit backend/containment failure — the kernel container, job object,
    /// IPC endpoint, or run registry could not be established, including a resource
    /// limit the active mechanism could not apply ([`exit::BACKEND`], 102). Spelled
    /// as the `runner_exit.source` value for the same ending.
    ContainerError,
    /// Nothing in the registry names the run the command was asked about
    /// ([`exit::CONTROL`], 103): no record carries that `run_id` at all, or — for
    /// `events` — a record does but publishes no JSONL locator to read, because the
    /// run was started without `--jsonl`. A clean exit deletes its own record, so
    /// this is also what a finished-and-reaped run looks like.
    NotFound,
    /// The run's registry entry is **confirmed** stale: the liveness probe ran and
    /// found no holder, so the runner is gone (it exited without cleaning up)
    /// ([`exit::CONTROL`], 103). `list` shows the entry as `stale`; `prune` removes
    /// it. See `docs/integration.md` §6, "Stale registry entry".
    Stale,
    /// The run's registry entry could not be probed **at all** — its lock file would
    /// not open, or the lock call itself failed — so nothing about the run is
    /// established either way ([`exit::CONTROL`], 103). Deliberately not [`Self::Stale`]:
    /// claiming the runner is gone would be an unconfirmed positive claim. `list`
    /// shows the entry as `unprobed` and `prune` leaves it alone. See
    /// `docs/integration.md` §6, "Unprobeable registry entry".
    Unprobed,
    /// More than one **live** registry entry carries the requested `run_id`, so the
    /// command refuses to guess which one was meant ([`exit::CONTROL`], 103) — the
    /// same fail-closed verdict for the control-plane verbs, the registry-only
    /// `wait`, and `events` (where it means the matching records name several
    /// different streams). See `docs/registry.md`, "Run id resolution — ambiguity is
    /// a hard failure".
    AmbiguousRunId,
    /// A single live target was resolved but could not be reached or did not answer
    /// ([`exit::CONTROL`], 103): it exposes no control endpoint, its endpoint is
    /// invalid, the connection failed, or the runner died mid-conversation. Also the
    /// verdict of the `--all` fan-outs, where one or more snapshot targets could not
    /// be reached or did not acknowledge — there, unlike the by-`run-id` case, some
    /// targets may have been acted on successfully and the per-target detail is in
    /// the JSON report on stdout.
    ControlUnreachable,
    /// A control-plane deadline elapsed with the runner still unresponsive
    /// ([`exit::CONTROL`], 103): the connect window, or the request/response
    /// exchange window. Distinguished from [`Self::ControlUnreachable`] because it is
    /// the one control-plane failure where nothing established that the target is
    /// unreachable — it was simply too slow within a bounded window — which is why
    /// it is the one that is [`Self::retryable`].
    IpcDeadline,
    /// A contract this build does not implement was declared by the other side and
    /// the answer was refused rather than misread ([`exit::CONTROL`], 103). Either
    /// read-only verb can produce it, each on its own version axis: an `inspect`
    /// reply carrying a control-plane `snapshot_version` outside the range this
    /// client reads — a range, because an older snapshot shape is still decoded —
    /// or an `attest` reply carrying an `attestation_version` other than the single
    /// one this client reads, read strictly with no such range because a misread
    /// membership verdict is a security answer rather than a diagnostic. It says
    /// **nothing** about the run's liveness — the target is registered, live, and
    /// reachable, and `cancel`/`kill`/`wait`/`list` are unaffected. Do not retry; use
    /// a build that implements the runner's version of *that* contract. See
    /// `docs/control-plane.md`, "Snapshot version: a newer runner's reply is
    /// refused, an older one is read" and "Attestation version".
    IncompatibleContract,
    /// An unexpected runner fault — the runner reached a state its own logic rules
    /// out ([`exit::INTERNAL`], 104). **A genuine bug**, never an environment
    /// problem: that is [`Self::Setup`]. Spelled as the `runner_exit.source` value
    /// for the same ending.
    Internal,
    /// The run exceeded a runner deadline — the whole-run `--timeout` or the
    /// `--idle-timeout` — and the runner tore the process tree down
    /// ([`exit::TIMEOUT`], 106). A runner-*imposed outcome*, not a child exit; which
    /// of the two deadlines fired is named by the `timeout` event's `reason`.
    /// Spelled as the `runner_exit.source` value for the same ending.
    Timeout,
    /// The run was cancelled by a **local stop signal** — `Ctrl-C`, `SIGTERM` /
    /// `SIGHUP`, or a Windows `Ctrl-Break` / console close / logoff / shutdown — and
    /// the runner tore the tree down ([`exit::CANCELLED`], 107). Which signal
    /// arrived is named by the `cancelled` event's `source`. Spelled as the
    /// `runner_exit.source` value for the same ending.
    Cancelled,
    /// The run was ended by a control-plane `cancel`: the same soft-stop → grace →
    /// hard-kill teardown as a `Ctrl-C`, triggered over the control channel
    /// ([`exit::CONTROL_CANCELLED`], 108). Spelled as the `runner_exit.source` value
    /// for the same ending.
    ControlCancel,
    /// The run was ended by a control-plane `kill`: the whole tree hard-killed
    /// immediately, no soft stop and no grace ([`exit::CONTROL_KILLED`], 109).
    /// Spelled as the `runner_exit.source` value for the same ending.
    ControlKill,
    /// The **preflight probe** found this binary's compatibility surface does not
    /// satisfy a `--require-*` expectation ([`exit::PROBE_INCOMPATIBLE`], 110) — a
    /// pre-launch verdict about *this binary*, never a run outcome, and the exact
    /// opposite direction from [`Self::IncompatibleContract`] (which is this build
    /// refusing someone else's declared version). The concrete unmet expectations
    /// are in `probe --json`'s own `mismatches` array on stdout.
    ProbeIncompatible,
    /// The per-user run registry itself could not be opened or scanned — the
    /// directory is missing, unreadable, or its entries could not be listed. Carries
    /// [`exit::SETUP`] (111) for the whole-registry commands (`list`, `prune`,
    /// `wait`, `events`, the `--all` fan-outs) and [`exit::CONTROL`] (103) when a
    /// by-`run-id` control client hits it, because there the consequence is that the
    /// single target could not be resolved. Split out from [`Self::Setup`] because
    /// "the registry is unreadable" points an operator at a specific, inspectable
    /// directory (`docs/registry.md`), unlike a generic setup failure.
    Registry,
    /// A setup / support failure ([`exit::SETUP`], 111): a prerequisite the runner
    /// needs to run — or to report a result — could not be established for an
    /// ordinary reason. An unwritable `--jsonl`/`--capture-dir`, an unopenable
    /// `--stdin-file`, an events stream that could not be read, an async runtime
    /// that would not build, a report that would not serialize. An environment
    /// condition the caller can usually act on, **never** a runner bug — that stays
    /// [`Self::Internal`]. Spelled as the `runner_exit.source` value for the same
    /// ending.
    Setup,
    /// The **`wait` subcommand's own** deadline elapsed while its target(s) were
    /// still live ([`exit::WAIT_TIMEOUT`], 112). *The waiter* gave up; the run was
    /// never touched and is still going. Never confuse it with [`Self::Timeout`],
    /// which means the opposite (the runner tore the tree down).
    WaitTimeout,
    /// A capture stream exceeded `--capture-max-bytes` while `--capture-overflow
    /// cancel` was active, and the runner ended the tree ([`exit::OUTPUT_OVERFLOW`],
    /// 113). Spelled as the `runner_exit.source` value for the same ending.
    OutputOverflow,
    /// `events --validate` found at least one line that does not conform to the
    /// event schema this binary embeds ([`exit::EVENTS_INVALID`], 114) — a verdict
    /// about a **document**, not about any run. The stream was found, opened, and
    /// read perfectly well: "it does not conform" is the answer, not a failure to
    /// produce one.
    EventsInvalid,
    /// `attest` established that the calling process is **not** in the container of
    /// the run it asked about ([`exit::NOT_A_MEMBER`], 115): the runner was reached,
    /// it named the caller from the control transport itself, and that identity is
    /// not one of its container's members.
    ///
    /// A *decided* answer, and the only kind here that reports one — every other
    /// value on this list names something that went wrong. It is deliberately kept
    /// apart from [`Self::ControlUnreachable`] and its siblings for that reason:
    /// "the runner says no" and "no runner said anything" call for opposite
    /// responses from an adapter gating work on membership. Not retryable: the
    /// process asking is the process asked about, and it will not become a member by
    /// asking again.
    NotAMember,
    /// `attest` could not be answered at all because the runner's platform cannot
    /// obtain a kernel-authenticated identity for the connecting client
    /// ([`exit::CONTROL`], 103, alongside every other "no answer you can act on").
    ///
    /// The fail-closed half of the same command: rather than degrade to an unproven
    /// "ok" — or to the caller's own claim about who it is — the runner declines to
    /// answer, and this names why. Emphatically **not** [`Self::NotAMember`]: nothing
    /// was established about membership either way. A consumer establishes this
    /// capability before it depends on it, with `probe --json --require-surface
    /// attest:peer-identity` against the runner's own binary (`docs/integration.md`);
    /// meeting it at runtime instead means that preflight was skipped or the runner
    /// is a different build. Not retryable: it is a property of the platform, not a
    /// transient condition.
    PeerIdentityUnsupported,
    /// `doctor` finished qualifying this host and the verdict is no
    /// ([`exit::HOST_UNQUALIFIED`], 116): a phase of the qualification failed, or a
    /// `--require-*` expectation about the host was not met.
    ///
    /// A verdict about the **machine**, and the exact counterpart of
    /// [`Self::ProbeIncompatible`], which is a verdict about the **binary**: one says
    /// the installed file does not expose the surface you need, the other that this
    /// environment did not successfully create a registry, contain a process,
    /// round-trip its control plane, and clean up. Which of `doctor`'s phases failed,
    /// and which requirement went unmet, is in the report on stdout — printed for a
    /// negative verdict exactly as it is for a positive one. Not retryable: a host
    /// that cannot contain a process does not become able to by being asked twice.
    HostUnqualified,
    /// A reserved-band code this build will not put a finer name to — the
    /// forward-compatible fallback, so the envelope can always be produced and `kind`
    /// is never absent or invented. Read `code`, not `kind`, when this appears.
    ///
    /// Reachable in one narrow situation — a `run --detach` relays the exit code of
    /// the copy it respawned from its own path, and that copy can be a *different
    /// build* if the binary on disk was replaced in between (`src/run/detach.rs`,
    /// `detached_start_failure`) — which arrives in **two** shapes, both answered
    /// here:
    ///
    /// - a code **no build assigns**: the retired `105`, or the still-reserved
    ///   `117`-`119`. Naming a number whose meaning is unassigned would be worse than
    ///   saying so (see [`Self::for_code`]);
    /// - a code **this build assigns to a different subcommand**: `110`
    ///   ([`Self::ProbeIncompatible`]), `112` ([`Self::WaitTimeout`]), `114`
    ///   ([`Self::EventsInvalid`]), `115` ([`Self::NotAMember`]), `116`
    ///   ([`Self::HostUnqualified`]) — minted only by
    ///   `probe`, `wait`, `events --validate`, `attest`, and `doctor`, and unreachable
    ///   from `run`. Reading a foreign build's
    ///   number through this build's table would state a verdict about a run that
    ///   nothing established: a relayed `112` would claim `wait_timeout` — the one
    ///   retryable kind, meaning "the run is still going, wait again" — for a run that
    ///   never started, and would contradict `error.schema.json`'s own
    ///   `wait_timeout ⇒ operation: "wait"` conditional. The relay therefore names
    ///   only the codes `run` itself mints and leaves the rest here.
    Unknown,
}

impl ErrorKind {
    /// The value's spelling on the wire — the single source of truth for it, used by
    /// [`serde::Serialize`](serde::Serialize) below, by the published
    /// `fixtures/schema/cli/error.schema.json` enum, and by the tables in
    /// `docs/exit-codes.md` and `docs/integration.md`.
    ///
    /// Deliberately an exhaustive `match` with no wildcard arm, so a new variant
    /// cannot be added without deciding its published name (`docs/schema.md`'s
    /// "grep every parallel enumeration" discipline, applied at the type level).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::SpawnError => "spawn_error",
            Self::ContainerError => "container_error",
            Self::NotFound => "not_found",
            Self::Stale => "stale",
            Self::Unprobed => "unprobed",
            Self::AmbiguousRunId => "ambiguous_run_id",
            Self::ControlUnreachable => "control_unreachable",
            Self::IpcDeadline => "ipc_deadline",
            Self::IncompatibleContract => "incompatible_contract",
            Self::Internal => "internal",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ControlCancel => "control_cancel",
            Self::ControlKill => "control_kill",
            Self::ProbeIncompatible => "probe_incompatible",
            Self::Registry => "registry",
            Self::Setup => "setup",
            Self::WaitTimeout => "wait_timeout",
            Self::OutputOverflow => "output_overflow",
            Self::EventsInvalid => "events_invalid",
            Self::NotAMember => "not_a_member",
            Self::PeerIdentityUnsupported => "peer_identity_unsupported",
            Self::HostUnqualified => "host_unqualified",
            Self::Unknown => "unknown",
        }
    }

    /// Whether repeating the **identical** invocation, unchanged, can plausibly
    /// succeed later without the caller changing anything.
    ///
    /// A derived convenience, and deliberately a pure function of the kind: it never
    /// depends on the message, the operation, or anything observed at runtime, so
    /// the two fields can never disagree and an adapter may equally well keep its own
    /// copy of this table.
    ///
    /// It is **conservative in one direction only**: `true` is a considered "a retry
    /// is a reasonable thing to do here", while `false` means "this build does not
    /// promise a retry helps" rather than "provably permanent". Only three kinds are
    /// retryable, and each for a stated reason:
    ///
    /// - [`Self::Unprobed`] — nothing at all was established about the entry, so a
    ///   second probe may establish something (if it keeps repeating, the registry
    ///   directory is what to investigate, not the retry count);
    /// - [`Self::IpcDeadline`] — a live runner was simply slower than a bounded
    ///   window, which a busy machine can clear on its own;
    /// - [`Self::WaitTimeout`] — the run is still live and untouched, so waiting
    ///   again (or longer) is exactly the intended response.
    ///
    /// Every `run`-family kind is `false` on purpose: re-running a command is a *new
    /// run with new side effects*, not a retry of a read-only query, and whether that
    /// is safe is the caller's judgement, never this binary's advice. The remaining
    /// failures are verdicts a retry cannot change (a confirmed-stale entry, an
    /// ambiguous id, a refused version, a non-conforming document) or environment
    /// problems that need action (`docs/integration.md` §6 says the same in prose:
    /// "Retrying does not clear it").
    pub fn retryable(self) -> bool {
        match self {
            Self::Unprobed | Self::IpcDeadline | Self::WaitTimeout => true,
            Self::Usage
            | Self::SpawnError
            | Self::ContainerError
            | Self::NotFound
            | Self::Stale
            | Self::AmbiguousRunId
            | Self::ControlUnreachable
            | Self::IncompatibleContract
            | Self::Internal
            | Self::Timeout
            | Self::Cancelled
            | Self::ControlCancel
            | Self::ControlKill
            | Self::ProbeIncompatible
            | Self::Registry
            | Self::Setup
            | Self::OutputOverflow
            | Self::EventsInvalid
            | Self::NotAMember
            | Self::PeerIdentityUnsupported
            | Self::HostUnqualified
            | Self::Unknown => false,
        }
    }

    /// The kind a reserved-band code carries when the failing code path did not name
    /// a finer one — the default every [`RunnerError::new`] starts with.
    ///
    /// Codes that map one-to-one onto a kind need nothing else; the codes that carry
    /// several genuinely different situations ([`exit::CONTROL`], [`exit::SETUP`])
    /// default to their **widest** honest reading and are narrowed at the call sites
    /// that know more, through [`RunnerError::with_kind`]. Defaulting the other way
    /// round — assuming the narrow case — would let a new 103 path silently claim to
    /// be, say, a confirmed-stale entry.
    pub fn for_code(code: u8) -> Self {
        match code {
            exit::USAGE => Self::Usage,
            exit::SPAWN => Self::SpawnError,
            exit::BACKEND => Self::ContainerError,
            exit::CONTROL => Self::ControlUnreachable,
            exit::INTERNAL => Self::Internal,
            exit::TIMEOUT => Self::Timeout,
            exit::CANCELLED => Self::Cancelled,
            exit::CONTROL_CANCELLED => Self::ControlCancel,
            exit::CONTROL_KILLED => Self::ControlKill,
            exit::PROBE_INCOMPATIBLE => Self::ProbeIncompatible,
            exit::SETUP => Self::Setup,
            exit::WAIT_TIMEOUT => Self::WaitTimeout,
            exit::OUTPUT_OVERFLOW => Self::OutputOverflow,
            exit::EVENTS_INVALID => Self::EventsInvalid,
            exit::NOT_A_MEMBER => Self::NotAMember,
            exit::HOST_UNQUALIFIED => Self::HostUnqualified,
            // `NOT_IMPLEMENTED` (105, retired) and the reserved `117`-`119`: no
            // active path mints them, and inventing a name for a number whose
            // meaning is not assigned would be worse than saying so.
            _ => Self::Unknown,
        }
    }
}

impl serde::Serialize for ErrorKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// One failed invocation, as the bounded object `--error-format json` prints.
///
/// Field order here is the order they appear on the wire, chosen to read
/// verdict-first: the version to pin, the code and kind that carry the decision, the
/// invocation they describe, the retry hint, and only then the prose.
#[derive(Debug, serde::Serialize)]
pub struct ErrorEnvelope<'a> {
    /// Always [`ERROR_VERSION`]. Check it before trusting the rest.
    pub error_version: u32,
    /// The reserved-band exit code this invocation is exiting with — the same number
    /// the process returns, so the envelope and `$?` can never disagree.
    pub code: u8,
    /// The finer name of what failed. See [`ErrorKind`].
    pub kind: ErrorKind,
    /// The subcommand that failed (`run`, `inspect`, `cancel`, `kill`, `attest`,
    /// `wait`, `events`, `list`, `prune`, `probe`, `doctor`) — always a `&'static str`
    /// from the CLI's own definition, never caller-supplied text.
    pub operation: &'static str,
    /// The run id the invocation named, or `null` when it named none: an `--all`
    /// fan-out, a whole-registry command (`list`/`prune`), a self-contained `probe`,
    /// a `doctor` (whose only run is the scratch one it mints for itself), or a `run`
    /// that let the runner generate its id. Present rather than omitted
    /// when null, so a consumer never distinguishes absent from unknown — the same
    /// convention the other published shapes use.
    pub run_id: Option<&'a str>,
    /// Whether repeating this exact invocation may succeed later — see
    /// [`ErrorKind::retryable`], of which this is purely a projection.
    pub retryable: bool,
    /// The human-readable explanation, verbatim from the failure itself: the very
    /// text the default prose mode prints after `processkit-cli: `.
    ///
    /// **Explicitly not part of the stable contract.** It is free to be reworded,
    /// re-punctuated, or made more specific in any release, and the golden fixture
    /// published for this envelope does not pin its text. Log it, show it to a
    /// human, attach it to a ticket — but never branch on it: that is what `code`
    /// and `kind` are for.
    pub message: String,
}

impl<'a> ErrorEnvelope<'a> {
    /// Describe `error` as it happened during `operation`, against `run_id` (if the
    /// invocation named one).
    pub fn new(error: &RunnerError, operation: &'static str, run_id: Option<&'a str>) -> Self {
        let kind = error.kind();
        Self {
            error_version: ERROR_VERSION,
            code: error.code(),
            kind,
            operation,
            run_id,
            retryable: kind.retryable(),
            message: error.to_string(),
        }
    }

    /// The single line printed to stderr, without its trailing newline.
    ///
    /// Serialization of this object cannot actually fail — every field is a scalar or
    /// a plain string — but the fallback is spelled out anyway rather than
    /// `unwrap`ped, because this is the *error* path: a panic here would replace a
    /// diagnosable failure with an undiagnosable one. The fallback keeps the two
    /// fields a consumer decides on (`code`, `kind`) and drops only the caller-supplied
    /// `run_id` and the prose, so it needs no escaping of its own and still satisfies
    /// `fixtures/schema/cli/error.schema.json`. This mirrors `control`'s
    /// `serialize_ack`, which takes the same precaution for the same reason.
    pub fn render_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!(
                r#"{{"error_version":{ERROR_VERSION},"code":{},"kind":"{}","operation":"{}","run_id":null,"retryable":{},"message":"the failure could not be rendered as JSON"}}"#,
                self.code,
                self.kind.as_str(),
                self.operation,
                self.retryable
            )
        })
    }
}

/// Report a runner-own failure on stderr in the format the invocation asked for:
/// the historical `processkit-cli: <message>` prose by default, the bounded
/// [`ErrorEnvelope`] under `--error-format json`.
///
/// The single place that decision is made, shared by [`crate::run`]'s own exit path
/// and by `src/main.rs`'s `report`, so the two can never drift into printing
/// different things for the same failure.
pub fn report_failure(
    error: &RunnerError,
    format: ErrorFormat,
    operation: &'static str,
    run_id: Option<&str>,
) {
    match format {
        ErrorFormat::Human => eprintln!("processkit-cli: {error}"),
        ErrorFormat::Json => {
            eprintln!(
                "{}",
                ErrorEnvelope::new(error, operation, run_id).render_line()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{Value, json};

    /// Every kind this build can name, for the totality checks below. Listed by hand
    /// rather than derived so that adding a variant fails a test until it is
    /// considered here too.
    const ALL: &[ErrorKind] = &[
        ErrorKind::Usage,
        ErrorKind::SpawnError,
        ErrorKind::ContainerError,
        ErrorKind::NotFound,
        ErrorKind::Stale,
        ErrorKind::Unprobed,
        ErrorKind::AmbiguousRunId,
        ErrorKind::ControlUnreachable,
        ErrorKind::IpcDeadline,
        ErrorKind::IncompatibleContract,
        ErrorKind::Internal,
        ErrorKind::Timeout,
        ErrorKind::Cancelled,
        ErrorKind::ControlCancel,
        ErrorKind::ControlKill,
        ErrorKind::ProbeIncompatible,
        ErrorKind::Registry,
        ErrorKind::Setup,
        ErrorKind::WaitTimeout,
        ErrorKind::OutputOverflow,
        ErrorKind::EventsInvalid,
        ErrorKind::NotAMember,
        ErrorKind::PeerIdentityUnsupported,
        ErrorKind::HostUnqualified,
        ErrorKind::Unknown,
    ];

    /// The kinds a failing `run` reports, spelled as the terminal `runner_exit`
    /// event's `source` values. Named once because two tests need the same set: one
    /// holds those spellings against the event schema, and one excludes them from the
    /// per-subcommand conditionals below — `run` mints this family as a whole rather
    /// than one verdict per reserved code, which is why the schema pins none of them
    /// to an operation.
    const RUN_FAMILY: &[ErrorKind] = &[
        ErrorKind::SpawnError,
        ErrorKind::ContainerError,
        ErrorKind::Timeout,
        ErrorKind::Cancelled,
        ErrorKind::ControlCancel,
        ErrorKind::ControlKill,
        ErrorKind::OutputOverflow,
        ErrorKind::Setup,
        ErrorKind::Internal,
    ];

    fn envelope_of(error: &RunnerError) -> Value {
        serde_json::from_str(&ErrorEnvelope::new(error, "inspect", Some("build-42")).render_line())
            .expect("the envelope is valid JSON")
    }

    /// The published schema document itself, read off disk — the consumer-facing half
    /// of this vocabulary, and the only honest thing to check this build against.
    fn published_schema() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/schema/cli/error.schema.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        serde_json::from_str(&text).expect("the schema document is valid JSON")
    }

    #[test]
    fn the_published_schema_enumerates_exactly_the_kinds_this_build_can_emit() {
        // A kind added here and not published there would be invisible to every
        // adapter.
        let schema = published_schema();
        let published: Vec<&str> = schema["$defs"]["errorEnvelope"]["properties"]["kind"]["enum"]
            .as_array()
            .expect("the schema enumerates the kind vocabulary")
            .iter()
            .map(|value| value.as_str().expect("each kind is a string"))
            .collect();

        let emitted: Vec<&str> = ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(
            published, emitted,
            "fixtures/schema/cli/error.schema.json must publish exactly the kinds this build \
             emits, in the same order"
        );
    }

    #[test]
    fn a_kind_with_a_reserved_code_of_its_own_is_pinned_to_the_command_that_mints_it() {
        // Some codes in the band name exactly one verdict, and each of those verdicts
        // is minted by exactly one subcommand. The schema states both facts as an
        // `allOf` conditional keyed on `kind` — which is what makes a *dishonest*
        // envelope (`{"kind":"host_unqualified","operation":"run","code":102}`)
        // invalid rather than merely unusual, and is the reason a kind must never be
        // assigned to a failure it does not describe.
        //
        // A missing conditional is invisible to a review that reads one task's diff
        // (it can only be seen by comparing the edit against a sibling kind's shape),
        // so it is asserted here instead. The set is **derived** from this build's own
        // code-to-kind table rather than listed, so the next kind given a code of its
        // own cannot land without its conditional: what is excluded is only what
        // genuinely has no single command — `usage` (any subcommand, in practice a
        // relayed detached start), the two codes several kinds refine (`CONTROL`,
        // `SETUP`), and the `run` family above.
        let schema = published_schema();
        let envelope = &schema["$defs"]["errorEnvelope"];
        let branches = envelope["allOf"]
            .as_array()
            .expect("the schema states its conditionals as an `allOf`");
        let operations: Vec<&str> = envelope["properties"]["operation"]["enum"]
            .as_array()
            .expect("the schema enumerates the operations")
            .iter()
            .map(|value| value.as_str().expect("each operation is a string"))
            .collect();

        let mut pinned = Vec::new();
        for code in exit::RUNNER_RANGE_START..=exit::RUNNER_RANGE_END {
            let kind = ErrorKind::for_code(code);
            if kind == ErrorKind::Unknown
                || matches!(code, exit::USAGE | exit::CONTROL | exit::SETUP)
                || RUN_FAMILY.contains(&kind)
            {
                continue;
            }
            let branch = branches
                .iter()
                .find(|branch| branch["if"]["properties"]["kind"]["const"] == json!(kind.as_str()))
                .unwrap_or_else(|| {
                    panic!(
                        "fixtures/schema/cli/error.schema.json must pin `{}` to the one \
                         subcommand that mints it and to code {code}, the way every sibling \
                         verdict kind is pinned — without it the published contract accepts an \
                         envelope claiming that kind for any command",
                        kind.as_str()
                    )
                });
            assert_eq!(
                branch["then"]["properties"]["code"]["const"],
                json!(code),
                "`{}`'s conditional must pin the code this build assigns it",
                kind.as_str()
            );
            let operation = branch["then"]["properties"]["operation"]["const"]
                .as_str()
                .unwrap_or_else(|| {
                    panic!(
                        "`{}`'s conditional must pin the subcommand that mints it",
                        kind.as_str()
                    )
                });
            assert!(
                operations.contains(&operation),
                "`{}` is pinned to `{operation}`, which is not one of the published operations \
                 {operations:?}",
                kind.as_str()
            );
            pinned.push(kind.as_str());
        }
        // A guard on the derivation itself: if the exclusions above ever swallowed
        // everything, the loop would pass by checking nothing at all.
        assert_eq!(
            pinned,
            vec![
                "probe_incompatible",
                "wait_timeout",
                "events_invalid",
                "not_a_member",
                "host_unqualified"
            ],
            "the verdict kinds this build gives a code of their own"
        );
    }

    #[test]
    fn the_run_family_kinds_are_spelled_exactly_as_the_event_streams_source_values() {
        // The envelope reuses `runnerExit.source`'s vocabulary for the endings that
        // vocabulary already names, rather than minting a second set of words for
        // the same facts. That reuse is only honest if it cannot drift, so it is
        // checked against the published event schema itself — not against a copy of
        // it kept here.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema/v1/schema.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let schema: Value =
            serde_json::from_str(&text).expect("the event schema document is valid JSON");
        let sources: Vec<&str> = schema["$defs"]["runnerExit"]["properties"]["source"]["enum"]
            .as_array()
            .expect("the event schema enumerates runner_exit.source")
            .iter()
            .map(|value| value.as_str().expect("each source is a string"))
            .collect();

        for kind in RUN_FAMILY.iter().copied() {
            assert!(
                sources.contains(&kind.as_str()),
                "`{}` must stay spelled as the runner_exit.source value it mirrors; \
                 fixtures/schema/v1/schema.json has {sources:?}",
                kind.as_str()
            );
        }

        // The converse is deliberately not asserted: `child_exit` is not a failure
        // and has no envelope of its own, and the kinds that refine CONTROL/SETUP
        // (or belong to a reader command) have no counterpart in a run's stream.
        assert!(
            !ALL.iter().any(|kind| kind.as_str() == "child_exit"),
            "a child's own exit is never a runner failure"
        );
    }

    #[test]
    fn every_kind_has_a_distinct_wire_name() {
        let mut names: Vec<&str> = ALL.iter().map(|kind| kind.as_str()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two kinds share a wire name: {names:?}");
    }

    #[test]
    fn the_kind_axis_is_never_coarser_than_the_exit_code() {
        // Every code the band assigns has a name of its own, so a consumer that
        // branches on `kind` alone never loses a distinction the code would have
        // made. The reverse (one code, several kinds) is the whole point and is
        // asserted separately below.
        let mut seen = Vec::new();
        for code in [
            exit::USAGE,
            exit::SPAWN,
            exit::BACKEND,
            exit::CONTROL,
            exit::INTERNAL,
            exit::TIMEOUT,
            exit::CANCELLED,
            exit::CONTROL_CANCELLED,
            exit::CONTROL_KILLED,
            exit::PROBE_INCOMPATIBLE,
            exit::SETUP,
            exit::WAIT_TIMEOUT,
            exit::OUTPUT_OVERFLOW,
            exit::EVENTS_INVALID,
            exit::NOT_A_MEMBER,
            exit::HOST_UNQUALIFIED,
        ] {
            let kind = ErrorKind::for_code(code);
            assert_ne!(
                kind,
                ErrorKind::Unknown,
                "every assigned code has a name of its own: {code}"
            );
            assert!(
                !seen.contains(&kind),
                "two assigned codes collapsed onto `{}`",
                kind.as_str()
            );
            seen.push(kind);
        }
    }

    #[test]
    fn control_and_setup_are_the_codes_a_kind_refines() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::Stale,
            ErrorKind::Unprobed,
            ErrorKind::AmbiguousRunId,
            ErrorKind::IpcDeadline,
            ErrorKind::IncompatibleContract,
            ErrorKind::PeerIdentityUnsupported,
        ] {
            assert_ne!(
                kind,
                ErrorKind::for_code(exit::CONTROL),
                "`{}` must be a narrowing of CONTROL, not its default reading",
                kind.as_str()
            );
        }
        assert_ne!(
            ErrorKind::Registry,
            ErrorKind::for_code(exit::SETUP),
            "`registry` must be a narrowing of SETUP, not its default reading"
        );
    }

    #[test]
    fn an_unassigned_reserved_code_is_named_unknown_rather_than_guessed() {
        // 105 is retired and 117-119 are reserved: no active path mints them, and a
        // relayed detached start failure is the one way a foreign build's code could
        // arrive here. The lower bound walks up from the *last assigned* code rather
        // than naming a number, so minting one moves this range instead of leaving a
        // stale assertion that the new code means nothing.
        assert_eq!(
            ErrorKind::for_code(exit::NOT_IMPLEMENTED),
            ErrorKind::Unknown
        );
        for code in exit::HOST_UNQUALIFIED + 1..=exit::RUNNER_RANGE_END {
            assert_eq!(
                ErrorKind::for_code(code),
                ErrorKind::Unknown,
                "reserved code {code} has no assigned meaning yet"
            );
        }
    }

    #[test]
    fn only_the_three_documented_kinds_advise_a_retry() {
        let retryable: Vec<&str> = ALL
            .iter()
            .filter(|kind| kind.retryable())
            .map(|kind| kind.as_str())
            .collect();
        assert_eq!(
            retryable,
            vec!["unprobed", "ipc_deadline", "wait_timeout"],
            "the retryable set is a documented contract (docs/exit-codes.md); changing it is a \
             contract change, not an implementation detail"
        );
    }

    #[test]
    fn the_envelope_is_one_bounded_line_of_scalars() {
        let error = RunnerError::new(exit::CONTROL, "cannot inspect run `build-42`: gone")
            .with_kind(ErrorKind::Stale);
        let line = ErrorEnvelope::new(&error, "inspect", Some("build-42")).render_line();
        assert!(
            !line.contains('\n'),
            "the envelope is exactly one line: {line}"
        );

        let value: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(
            value,
            json!({
                "error_version": 1,
                "code": 103,
                "kind": "stale",
                "operation": "inspect",
                "run_id": "build-42",
                "retryable": false,
                "message": "cannot inspect run `build-42`: gone",
            })
        );
        let object = value.as_object().expect("an object");
        assert!(
            object
                .values()
                .all(|field| !field.is_object() && !field.is_array()),
            "every field is a scalar — the envelope is bounded by construction: {line}"
        );
    }

    #[test]
    fn a_run_id_the_invocation_never_named_is_null_rather_than_absent() {
        let error = RunnerError::new(exit::SETUP, "could not read the run registry");
        let line = ErrorEnvelope::new(&error, "list", None).render_line();
        let value: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(value["run_id"], Value::Null);
        assert!(
            value.as_object().expect("an object").contains_key("run_id"),
            "the field is present-and-null, never omitted: {line}"
        );
    }

    #[test]
    fn the_code_and_the_retry_hint_are_derived_never_restated() {
        // Both fields are projections: the code is the process's own exit status and
        // the hint is a pure function of the kind, so neither can drift from what the
        // caller observes.
        let error = RunnerError::new(exit::WAIT_TIMEOUT, "stopped waiting for run `build-42`");
        let value = envelope_of(&error);
        assert_eq!(value["code"], json!(exit::WAIT_TIMEOUT));
        assert_eq!(value["kind"], json!("wait_timeout"));
        assert_eq!(value["retryable"], json!(true));
    }

    #[test]
    fn an_explicit_kind_narrows_the_default_without_touching_the_code() {
        let plain = RunnerError::new(exit::CONTROL, "cannot inspect run `x`");
        assert_eq!(envelope_of(&plain)["kind"], json!("control_unreachable"));

        let narrowed = RunnerError::new(exit::CONTROL, "cannot inspect run `x`")
            .with_kind(ErrorKind::AmbiguousRunId);
        let value = envelope_of(&narrowed);
        assert_eq!(value["kind"], json!("ambiguous_run_id"));
        assert_eq!(
            value["code"],
            json!(exit::CONTROL),
            "narrowing the kind never changes the exit code the caller sees"
        );
    }

    #[test]
    fn control_characters_in_a_message_are_escaped_not_stripped() {
        // JSON renderers in this crate do not use `text::terminal_safe*`: serde_json
        // escapes controls without changing the data (see `src/text.rs`). The line
        // must still be one line.
        let error = RunnerError::new(exit::SETUP, "could not open \"a\nb\" \u{1b}[31m");
        let line = ErrorEnvelope::new(&error, "events", None).render_line();
        assert!(!line.contains('\n'), "still one line: {line}");
        let value: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(
            value["message"],
            json!("could not open \"a\nb\" \u{1b}[31m")
        );
    }

    #[test]
    fn the_defensive_fallback_line_is_still_a_valid_envelope() {
        // The real path cannot fail, so exercise the fallback's own text directly:
        // what matters is that it parses and keeps the two fields a consumer decides
        // on.
        let error = RunnerError::new(exit::CONTROL, "unused").with_kind(ErrorKind::IpcDeadline);
        let envelope = ErrorEnvelope::new(&error, "cancel", Some("build-42"));
        let fallback = format!(
            r#"{{"error_version":{ERROR_VERSION},"code":{},"kind":"{}","operation":"{}","run_id":null,"retryable":{},"message":"the failure could not be rendered as JSON"}}"#,
            envelope.code,
            envelope.kind.as_str(),
            envelope.operation,
            envelope.retryable
        );
        let value: Value = serde_json::from_str(&fallback).expect("the fallback is valid JSON");
        assert_eq!(value["code"], json!(exit::CONTROL));
        assert_eq!(value["kind"], json!("ipc_deadline"));
        assert_eq!(value["retryable"], json!(true));
    }
}
