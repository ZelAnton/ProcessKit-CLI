//! Human and JSON rendering for control-plane snapshots.

use crate::exit::{self, RunnerError};

use super::Snapshot;

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
            crate::text::terminal_safe(mechanism)
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
            crate::text::terminal_safe(started_at)
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
                    .map(crate::text::terminal_safe)
                    .unwrap_or_else(|| "-".to_string()),
                member
                    .start_time
                    .as_deref()
                    .map(crate::text::terminal_safe)
                    .unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    lines.extend(crate::text::aligned_table(HEADERS, &cells, "  ", "  "));
    lines
}
