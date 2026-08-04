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
//! runner's parent and so cannot wait on it as a child process. [`events_cmd`]
//! closes the loop by reading a run's JSONL stream back — rendering, following,
//! passing through, or schema-checking the very events [`events`] wrote — resolving
//! the stream through the same registry, and mutating nothing. [`error_envelope`]
//! is the failure-side counterpart to all of them: under the global
//! `--error-format json` it renders whatever any of these commands failed with as
//! one bounded, versioned JSON object on stderr, so an adapter branches on a
//! published `kind` instead of on prose. The compatibility
//! surface — CLI flags (see [`cli`]), the exit-code
//! contract (see [`exit`] and `docs/exit-codes.md`), the JSONL `schema_version`
//! (see [`events`] and `docs/schema.md`), and the machine-error envelope's own
//! `error_version` (see [`error_envelope`] and `docs/exit-codes.md`) — is fixed.

#[doc(hidden)]
pub mod capture;
#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod control;
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
