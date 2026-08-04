//! Human and JSON rendering for control-plane snapshots.
//!
//! Every [`Snapshot`] that reaches this module came out of
//! [`super::SnapshotReply::accept`] — that is the only way to obtain one from the
//! wire — so the payload is a shape this build actually implements. The
//! `snapshot_version` printed here is the value the **runner** declared, which is not
//! always this build's own [`super::SNAPSHOT_VERSION`]: it may be any version down to
//! [`super::MIN_READABLE_SNAPSHOT_VERSION`], and printing it unchanged is the point —
//! it reports which build's contract the answer came from. A version outside that
//! range never gets this far; it is refused with `CONTROL` (103) before rendering (see
//! the parent module's "The snapshot version a runner declares — checked, and acted
//! on"). Nothing here re-checks any of that: a second copy of the policy is exactly
//! the drift the shared acceptance step exists to prevent.

use crate::exit::{self, RunnerError};

use super::{AttestVerdict, Attestation, InspectAllOutcome, InspectAllStatus, Snapshot};

/// Choose `inspect`'s output form: exact JSON when requested, otherwise the
/// bounded terminal-safe human rendering.
pub(super) fn snapshot_output_lines(
    snapshot: &Snapshot,
    json: bool,
) -> Result<Vec<String>, RunnerError> {
    if json {
        let line = serde_json::to_string(snapshot).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not render the inspect snapshot: {err}"),
            )
        })?;
        Ok(vec![line])
    } else {
        Ok(render_snapshot_human(snapshot))
    }
}

/// Render every [`Snapshot`] field plus a member table. Destructuring without
/// `..` makes an additive snapshot field fail compilation until the human view is
/// updated too.
pub(super) fn render_snapshot_human(snapshot: &Snapshot) -> Vec<String> {
    let Snapshot {
        snapshot_version,
        run_id,
        mechanism,
        root_pid,
        started_at,
        jsonl,
        capture_dir,
        members,
    } = snapshot;

    const LABEL_WIDTH: usize = 19;
    let mut lines = vec![
        format!("{:<LABEL_WIDTH$}{snapshot_version}", "snapshot_version:"),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "run_id:",
            crate::text::terminal_safe_bounded(run_id)
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "mechanism:",
            crate::text::terminal_safe_bounded(mechanism)
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "root_pid:",
            root_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string())
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "started_at:",
            crate::text::terminal_safe_bounded(started_at)
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "jsonl:",
            jsonl
                .as_deref()
                .map(crate::text::terminal_safe_bounded)
                .unwrap_or_else(|| "-".to_string())
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "capture_dir:",
            capture_dir
                .as_deref()
                .map(crate::text::terminal_safe_bounded)
                .unwrap_or_else(|| "-".to_string())
        ),
    ];

    if members.is_empty() {
        lines.push(format!("{:<LABEL_WIDTH$}(none)", "members:"));
        return lines;
    }
    lines.push(format!("{:<LABEL_WIDTH$}{}", "members:", members.len()));

    const HEADERS: [&str; 4] = ["PID", "PPID", "NAME", "START_TIME"];
    let cells: Vec<[String; 4]> = members
        .iter()
        .map(|member| {
            [
                member.pid.to_string(),
                member
                    .ppid
                    .map(|ppid| ppid.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                member
                    .name
                    .as_deref()
                    .map(crate::text::terminal_safe_bounded)
                    .unwrap_or_else(|| "-".to_string()),
                member
                    .start_time
                    .as_deref()
                    .map(crate::text::terminal_safe_bounded)
                    .unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    lines.extend(crate::text::aligned_table(HEADERS, &cells, "  ", "  "));
    lines
}

/// Choose `attest`'s output form: exact JSON when requested, otherwise the bounded
/// terminal-safe human rendering.
///
/// Every [`Attestation`] that reaches here came out of
/// [`super::AttestationReply::accept`], so it declares a contract this build
/// implements and describes the run that was addressed. Rendering never decides
/// anything: the verdict printed here and the exit code the caller gets come from the
/// same one field, mapped in [`super::attest_outcome`], so a human reading stdout and
/// a script reading `$?` can never be told different things.
pub(super) fn attestation_output_lines(
    attestation: &Attestation,
    json: bool,
) -> Result<Vec<String>, RunnerError> {
    if json {
        let line = serde_json::to_string(attestation).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not render the attestation: {err}"),
            )
        })?;
        Ok(vec![line])
    } else {
        Ok(render_attestation_human(attestation))
    }
}

/// Render every [`Attestation`] field. Destructuring without `..` makes an additive
/// field fail compilation until the human view is updated too — the same discipline
/// [`render_snapshot_human`] uses.
fn render_attestation_human(attestation: &Attestation) -> Vec<String> {
    let Attestation {
        attestation_version,
        run_id,
        verdict,
        peer_pid,
        mechanism,
        checked_at,
    } = attestation;

    const LABEL_WIDTH: usize = 22;
    vec![
        format!(
            "{:<LABEL_WIDTH$}{attestation_version}",
            "attestation_version:"
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "run_id:",
            crate::text::terminal_safe_bounded(run_id)
        ),
        format!("{:<LABEL_WIDTH$}{}", "verdict:", verdict_str(*verdict)),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "peer_pid:",
            peer_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string())
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "mechanism:",
            crate::text::terminal_safe_bounded(mechanism)
        ),
        format!(
            "{:<LABEL_WIDTH$}{}",
            "checked_at:",
            crate::text::terminal_safe_bounded(checked_at)
        ),
    ]
}

/// The verdict's human spelling, identical to its wire spelling on purpose: an
/// operator reading a terminal and an adapter reading `--json` should be able to
/// quote the same word to each other.
fn verdict_str(verdict: AttestVerdict) -> &'static str {
    match verdict {
        AttestVerdict::Member => "member",
        AttestVerdict::NotAMember => "not_a_member",
        AttestVerdict::PeerIdentityUnsupported => "peer_identity_unsupported",
    }
}

/// Choose `inspect --all`'s output form. JSON is the original single-array wire
/// representation; the default human form gives every target one summary row and
/// expands inspected snapshots through the existing single-run renderer.
pub(super) fn inspect_all_output_lines(
    outcomes: &[InspectAllOutcome],
    json: bool,
) -> Result<Vec<String>, RunnerError> {
    if json {
        let line = serde_json::to_string(outcomes).map_err(|err| {
            RunnerError::new(
                exit::SETUP,
                format!("could not render the inspect --all report: {err}"),
            )
        })?;
        return Ok(vec![line]);
    }
    Ok(render_inspect_all_human(outcomes))
}

fn render_inspect_all_human(outcomes: &[InspectAllOutcome]) -> Vec<String> {
    if outcomes.is_empty() {
        return vec!["no live runs to inspect".to_string()];
    }

    const HEADERS: [&str; 3] = ["RUN_ID", "STATUS", "ERROR"];
    let cells: Vec<[String; 3]> = outcomes
        .iter()
        .map(|outcome| {
            [
                crate::text::terminal_safe_bounded(&outcome.run_id),
                match outcome.status {
                    InspectAllStatus::Inspected => "inspected",
                    InspectAllStatus::AlreadyGone => "already_gone",
                    InspectAllStatus::Failed => "failed",
                }
                .to_string(),
                outcome
                    .error
                    .as_deref()
                    .map(crate::text::terminal_safe_bounded)
                    .unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    let mut lines = crate::text::aligned_table(HEADERS, &cells, "", "  ");

    for outcome in outcomes {
        let Some(snapshot) = &outcome.snapshot else {
            continue;
        };
        lines.push(String::new());
        lines.push(format!(
            "snapshot for {}:",
            crate::text::terminal_safe_bounded(&outcome.run_id)
        ));
        lines.extend(
            render_snapshot_human(snapshot)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    lines
}
