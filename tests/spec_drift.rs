//! Upstream identifier drift gate: hold this crate's projections of ProcessKit's
//! closed vocabularies against the dictionary ProcessKit itself ships.
//!
//! # What this exists to catch
//!
//! Every enum this CLI projects onto a machine contract is `#[non_exhaustive]`
//! upstream. Each projection therefore ends in a conservative `_` arm — `"unknown"`
//! for a mechanism, `"none"` for an abrupt-cleanup guarantee, `"failed"` for a soft
//! signal. Those arms are correct behaviour and deliberately not removable: a build
//! that meets a value it predates must degrade honestly rather than guess.
//!
//! What they are *not* is a notification. When ProcessKit 3.3 added
//! `Mechanism::ProcessReaper`, this crate compiled unchanged, every test stayed
//! green, and the new mechanism quietly reported itself as `unknown` — visible only
//! to whoever eventually built the binary on FreeBSD. The gap between "upstream grew
//! a value" and "we noticed" was bounded by nothing.
//!
//! Since 3.2 ProcessKit closes that gap on its side by publishing
//! `spec/identifiers.json`: for each closed enum, its Rust path, its `class`, and
//! one stable `identifier` per variant, generated from a compile-time-exhaustive
//! `match` inside the crate (so the dictionary cannot itself go stale behind the
//! types it describes). The file ships **inside the published crate package**, so
//! the dictionary belonging to exactly the version `Cargo.lock` resolved is already
//! on disk after any ordinary build — no network, no vendored copy, no second
//! version to keep in sync.
//!
//! This tier reads that dictionary and requires every identifier in it to be
//! *deliberately* represented in the projection that publishes the vocabulary. A new
//! upstream variant fails the build here, naming the enum and the identifier, at the
//! moment the dependency is bumped — instead of surfacing as a wrong string on
//! someone's wire months later.
//!
//! # What it deliberately does not do
//!
//! It never widens a projection, a schema, or a fixture. Deciding what a new
//! upstream value *means* for this project's normative contract — whether it earns a
//! new enum member, joins an existing bucket, or stays unreachable — is a change to
//! a published contract, and belongs to a task with a human in it. This gate's whole
//! job is to make sure that task gets opened.
//!
//! # The three recorded decisions
//!
//! **1. Locating the dictionary: `cargo metadata`, no network, no vendored copy.**
//! [`dictionary_path`] runs `cargo metadata --format-version 1 --locked --offline`
//! and walks the resolve graph from this package to its own `processkit` dependency,
//! then reads `spec/identifiers.json` next to that package's manifest. Resolving via
//! the graph rather than by "the package named processkit" keeps the answer correct
//! if a transitive second major version is ever in the tree; `--locked` pins it to
//! the version `Cargo.lock` actually resolves rather than whatever a fresh
//! resolution would pick; `--offline` keeps the gate hermetic. Because the path
//! comes from the resolved package rather than a registry-layout guess, the gate
//! follows a `[patch.crates-io]` git checkout automatically — which is what lets
//! `canary.yml` run it against ProcessKit's main branch and see a new identifier
//! *before* it is released.
//!
//! **2. A missing dictionary fails, and is never skipped.** If the file is not where
//! the resolved package says it should be — a patched, vendored, or path dependency
//! whose source tree omits `spec/` — [`load_dictionary`] panics with the resolved
//! version, the probed path, and what to do about it. It does not `return` early
//! with a message: a test that passes while printing an explanation is a *green*
//! test, cargo hides its stdout by default, and the result would be a gate that
//! reports "no drift" precisely when it has stopped looking. That is the exact
//! failure this tier exists to prevent, so it is not available as a degraded mode.
//! There is deliberately no environment-variable escape hatch either: the tier is
//! opt-in (`--features spec-drift`), so a build that cannot satisfy it simply does
//! not enable it, which is a visible choice in a workflow file rather than an
//! invisible one in an environment.
//!
//! **3. Its own tier, not the default `cargo test`.** See the `spec-drift` feature's
//! comment in `Cargo.toml` for why: it needs a working cargo and an unpacked
//! dependency source, and its verdict is host-independent. CI runs it once, in the
//! gating `spec-drift` job; `canary.yml` runs it against upstream main; and
//! `just spec-drift` is the local equivalent. CONTRIBUTING.md, "Upstream identifier
//! drift", is the narrative version, including what to do when it fails.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use processkit::{
    LimitKind, LimitVerdict, Mechanism, Outcome, ParentDeathCleanup, Signal, SoftSignal,
    SoftStopScope,
};
use processkit_cli::events::{abrupt_cleanup_scope_str, mechanism_str, outcome_fields};
use processkit_cli::run::{
    limit_kind_str, limit_verdict_str, soft_signal_str, soft_stop_scope_str,
};
use serde_json::Value;

/// The dictionary shape this gate's parser understands. A bump means ProcessKit
/// restructured the file, and the parser below has to be re-read against it rather
/// than silently interpreting a document it no longer understands.
const SUPPORTED_DICTIONARY_SCHEMA_VERSION: u64 = 1;

/// How a dictionary identifier is checked against this crate's Rust projection.
///
/// The split follows the dictionary's own `class`, because the two classes differ in
/// exactly the capability this gate needs. A `configurable` value round-trips
/// through upstream's `from_name`, so the gate can turn an identifier *back* into the
/// variant it names and drive the real projection with it — including a variant this
/// crate's source never mentions. A `report_only` value has no `from_name` inverse
/// (upstream reports it and is never handed it back), so the gate instead runs every
/// variant this build can name through the real projection and compares identifier
/// *sets*.
enum Projection {
    /// Resolve the identifier to its variant and render it. `None` means this build's
    /// ProcessKit does not know the identifier at all — dictionary and linked crate
    /// disagree, which is its own failure.
    Parsed(fn(&str) -> Option<&'static str>),
    /// The identifiers this build's projection can emit, produced by running each
    /// nameable variant through the real projection function.
    Emitted(fn() -> BTreeSet<&'static str>),
}

/// One upstream vocabulary this CLI republishes, and every surface that republishes
/// it.
struct Vocabulary {
    /// The enum's `path` key in `spec/identifiers.json`.
    path: &'static str,
    /// The `class` the dictionary is expected to give it. Checked, not assumed: a
    /// reclassification upstream changes which [`Projection`] strategy is even
    /// available, so it must not pass unnoticed.
    class: &'static str,
    /// This crate's Rust projection of the vocabulary.
    projection: Projection,
    /// JSON Schema property names whose `enum` array publishes this vocabulary, in
    /// `fixtures/schema/v1/schema.json` and `fixtures/schema/cli/*.schema.json`.
    /// Located by property name across *every* schema document rather than by a
    /// fixed pointer, so a new echo site is covered the moment it is added and a
    /// forgotten one is a failure — the recurring "updated the schema but not the
    /// `attest`/`doctor` mirror of it" miss.
    wire_properties: &'static [&'static str],
    /// Why [`Vocabulary::wire_properties`] is empty, for the one vocabulary that
    /// reaches the wire through an unconstrained string.
    no_wire_reason: Option<&'static str>,
}

/// The vocabularies this CLI projects, derived by reading the projections rather than
/// by trusting any prose: each entry below names the function that renders it and the
/// published enum(s) that carry it.
fn projected_vocabularies() -> Vec<Vocabulary> {
    vec![
        // `run_started.mechanism`, plus the `inspect`/`attest`/`doctor` machine
        // reports, via `events::mechanism_str` (`_ => "unknown"`).
        Vocabulary {
            path: "processkit::Mechanism",
            class: "configurable",
            projection: Projection::Parsed(|id| Mechanism::from_name(id).map(mechanism_str)),
            wire_properties: &["mechanism"],
            no_wire_reason: None,
        },
        // `run_started.abrupt_cleanup` and `doctor`'s containment report, via
        // `events::abrupt_cleanup_scope_str` (`_ => "none"`, the never-overclaim
        // fallback).
        Vocabulary {
            path: "processkit::ParentDeathCleanup",
            class: "configurable",
            projection: Projection::Parsed(|id| {
                ParentDeathCleanup::from_name(id).map(abrupt_cleanup_scope_str)
            }),
            wire_properties: &["abrupt_cleanup"],
            no_wire_reason: None,
        },
        // `limit_hit.limit`, via `run::limit_kind_str` (`_ => "unknown"`).
        Vocabulary {
            path: "processkit::LimitKind",
            class: "configurable",
            projection: Projection::Parsed(|id| LimitKind::from_name(id).map(limit_kind_str)),
            wire_properties: &[],
            no_wire_reason: Some(
                "`limit_hit.limit` is an unconstrained JSON string in the schema (it carries a \
                 pre-spawn failure's kind, described rather than enumerated), so `limit_kind_str` \
                 is this vocabulary's only closed surface",
            ),
        },
        // The three `limit_evidence` axes, via `run::limit_verdict_str` (a
        // pass-through of the upstream identifier, so the closed surface that a new
        // verdict would break is the published enum, not the function).
        Vocabulary {
            path: "processkit::LimitVerdict",
            class: "configurable",
            projection: Projection::Parsed(|id| LimitVerdict::from_name(id).map(limit_verdict_str)),
            wire_properties: &["memory", "processes", "cpu"],
            no_wire_reason: None,
        },
        // `cleanup_finished.shutdown.soft_stop_scope`, via `run::soft_stop_scope_str`
        // (also a pass-through; same reasoning as the verdict above).
        Vocabulary {
            path: "processkit::SoftStopScope",
            class: "configurable",
            projection: Projection::Parsed(|id| {
                SoftStopScope::from_name(id).map(soft_stop_scope_str)
            }),
            wire_properties: &["soft_stop_scope"],
            no_wire_reason: None,
        },
        // `cleanup_finished.shutdown.soft_signal`, via `run::soft_signal_str`
        // (`_ => "failed"`).
        Vocabulary {
            path: "processkit::SoftSignal",
            class: "report_only",
            projection: Projection::Emitted(|| {
                // Every fate this build can name, rendered by the real projection.
                // The payload is immaterial to the identifier (`Sent`/`Failed` carry
                // the attempted signal), so a representative one is enough.
                [
                    SoftSignal::Sent(Signal::Term),
                    SoftSignal::Unsupported,
                    SoftSignal::Failed(Signal::Term),
                ]
                .iter()
                .map(|fate| {
                    let projected = soft_signal_str(fate);
                    assert_eq!(
                        projected,
                        fate.name(),
                        "this crate's `soft_signal` string must be ProcessKit's own identifier \
                         for the fate, or comparing identifier sets below proves nothing",
                    );
                    projected
                })
                .collect()
            }),
            wire_properties: &["soft_signal"],
            no_wire_reason: None,
        },
        // `root_exited.outcome`, via `events::outcome_fields` (`_ => "unknown"`).
        Vocabulary {
            path: "processkit::Outcome",
            class: "report_only",
            projection: Projection::Emitted(|| {
                [
                    Outcome::Exited(0),
                    Outcome::Signalled(Some(9)),
                    Outcome::TimedOut,
                ]
                .iter()
                .map(|outcome| {
                    let (projected, _, _) = outcome_fields(outcome);
                    assert_eq!(
                        projected,
                        outcome.name(),
                        "this crate's `outcome` string must be ProcessKit's own identifier \
                             for the disposition, or comparing identifier sets below proves \
                             nothing",
                    );
                    projected
                })
                .collect()
            }),
            wire_properties: &["outcome"],
            no_wire_reason: None,
        },
    ]
}

/// Dictionary enums this CLI does **not** project, each with the reason. Every enum
/// in the dictionary must appear either here or in [`projected_vocabularies`], so an
/// enum ProcessKit adds later cannot land in neither list and go unconsidered — the
/// same completeness discipline one level up from the identifier check itself.
///
/// "Not projected" means no string derived from the enum reaches a machine contract
/// of this CLI. Using a variant as *input* (this runner sets `StdioMode::Inherit`)
/// does not count: input has no vocabulary to drift.
const NOT_PROJECTED: &[(&str, &str)] = &[
    (
        "processkit::StopReason",
        "supervision API; this runner drives `ProcessGroup::stop` directly and never runs a \
         supervisor, so no stop reason is ever produced or published",
    ),
    (
        "processkit::LimitReason",
        "the reason a cap could not be applied reaches the wire only inside `limit_hit.detail`, \
         which is the backend error's own `Display` text (human-readable, explicitly not a \
         machine vocabulary); the machine-readable half of that event is `limit`, a `LimitKind`",
    ),
    (
        "processkit::StdioMode",
        "input only: `run` selects `Inherit`/`Piped` when building the command, and never reports \
         the choice as a string",
    ),
    (
        "processkit::LineTerminator",
        "line-splitting is a capture-side concern this CLI does not configure or report; it tees \
         raw bytes and never publishes a terminator",
    ),
    (
        "processkit::OverflowMode",
        "input only: the capture ceiling's overflow policy is set on the command; the CLI's own \
         `--capture-overflow` values are its own flag vocabulary, not this enum's",
    ),
    (
        "processkit::OutputStream",
        "the `output_overflow.stream` values (`stdout`/`stderr`) come from this crate's own \
         `CaptureOverflow`, which names the stream that crossed the runner's ceiling; the \
         upstream enum is not in that path",
    ),
    (
        "processkit::Priority",
        "process priority is not a `run` flag and is never set or reported",
    ),
    (
        "processkit::RestartPolicy",
        "supervision API; this runner never restarts a child",
    ),
    (
        "processkit::Signal",
        "the runner's own `cancelled.source` names the signal *it* received (`ctrl_c`/`sigterm`/\
         `sighup`/the Windows console events) from this crate's `CancelSignal`; the signal \
         ProcessKit sends during a soft stop is reported as a fate (`SoftSignal`), not as a \
         signal name",
    ),
    (
        "processkit::RlimitResource",
        "POSIX rlimits are not a `run` flag; resource caps go through `ProcessGroupOptions` and \
         are reported as `LimitKind`",
    ),
    (
        "processkit::ErrorKind",
        "this CLI publishes its own `ErrorKind` vocabulary in the `--error-format json` envelope \
         (src/exit.rs), derived from its reserved exit-code band rather than from the backend's \
         error taxonomy; backend failures are classified by matching `ErrorReason`, and the text \
         they carry is `Display` output, not an identifier",
    ),
    (
        "processkit::ProcessEvent",
        "the streaming per-process event API is unused: this runner reads outcomes and container \
         state directly, and its JSONL vocabulary is its own",
    ),
    (
        "processkit::SupervisionEvent",
        "supervision API; unused, as above",
    ),
];

/// An identifier this CLI knowingly does not represent, with the reason and what
/// would change the answer.
///
/// This is the gate's only "known and accepted" channel, and it is deliberately
/// narrow: it does not silence a *vocabulary*, only one named identifier, and it is
/// checked in both directions. An entry that no longer describes reality — because
/// the identifier is now represented, or has left the dictionary — fails the gate as
/// a stale acknowledgement, so this list cannot quietly become the place where drift
/// goes to be forgotten.
struct AcknowledgedGap {
    path: &'static str,
    identifier: &'static str,
    reason: &'static str,
}

const ACKNOWLEDGED_GAPS: &[AcknowledgedGap] = &[AcknowledgedGap {
    path: "processkit::Outcome",
    identifier: "inactivity_timed_out",
    reason: "this runner never arms ProcessKit's own inactivity deadline \
             (`Command::inactivity_timeout` is called nowhere in `src/`); `--idle-timeout` is a \
             deadline the runner races itself and reports as its own `timeout` event with \
             `reason: \"idle\"`, so the backend cannot produce this disposition and \
             `outcome_fields`'s `_` arm is unreachable rather than wrong. Adding the value to \
             `root_exited.outcome` would widen a published v1 enum for a case that cannot occur, \
             which is a normative schema decision and not this gate's to take. Revisit if the \
             runner ever hands the inactivity window to ProcessKit instead of racing it.",
}];

/// The gate.
#[test]
fn every_shipped_identifier_is_represented_in_a_projection() {
    let (package_version, dictionary_path) = dictionary_path();
    let dictionary = load_dictionary(&package_version, &dictionary_path);
    let schemas = load_schema_documents();

    let vocabularies = projected_vocabularies();
    check_enum_coverage(&dictionary, &vocabularies);

    let mut unrepresented: Vec<(&'static str, String, Vec<String>)> = Vec::new();

    for vocabulary in &vocabularies {
        let entry = dictionary.get(vocabulary.path).unwrap_or_else(|| {
            panic!(
                "`{}` is projected by this CLI but absent from ProcessKit {package_version}'s \
                 {}. Either the enum was renamed or removed upstream (find what replaced it and \
                 update the projection), or this gate's table is stale.",
                vocabulary.path,
                dictionary_path.display(),
            )
        });

        assert_eq!(
            entry.class, vocabulary.class,
            "ProcessKit reclassified `{}` from `{}` to `{}`. The class decides which check is \
             even possible here (a `configurable` value parses back through `from_name`, a \
             `report_only` one does not), so re-read this gate's strategy for it before \
             updating the expectation.",
            vocabulary.path, vocabulary.class, entry.class,
        );

        let emitted = match &vocabulary.projection {
            Projection::Emitted(build) => Some(build()),
            Projection::Parsed(_) => None,
        };

        for (variant, identifier) in &entry.variants {
            let mut missing = Vec::new();

            match (&vocabulary.projection, &emitted) {
                (Projection::Parsed(project), _) => match project(identifier) {
                    None => missing.push(format!(
                        "the linked ProcessKit does not resolve `{identifier}` through \
                         `{}::from_name` at all — the shipped dictionary and the compiled crate \
                         disagree, which should be impossible for a package's own `spec/` file",
                        vocabulary.path,
                    )),
                    Some(projected) if projected != identifier => missing.push(format!(
                        "this crate's Rust projection renders `{}::{variant}` as \
                         `{projected}`, not `{identifier}` — the variant is falling into the \
                         conservative `_` arm (or the spelling drifted)",
                        vocabulary.path,
                    )),
                    Some(_) => {}
                },
                (Projection::Emitted(_), Some(emitted)) => {
                    if !emitted.contains(identifier.as_str()) {
                        missing.push(format!(
                            "this crate's Rust projection never emits `{identifier}`; it emits \
                             only {} — `{}::{variant}` is unnamed in the projection and would \
                             fall into the conservative `_` arm",
                            quoted_list(emitted.iter().copied()),
                            vocabulary.path,
                        ));
                    }
                }
                (Projection::Emitted(_), None) => unreachable!("emitted set is built above"),
            }

            for property in vocabulary.wire_properties {
                for (file, values) in schema_enums(&schemas, property) {
                    if !values.contains(identifier) {
                        missing.push(format!(
                            "`{file}` publishes the `{property}` enum as {}, which does not \
                             include `{identifier}`",
                            quoted_list(values.iter().map(String::as_str)),
                        ));
                    }
                }
            }

            if !missing.is_empty() {
                unrepresented.push((vocabulary.path, identifier.clone(), missing));
            }
        }

        for property in vocabulary.wire_properties {
            assert!(
                !schema_enums(&schemas, property).is_empty(),
                "no schema document declares an `enum` for the `{property}` property, but `{}` \
                 is recorded here as reaching the wire through it. Either the property was \
                 renamed (update this table) or its enum was loosened into an open string \
                 (record that in `no_wire_reason` instead).",
                vocabulary.path,
            );
        }

        if vocabulary.wire_properties.is_empty() {
            assert!(
                vocabulary
                    .no_wire_reason
                    .is_some_and(|reason| !reason.trim().is_empty()),
                "`{}` declares no wire property and no reason for it; a vocabulary with neither \
                 is an unexplained hole in this gate.",
                vocabulary.path,
            );
        }
    }

    let acknowledged = check_acknowledged_gaps(&dictionary, &vocabularies, &unrepresented);

    let unacknowledged: Vec<_> = unrepresented
        .iter()
        .filter(|(path, identifier, _)| !acknowledged.contains(&(*path, identifier.as_str())))
        .collect();

    assert!(
        unacknowledged.is_empty(),
        "ProcessKit {package_version} publishes identifiers this build does not represent.\n\n\
         {}\n\
         This is upstream growth, not a test to relax: decide what each value means for this \
         project's published contract (a new enum member, an existing bucket, or an unreachable \
         case), change the projection and every schema that carries it, then re-run. \
         CONTRIBUTING.md, \"Upstream identifier drift\", walks through it. If a value genuinely \
         cannot reach this CLI, record it in `ACKNOWLEDGED_GAPS` with the reason — never by \
         loosening the check.\n\n\
         Dictionary read from: {}",
        unacknowledged
            .iter()
            .map(|(path, identifier, missing)| {
                let detail = missing
                    .iter()
                    .map(|line| format!("    - {line}\n"))
                    .collect::<String>();
                format!("  {path} identifier `{identifier}`:\n{detail}")
            })
            .collect::<String>(),
        dictionary_path.display(),
    );
}

/// One dictionary enum: its class and its `(variant, identifier)` pairs.
struct DictionaryEnum {
    class: String,
    variants: Vec<(String, String)>,
}

/// Every enum in the dictionary must be classified by this gate, in exactly one of
/// the two tables, and every table entry must still exist upstream.
fn check_enum_coverage(dictionary: &BTreeMap<String, DictionaryEnum>, projected: &[Vocabulary]) {
    let projected_paths: BTreeSet<&str> = projected.iter().map(|v| v.path).collect();
    let unprojected_paths: BTreeSet<&str> = NOT_PROJECTED.iter().map(|(path, _)| *path).collect();

    let overlap: Vec<&str> = projected_paths
        .intersection(&unprojected_paths)
        .copied()
        .collect();
    assert!(
        overlap.is_empty(),
        "these enums are listed as both projected and not projected: {}",
        quoted_list(overlap.into_iter()),
    );

    let unclassified: Vec<&str> = dictionary
        .keys()
        .map(String::as_str)
        .filter(|path| !projected_paths.contains(path) && !unprojected_paths.contains(path))
        .collect();
    assert!(
        unclassified.is_empty(),
        "ProcessKit's dictionary carries enums this gate has never been told about: {}. Decide \
         whether this CLI projects each one: add it to `projected_vocabularies()` with the \
         function and schema enum that publish it, or to `NOT_PROJECTED` with the reason it \
         never reaches a machine contract. Leaving it unlisted is the one outcome this gate does \
         not allow, because it is how a vocabulary gets published without anyone deciding to.",
        quoted_list(unclassified.into_iter()),
    );

    let vanished: Vec<&str> = unprojected_paths
        .iter()
        .copied()
        .filter(|path| !dictionary.contains_key(*path))
        .collect();
    assert!(
        vanished.is_empty(),
        "these enums are recorded in `NOT_PROJECTED` but are no longer in ProcessKit's \
         dictionary: {}. They were renamed or removed upstream; drop the stale rows (and check \
         nothing started depending on them).",
        quoted_list(vanished.into_iter()),
    );
}

/// Check every acknowledgement in both directions and return the set that currently
/// applies. A stale entry is a failure, not a no-op: an acknowledgement that has
/// stopped describing reality is indistinguishable from an unexamined one.
fn check_acknowledged_gaps<'a>(
    dictionary: &BTreeMap<String, DictionaryEnum>,
    projected: &[Vocabulary],
    unrepresented: &'a [(&'static str, String, Vec<String>)],
) -> BTreeSet<(&'a str, &'a str)> {
    let mut applied = BTreeSet::new();

    for gap in ACKNOWLEDGED_GAPS {
        assert!(
            !gap.reason.trim().is_empty(),
            "the acknowledgement for `{}` identifier `{}` carries no reason; an unexplained \
             exemption is exactly the silence this gate exists to break.",
            gap.path,
            gap.identifier,
        );
        assert!(
            projected
                .iter()
                .any(|vocabulary| vocabulary.path == gap.path),
            "`{}` is acknowledged as having an unrepresented identifier, but it is not one of \
             the vocabularies this gate checks — an acknowledgement against an unchecked enum \
             exempts nothing and only reads as if it did.",
            gap.path,
        );

        let entry = dictionary.get(gap.path).unwrap_or_else(|| {
            panic!(
                "`{}` is acknowledged as a gap but is no longer in ProcessKit's dictionary; the \
                 acknowledgement is stale — delete it.",
                gap.path,
            )
        });
        assert!(
            entry
                .variants
                .iter()
                .any(|(_, identifier)| identifier == gap.identifier),
            "`{}` no longer publishes the identifier `{}` this gap acknowledges; the \
             acknowledgement is stale — delete it.",
            gap.path,
            gap.identifier,
        );

        let still_missing = unrepresented
            .iter()
            .find(|(path, identifier, _)| *path == gap.path && identifier == gap.identifier);
        assert!(
            still_missing.is_some(),
            "`{}` identifier `{}` is now fully represented, so its acknowledgement is stale — \
             delete it, and let the gate cover the value for real.",
            gap.path,
            gap.identifier,
        );

        let (path, identifier, _) = still_missing.expect("checked just above");
        applied.insert((*path, identifier.as_str()));
    }

    applied
}

/// The resolved `processkit` version and the path to its shipped dictionary.
///
/// Resolved through `cargo metadata`'s dependency graph — this package's own
/// `processkit` edge — rather than by name across all packages, so a transitive
/// second major version could never be mistaken for the one this crate compiles
/// against.
fn dictionary_path() -> (String, PathBuf) {
    let cargo = std::env::var("CARGO")
        .ok()
        .unwrap_or_else(|| option_env!("CARGO").unwrap_or("cargo").to_string());
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    let output = Command::new(&cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .current_dir(manifest_dir)
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "could not run `{cargo} metadata` to locate the resolved `processkit` package: \
                 {err}. This tier needs a working cargo; see CONTRIBUTING.md, \"Upstream \
                 identifier drift\"."
            )
        });
    assert!(
        output.status.success(),
        "`{cargo} metadata --locked --offline` failed ({}). `--locked` requires Cargo.lock to be \
         current and `--offline` requires the dependency to be already fetched; both hold after \
         an ordinary build of this crate.\n\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let metadata: Value =
        serde_json::from_slice(&output.stdout).expect("`cargo metadata` emits JSON");

    let root = metadata["resolve"]["root"]
        .as_str()
        .expect("`cargo metadata` names the workspace root package")
        .to_string();
    let dependency_id = metadata["resolve"]["nodes"]
        .as_array()
        .expect("`cargo metadata` lists resolve nodes")
        .iter()
        .find(|node| node["id"].as_str() == Some(&root))
        .expect("this package has a resolve node")["deps"]
        .as_array()
        .expect("a resolve node lists its deps")
        .iter()
        .find(|dep| dep["name"].as_str() == Some("processkit"))
        .expect("this package depends on `processkit`")["pkg"]
        .as_str()
        .expect("a resolved dep names its package")
        .to_string();

    let package = metadata["packages"]
        .as_array()
        .expect("`cargo metadata` lists packages")
        .iter()
        .find(|package| package["id"].as_str() == Some(&dependency_id))
        .expect("the resolved dependency is among the packages");

    let version = package["version"]
        .as_str()
        .expect("a package declares its version")
        .to_string();
    let manifest_path = Path::new(
        package["manifest_path"]
            .as_str()
            .expect("a package declares its manifest path"),
    );
    let package_root = manifest_path
        .parent()
        .expect("a manifest always sits in a directory");

    (version, package_root.join("spec").join("identifiers.json"))
}

/// Read and parse the dictionary, failing loudly — never skipping — when it is not
/// there. See decision 2 in this module's header for why a skip is not an option.
fn load_dictionary(version: &str, path: &Path) -> BTreeMap<String, DictionaryEnum> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "the resolved `processkit` {version} ships no readable stable-identifier dictionary \
             at {} ({err}).\n\n\
             This gate refuses to pass here rather than skip: a green result would mean \"no \
             drift found\" when it actually means \"nothing was checked\", which is the exact \
             blindness the tier exists to remove.\n\n\
             If the dependency comes from a patched, vendored, or path source whose tree omits \
             `spec/identifiers.json`, either restore that file in the source you are building \
             against, or do not enable the `spec-drift` feature for that build — the tier is \
             opt-in precisely so declining it is a visible choice. If the dependency is an \
             ordinary registry package, the unpacked source is incomplete: `cargo clean` and \
             rebuild.",
            path.display(),
        )
    });

    let document: Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not valid JSON: {err}", path.display()));

    let schema_version = document["schema_version"].as_u64().unwrap_or_else(|| {
        panic!(
            "{} carries no numeric `schema_version`; this gate's parser was written against \
             version {SUPPORTED_DICTIONARY_SCHEMA_VERSION} of the dictionary format.",
            path.display(),
        )
    });
    assert_eq!(
        schema_version,
        SUPPORTED_DICTIONARY_SCHEMA_VERSION,
        "{} is dictionary format version {schema_version}; this gate reads version \
         {SUPPORTED_DICTIONARY_SCHEMA_VERSION}. Re-read the new format before bumping this \
         constant — a parser that guesses at a reshaped document can report agreement it never \
         checked.",
        path.display(),
    );

    let enums = document["enums"]
        .as_array()
        .unwrap_or_else(|| panic!("{} carries no `enums` array", path.display()));

    let mut parsed = BTreeMap::new();
    for entry in enums {
        let enum_path = entry["path"]
            .as_str()
            .unwrap_or_else(|| panic!("every dictionary enum has a `path` in {}", path.display()))
            .to_string();
        let class = entry["class"]
            .as_str()
            .unwrap_or_else(|| panic!("`{enum_path}` has no `class` in {}", path.display()))
            .to_string();
        let variants = entry["variants"]
            .as_array()
            .unwrap_or_else(|| panic!("`{enum_path}` has no `variants` in {}", path.display()))
            .iter()
            .map(|variant| {
                let name = variant["variant"]
                    .as_str()
                    .unwrap_or_else(|| panic!("a `{enum_path}` variant has no `variant` name"))
                    .to_string();
                let identifier = variant["identifier"]
                    .as_str()
                    .unwrap_or_else(|| panic!("`{enum_path}::{name}` has no `identifier`"))
                    .to_string();
                (name, identifier)
            })
            .collect::<Vec<_>>();
        assert!(
            !variants.is_empty(),
            "`{enum_path}` lists no variants in {}",
            path.display(),
        );
        parsed.insert(enum_path, DictionaryEnum { class, variants });
    }

    assert!(
        !parsed.is_empty(),
        "{} lists no enums at all",
        path.display(),
    );
    parsed
}

/// Every published JSON Schema document, by repo-relative name.
fn load_schema_documents() -> Vec<(String, Value)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut documents = Vec::new();

    let v1 = root.join("fixtures/schema/v1/schema.json");
    documents.push(("fixtures/schema/v1/schema.json".to_string(), read_json(&v1)));

    let cli_dir = root.join("fixtures/schema/cli");
    let mut cli_files: Vec<PathBuf> = std::fs::read_dir(&cli_dir)
        .unwrap_or_else(|err| panic!("could not read {}: {err}", cli_dir.display()))
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".schema.json"))
        })
        .collect();
    cli_files.sort();
    assert!(
        !cli_files.is_empty(),
        "no `*.schema.json` documents under {}; the machine-output schema set cannot be empty",
        cli_dir.display(),
    );

    for file in cli_files {
        let name = format!(
            "fixtures/schema/cli/{}",
            file.file_name()
                .and_then(|name| name.to_str())
                .expect("a schema file has a UTF-8 name"),
        );
        documents.push((name, read_json(&file)));
    }

    documents
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("could not read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not valid JSON: {err}", path.display()))
}

/// Every `enum` array published for `property`, across every schema document, as
/// `(document name, string values)`. A property whose schema carries no `enum` (an
/// open string) contributes nothing, so only genuinely closed surfaces are checked.
fn schema_enums(documents: &[(String, Value)], property: &str) -> Vec<(String, BTreeSet<String>)> {
    let mut found = Vec::new();
    for (name, document) in documents {
        let mut values = Vec::new();
        collect_property_enums(document, property, &mut values);
        for value in values {
            found.push((name.clone(), value));
        }
    }
    found
}

/// Walk a schema document collecting the `enum` string set of every subschema that
/// is the value of a `property`-named key.
fn collect_property_enums(value: &Value, property: &str, found: &mut Vec<BTreeSet<String>>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == property
                    && let Some(values) = child.get("enum").and_then(Value::as_array)
                {
                    found.push(
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                    );
                }
                collect_property_enums(child, property, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_property_enums(item, property, found);
            }
        }
        _ => {}
    }
}

fn quoted_list<'a>(values: impl Iterator<Item = &'a str>) -> String {
    values
        .map(|value| format!("`{value}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
