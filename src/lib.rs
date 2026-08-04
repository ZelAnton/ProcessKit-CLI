//! Internal library crate backing the `processkit-cli` binary (`src/main.rs`).
//!
//! # Not a stable public API
//!
//! **This library is an internal implementation detail and is _not_ a stable
//! public Rust API.** It is published to crates.io only because it ships in the
//! same crate as the `processkit-cli` binary; it exists so the crate's own test,
//! property-test, fuzz, and benchmark tiers (and any future in-tree tooling) can
//! reach the runner's internals directly instead of only through the shipped
//! binary. Every module below is `#[doc(hidden)]`, and no item here is covered by
//! semantic-versioning guarantees: names, signatures, visibility, and behavior may
//! change or disappear in any release, including a patch release. Do not depend on
//! it as a library.
//!
//! The crate's **supported compatibility surface** is the command-line binary
//! only: the CLI flags/subcommands (see [`cli`]), the reserved runner-own
//! exit-code contract (see [`exit`] and `docs/exit-codes.md`), and the versioned
//! JSONL event `schema_version` (see [`events`] and `docs/schema.md`). Those —
//! not any Rust symbol below — are what this project keeps stable.
//!
//! # What the runner does
//!
//! The `run` subcommand is implemented in [`run`]: it spawns the child into a
//! ProcessKit container this process owns, echoes the child's output live,
//! forwards its exit code faithfully, and writes the versioned JSONL lifecycle
//! events (see [`events`] and `docs/schema.md`) to the `--jsonl` file. The control
//! plane's clients live in [`control`]: `inspect` reaches a live `run` over the
//! per-user registry and local transport and prints a machine-readable snapshot,
//! and `cancel`/`kill` reach the same live runner over the same transport to end
//! it — a graceful soft-stop → grace → hard-kill for `cancel`, an immediate hard
//! kill for `kill` — each a distinguishable outcome in the JSONL stream and by
//! exit code. [`list`] is the discovery counterpart: it scans the same registry
//! and prints every entry, whatever its health (live/stale/unprobed), for a caller that has lost (or never
//! had) a `run_id`; [`prune`] is the cleanup counterpart, reaping the
//! confirmed-stale leftovers of runners that died abruptly while never touching a
//! live entry; and [`wait`] is the *lifetime* counterpart, blocking on the same
//! registry until a run is no longer live, for a supervisor that is not the
//! runner's parent and so cannot wait on it as a child process. [`control`] also
//! carries the fourth control-plane client, `attest`: it asks a live runner whether
//! the *calling* process is inside that run's container, answering from the
//! kernel-supplied identity of whoever opened the control connection rather than
//! from anything the caller said, so an adapter's "the caller belongs to run X"
//! convention becomes a runner-checked fact (a decided negative is its own reserved
//! code, [`exit::NOT_A_MEMBER`]). [`events_cmd`]
//! closes the loop by reading a run's JSONL stream back — rendering, following,
//! passing through, or schema-checking the very events [`events`] wrote — resolving
//! the stream through the same registry, and mutating nothing.
//!
//! The two preflight commands sit either side of the same question and answer
//! different halves of it. [`probe`] is the side-effect-free one: it reports (and,
//! with `--require-*`, verifies) what *this binary* is — its CLI surface, its
//! reserved exit-code band, its JSONL `schema_version` — without spawning, opening,
//! or binding anything. [`doctor`] is the side-effecting counterpart, and answers
//! what *this host* can actually do: it performs a bounded scratch run of this
//! binary's own harmless child through the ordinary `run`/control-plane path and
//! reports the facts it observed doing so — the registry directory it created and its
//! owner-only protection, the containment mechanism and abrupt-cleanup level this
//! machine gave it, a full `inspect`/`cancel`/terminal round-trip over the local
//! transport, a confirmed-empty teardown, and per-phase timings — leaving nothing
//! behind on success and a named diagnostics directory on failure (a negative verdict
//! is its own reserved code, [`exit::HOST_UNQUALIFIED`]). "The installed binary is
//! compatible" and "this host has successfully exercised the containment and control
//! path" are different claims, and these are the two commands that make them.
//! [`error_envelope`]
//! is the failure-side counterpart to all of them: under the global
//! `--error-format json` it renders whatever any of these commands failed with as
//! one bounded, versioned JSON object on stderr, so an adapter branches on a
//! published `kind` instead of on prose. It adds no fourth compatibility surface:
//! like every other machine-readable output it rides on the flag that turns it on
//! and on the reserved code it carries, and pins its own shape in the payload with
//! `error_version` — the same relationship `probe --json`'s `probe_version`,
//! `inspect`'s `snapshot_version`, `attest --json`'s `attestation_version`, and
//! `doctor --json`'s `doctor_version` have
//! to that surface (see `docs/compatibility.md`,
//! "Machine-output schemas"). Those version fields are an independent axis *layered
//! on* the three surfaces, never a count of them. The compatibility
//! surface itself — CLI flags (see [`cli`]), the exit-code
//! contract (see [`exit`] and `docs/exit-codes.md`), and the JSONL `schema_version`
//! (see [`events`] and `docs/schema.md`) — is fixed, and is the same three-item list
//! this page opens with.

#[doc(hidden)]
pub mod capture;
#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod control;
#[doc(hidden)]
pub mod doctor;
#[doc(hidden)]
pub mod duration_fmt;
#[doc(hidden)]
pub mod error_envelope;
#[doc(hidden)]
pub mod events;
#[doc(hidden)]
pub mod events_cmd;
#[doc(hidden)]
pub mod exit;
#[doc(hidden)]
pub mod hash;
#[doc(hidden)]
pub mod labels;
#[doc(hidden)]
pub mod list;
#[doc(hidden)]
pub mod probe;
#[doc(hidden)]
pub mod prune;
#[doc(hidden)]
pub mod registry;
#[doc(hidden)]
pub mod run;
mod text;
#[doc(hidden)]
pub mod wait;
#[cfg(windows)]
#[doc(hidden)]
pub mod win_security;
