use super::*;
use crate::registry::test_support::{
    scratch_registry as scratch_registry_dir, write_stale_entry, write_unprobeable_entry,
};

/// A snapshot round-trips through JSON: the client parses exactly what the server
/// serialized, members included — both a fully enriched member and a bare-PID one
/// (a platform gap or a vanished-mid-read member), the same two shapes the JSONL
/// `members_snapshot` fixture pins. This is the wire contract `inspect` depends on.
#[test]
fn snapshot_round_trips_through_json() {
    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        run_id: "run-42".to_string(),
        mechanism: "job_object".to_string(),
        root_pid: Some(4242),
        started_at: "2026-07-21T00:00:00.000Z".to_string(),
        jsonl: Some("C:\\runs\\run-42.jsonl".to_string()),
        capture_dir: Some("C:\\runs\\run-42".to_string()),
        members: vec![
            Member {
                pid: 4242,
                ppid: Some(4200),
                name: Some("worker.exe".to_string()),
                start_time: Some("133456789000000000".to_string()),
            },
            Member::from_pid(4243),
        ],
    };
    let line = serialize_snapshot(&snapshot);
    let parsed: Snapshot = serde_json::from_str(&line).expect("a snapshot line parses back");
    assert_eq!(parsed.snapshot_version, SNAPSHOT_VERSION);
    assert_eq!(parsed.run_id, "run-42");
    assert_eq!(parsed.mechanism, "job_object");
    assert_eq!(parsed.root_pid, Some(4242));
    assert_eq!(parsed.jsonl.as_deref(), Some("C:\\runs\\run-42.jsonl"));
    assert_eq!(parsed.capture_dir.as_deref(), Some("C:\\runs\\run-42"));
    assert_eq!(parsed.members.len(), 2);
    assert_eq!(parsed.members[0].pid, 4242);
    assert_eq!(parsed.members[0].ppid, Some(4200));
    assert_eq!(parsed.members[0].name.as_deref(), Some("worker.exe"));
    assert_eq!(
        parsed.members[0].start_time.as_deref(),
        Some("133456789000000000")
    );
    // A member built from a bare pid still round-trips with the enriched fields
    // explicitly null, not omitted.
    assert_eq!(parsed.members[1].pid, 4243);
    assert!(parsed.members[1].ppid.is_none());
}

/// T-214 regression: `inspect --json`'s wire/stdout shape is pinned byte-for-byte
/// against a fixed snapshot. This goes through [`snapshot_output_lines`] — the
/// same function `inspect_async`'s `--json` branch calls — not a reimplementation,
/// so a change to the client's actual print path (not just `Snapshot`'s serde
/// shape, which [`serialize_snapshot`]'s own round-trip test above separately
/// covers) fails this test too. A field added/renamed/reordered, or a whitespace
/// change, fails this test.
#[test]
fn json_output_is_byte_for_byte_pinned() {
    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        run_id: "build-42".to_string(),
        mechanism: "job_object".to_string(),
        root_pid: Some(4242),
        started_at: "2026-07-20T21:00:00.000Z".to_string(),
        jsonl: Some("/runs/build-42.jsonl".to_string()),
        capture_dir: None,
        members: vec![Member {
            pid: 4242,
            ppid: Some(4200),
            name: Some("build.exe".to_string()),
            start_time: Some("133456789000000000".to_string()),
        }],
    };
    let lines = snapshot_output_lines(&snapshot, true).expect("a plain Snapshot always serializes");
    assert_eq!(
        lines,
        vec![format!(
            "{{\"snapshot_version\":{SNAPSHOT_VERSION},\"run_id\":\"build-42\",\"mechanism\":\
                 \"job_object\",\"root_pid\":4242,\"started_at\":\"2026-07-20T21:00:00.000Z\",\
                 \"jsonl\":\"/runs/build-42.jsonl\",\"capture_dir\":null,\"members\":[{{\"pid\":4242,\
                 \"ppid\":4200,\"name\":\"build.exe\",\"start_time\":\
                 \"133456789000000000\"}}]}}"
        )],
        "inspect --json's output shape must stay byte-for-byte unchanged"
    );
}

/// The other half of the `if json` branch [`json_output_is_byte_for_byte_pinned`]
/// covers: with `json = false`, [`snapshot_output_lines`] must return the
/// human-readable rendering, not the JSON line — so a mutation flipping the
/// condition (or collapsing it to a constant) is caught here, not just by a
/// difference in the two tests' expected literals.
#[test]
fn snapshot_output_lines_without_json_is_the_human_rendering() {
    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        run_id: "run-1".to_string(),
        mechanism: "process_group".to_string(),
        root_pid: None,
        started_at: "2026-07-20T21:00:00.000Z".to_string(),
        jsonl: None,
        capture_dir: None,
        members: vec![],
    };
    let lines = snapshot_output_lines(&snapshot, false)
        .expect("a plain Snapshot always renders human-readably");
    assert_eq!(
        lines,
        render_snapshot_human(&snapshot),
        "json = false must take the human-readable path, not the JSON one"
    );
    assert!(
        !lines[0].starts_with('{'),
        "the human-readable form must not look like the JSON line: {lines:?}"
    );
}

/// The human-readable rendering (`inspect` without `--json`) shows every field a
/// JSON snapshot carries — including `snapshot_version`, not just the five
/// operator-facing ones — plus the member table, column-aligned the same way
/// `list`'s table is. This test asserts the exact line set, so it would fail (not
/// just look incomplete) if a field silently dropped out of the rendering again.
#[test]
fn render_snapshot_human_shows_every_snapshot_field() {
    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        run_id: "build-42".to_string(),
        mechanism: "job_object".to_string(),
        root_pid: Some(4242),
        started_at: "2026-07-20T21:00:00.000Z".to_string(),
        jsonl: Some("/runs/build-42.jsonl".to_string()),
        capture_dir: Some("/runs/build-42".to_string()),
        members: vec![
            Member {
                pid: 4242,
                ppid: Some(4200),
                name: Some("build.exe".to_string()),
                start_time: Some("133456789000000000".to_string()),
            },
            Member::from_pid(99),
        ],
    };
    let lines = render_snapshot_human(&snapshot);
    assert_eq!(
        lines,
        vec![
            "snapshot_version:  2".to_string(),
            "run_id:            build-42".to_string(),
            "mechanism:         job_object".to_string(),
            "root_pid:          4242".to_string(),
            "started_at:        2026-07-20T21:00:00.000Z".to_string(),
            "jsonl:             /runs/build-42.jsonl".to_string(),
            "capture_dir:       /runs/build-42".to_string(),
            "members:           2".to_string(),
            "  PID   PPID  NAME       START_TIME".to_string(),
            "  4242  4200  build.exe  133456789000000000".to_string(),
            "  99    -     -          -".to_string(),
        ]
    );
}

/// A `null` `root_pid` and an empty `members` list — both real, documented
/// snapshot shapes (a backend that exposed no root pid; a container queried
/// before any member exists) — render as an explicit placeholder, never a blank
/// or missing line.
#[test]
fn render_snapshot_human_handles_absent_root_pid_and_no_members() {
    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        run_id: "run-1".to_string(),
        mechanism: "process_group".to_string(),
        root_pid: None,
        started_at: "2026-07-20T21:00:00.000Z".to_string(),
        jsonl: None,
        capture_dir: None,
        members: vec![],
    };
    let lines = render_snapshot_human(&snapshot);
    assert_eq!(
        lines,
        vec![
            "snapshot_version:  2".to_string(),
            "run_id:            run-1".to_string(),
            "mechanism:         process_group".to_string(),
            "root_pid:          -".to_string(),
            "started_at:        2026-07-20T21:00:00.000Z".to_string(),
            "jsonl:             -".to_string(),
            "capture_dir:       -".to_string(),
            "members:           (none)".to_string(),
        ]
    );
}

/// Human-readable inspect output is a terminal boundary. Snapshot strings can
/// originate in the registry, on the wire, or in OS process metadata, so control
/// bytes must be collapsed before either the key/value block or member table is
/// aligned. The JSON branch is separately pinned byte-for-byte above.
#[test]
fn render_snapshot_human_sanitizes_untrusted_strings() {
    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        run_id: "run\nnext\u{1b}[31m".to_string(),
        mechanism: "job\tobject".to_string(),
        root_pid: Some(7),
        started_at: "time\rrewound".to_string(),
        jsonl: Some("artifact\npath".to_string()),
        capture_dir: None,
        members: vec![Member {
            pid: 7,
            ppid: None,
            name: Some("worker\nname\u{7}".to_string()),
            start_time: Some("start\ttime".to_string()),
        }],
    };

    let lines = render_snapshot_human(&snapshot);
    assert!(
        lines
            .iter()
            .all(|line| line.chars().all(|character| !character.is_control())),
        "no terminal control character survives inspect rendering: {lines:?}"
    );
    assert!(lines.iter().any(|line| line.contains("run next [31m")));
    assert!(lines.iter().any(|line| line.contains("worker name ")));
    assert!(lines.iter().any(|line| line.contains("start time")));
}

#[test]
fn render_snapshot_human_bounds_an_untrusted_run_id() {
    let raw_run_id = "r".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS + 20);
    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        run_id: raw_run_id.clone(),
        mechanism: "job_object".to_string(),
        root_pid: None,
        started_at: "2026-07-20T21:00:00.000Z".to_string(),
        jsonl: None,
        capture_dir: None,
        members: vec![],
    };

    let human = render_snapshot_human(&snapshot);
    assert_eq!(
        human[1],
        format!(
            "{:<19}{}...",
            "run_id:",
            "r".repeat(crate::text::TERMINAL_FIELD_MAX_CHARS)
        )
    );
    let json = snapshot_output_lines(&snapshot, true).expect("serialize JSON snapshot");
    assert!(
        json[0].contains(&raw_run_id),
        "JSON preserves identity data"
    );
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
    let source = SnapshotSource::new(
        "run-x",
        "process_group",
        Some(7),
        started,
        "/runs/run-x.jsonl",
        Some("/runs/run-x"),
        &members,
    );

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

/// Both mutation paths reject every independently malformed dimension of an
/// acknowledgement, including a valid-looking reply from a different run.
#[test]
fn ack_validation_requires_acceptance_action_and_run_id() {
    let accepted = ControlAck {
        accepted: true,
        action: "cancel".to_string(),
        run_id: "run-a".to_string(),
    };
    assert!(ack_matches(&accepted, "cancel", "run-a"));

    let rejected = ControlAck {
        accepted: false,
        action: "cancel".to_string(),
        run_id: "run-a".to_string(),
    };
    assert!(!ack_matches(&rejected, "cancel", "run-a"));
    assert!(!ack_matches(&accepted, "kill", "run-a"));
    assert!(!ack_matches(&accepted, "cancel", "run-b"));
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
        "/runs/run-t.jsonl",
        None,
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

/// (T-173) A request line over [`MAX_LINE_BYTES`] with no terminating `\n` — a
/// broken or hostile owner-local client — must not make `serve_one` buffer it
/// without bound. `read_bounded_line`'s `take`-based cap returns deterministically
/// once the ceiling is exhausted (it never needs the peer to send more or close
/// the connection), so this test proves both halves of the requirement in one
/// shot: the exchange completes at all (a hang would make this test itself time
/// out under the harness) and it completes with a structured error, the same
/// closing path an unrecognized verb gets — never a silent truncate-and-continue.
#[tokio::test]
async fn server_rejects_an_oversized_request_line_without_unbounded_buffering() {
    let (mut client, server) = tokio::io::duplex(MAX_LINE_BYTES + 4096);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let members = || vec![Member::from_pid(1)];
    let source = SnapshotSource::new(
        "run-t",
        "job_object",
        Some(1),
        SystemTime::UNIX_EPOCH,
        "/runs/run-t.jsonl",
        None,
        &members,
    );

    // One byte past the ceiling, no trailing `\n` at all — exactly the shape that
    // made `read_line` buffer without bound before this fix.
    let oversized = vec![b'x'; MAX_LINE_BYTES + 1];
    client
        .write_all(&oversized)
        .await
        .expect("write past the byte ceiling into the duplex buffer");

    serve_one(server, &source, &tx).await.expect(
        "an oversized request still closes normally through the structured-error path, \
             not a hard connection error",
    );

    let mut reader = BufReader::new(&mut client);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .expect("read the structured error response");
    let value: serde_json::Value =
        serde_json::from_str(line.trim()).expect("a structured JSON error, not a hang or a panic");
    assert!(
        value.get("error").and_then(|v| v.as_str()).is_some(),
        "an oversized request yields an error object: {line}"
    );
    assert!(
        rx.try_recv().is_err(),
        "an oversized request must never route a teardown command"
    );
}

#[tokio::test]
async fn bounded_line_distinguishes_mid_line_eof_from_limit_exhaustion() {
    let mut short_reader = BufReader::new(&b"inspect"[..]);
    let mut short_line = String::new();
    let short_err = read_bounded_line(&mut short_reader, &mut short_line)
        .await
        .expect_err("a partial line at EOF is invalid");
    assert_eq!(short_err.kind(), io::ErrorKind::InvalidData);
    assert!(
        short_err.to_string().contains("peer closed"),
        "short EOF must describe the actual failure: {short_err}"
    );
    assert!(
        !short_err.to_string().contains("exceeded"),
        "short EOF must not claim the byte limit was exhausted: {short_err}"
    );

    let at_limit = vec![b'x'; MAX_LINE_BYTES];
    let mut limit_reader = BufReader::new(at_limit.as_slice());
    let mut limit_line = String::new();
    let limit_err = read_bounded_line(&mut limit_reader, &mut limit_line)
        .await
        .expect_err("an unterminated line at the cap is invalid");
    assert_eq!(limit_err.kind(), io::ErrorKind::InvalidData);
    assert!(
        limit_err.to_string().contains("exceeded"),
        "cap exhaustion keeps the bounded-read diagnostic: {limit_err}"
    );
}

/// (T-173) `converse`'s reply read is bounded by the same [`MAX_LINE_BYTES`]
/// ceiling: a reply over the cap with no terminating `\n` (a wedged or
/// misbehaving runner) surfaces as the same `InvalidData` `io::Error` shape an
/// unparsable reply already does, never an unbounded read on the client side.
#[tokio::test]
async fn converse_rejects_an_oversized_response_line_without_unbounded_buffering() {
    let (client_stream, mut server_stream) = tokio::io::duplex(MAX_LINE_BYTES + 4096);

    let oversized = vec![b'y'; MAX_LINE_BYTES + 1];
    server_stream
        .write_all(&oversized)
        .await
        .expect("write past the byte ceiling into the duplex buffer");

    let err = converse::<_, Snapshot>(client_stream, INSPECT_REQUEST)
        .await
        .expect_err("an oversized response line must be a hard error, not an unbounded read");
    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "matches the existing unparsable-reply error kind: {err}"
    );
}

/// (T-191) A structured `{"error": "..."}` reply — what `serve_one` answers an
/// unrecognized verb or an oversized request line with — does not parse as
/// `Snapshot`/`ControlAck`, but `converse` must recognize it as `ErrorReply` and
/// surface its `error` text verbatim, not the generic "unreadable response"
/// `serde` field-mismatch message.
#[tokio::test]
async fn converse_surfaces_a_structured_error_response_verbatim() {
    let (client_stream, mut server_stream) = tokio::io::duplex(MAX_LINE_BYTES + 4096);

    let error_line = serialize_error("control request rejected: line exceeded the byte limit");
    server_stream
        .write_all(error_line.as_bytes())
        .await
        .expect("write the structured error response into the duplex buffer");
    server_stream
        .write_all(b"\n")
        .await
        .expect("terminate the response line");

    let err = converse::<_, Snapshot>(client_stream, INSPECT_REQUEST)
        .await
        .expect_err("a structured error response is still an error, not a Snapshot");
    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "matches the existing unparsable-reply error kind: {err}"
    );
    let message = err.to_string();
    assert!(
        message.contains("control request rejected: line exceeded the byte limit"),
        "the server's own error text is raised verbatim: {message}"
    );
    assert!(
        !message.contains("unreadable response"),
        "a recognized structured error is distinguishable from real garbage: {message}"
    );
}

/// (T-191 review R-01) A server error message containing characters JSON must
/// escape — a quote, backslashes (as in a Windows named-pipe path), an embedded
/// newline — used to make the previous borrowed-`&str` fallback silently fail to
/// parse: `serde_json` can only borrow a JSON string field when deserializing it
/// needs no unescaping, so any real, uncontrolled peer diagnostic containing such
/// a character fell all the way through to the generic "unreadable response"
/// message the fallback exists to avoid. `converse` must surface this text just
/// as reliably as the escape-free case above (built through the real
/// `serialize_error`, so this exercises an actual server round-trip rather than a
/// hand-picked "safe" literal), and the embedded newline must not survive into
/// the one-line CLI message verbatim.
#[tokio::test]
async fn converse_surfaces_a_structured_error_response_with_escaped_text() {
    let (client_stream, mut server_stream) = tokio::io::duplex(MAX_LINE_BYTES + 4096);

    let raw = "unknown control request `say \"hi\"` at path \\\\.\\pipe\\processkit-x\nline two";
    let error_line = serialize_error(raw);
    server_stream
        .write_all(error_line.as_bytes())
        .await
        .expect("write the structured error response into the duplex buffer");
    server_stream
        .write_all(b"\n")
        .await
        .expect("terminate the response line");

    let err = converse::<_, Snapshot>(client_stream, INSPECT_REQUEST)
        .await
        .expect_err("a structured error response is still an error, not a Snapshot");
    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "matches the existing unparsable-reply error kind: {err}"
    );
    let message = err.to_string();
    assert!(
        !message.contains("unreadable response"),
        "escaped server text must still be recognized as an ErrorReply, not garbage: {message}"
    );
    assert!(
        message.contains("unknown control request"),
        "the server's own diagnostic is still raised: {message}"
    );
    assert!(
        message.contains("processkit-x"),
        "the pipe path survives escaping and normalization: {message}"
    );
    assert!(
        !message.contains('\n'),
        "the embedded newline is collapsed so a peer cannot reformat CLI output: {message}"
    );
}

/// (T-191 review R-01) `normalize_peer_error_text` collapses control characters
/// (so a peer cannot inject newlines into the CLI's one-line diagnostic) and caps
/// length (so a maximally-sized `MAX_LINE_BYTES` error text does not dominate the
/// CLI's output).
#[test]
fn normalize_peer_error_text_collapses_control_characters_and_caps_length() {
    let normalized = normalize_peer_error_text("line one\nline two\ttabbed");
    assert_eq!(normalized, "line one line two tabbed");

    let long = "x".repeat(MAX_PEER_ERROR_CHARS + 50);
    let normalized_long = normalize_peer_error_text(&long);
    assert!(
        normalized_long.ends_with("... (truncated)"),
        "an oversized peer error text is truncated: {normalized_long}"
    );
    assert!(
        normalized_long.len() < long.len(),
        "the truncated text is strictly shorter than the input: {normalized_long}"
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

#[tokio::test]
async fn connect_live_rejects_an_invalid_registry_endpoint_before_io() {
    let err = match connect_live("not-a-platform-endpoint", "kill", "run-9").await {
        Ok(_) => panic!("an invalid registry endpoint must not be opened"),
        Err(err) => err,
    };
    assert_eq!(err.code(), exit::CONTROL);
    assert!(err.to_string().contains("invalid control endpoint"));
}

#[cfg(unix)]
#[test]
fn unix_control_endpoint_validation_accepts_any_base_but_rejects_unsafe_shapes() {
    assert!(endpoint_is_valid("/different/tmp/pkc-123-abc/c.sock"));
    for endpoint in [
        "relative/pkc-123/c.sock",
        "/tmp/../pkc-123/c.sock",
        "/tmp//pkc-123/c.sock",
        "/tmp/pkc-/c.sock",
        "/tmp/pkc-123/not-the-socket",
        "/tmp/pkc-bad_token/c.sock",
    ] {
        assert!(!endpoint_is_valid(endpoint), "must reject {endpoint:?}");
    }
}

#[cfg(windows)]
#[test]
fn windows_control_endpoint_validation_requires_the_owned_pipe_shape() {
    assert!(endpoint_is_valid(r"\\.\pipe\processkit-cli-1234-abc-0"));
    for endpoint in [
        r"\\.\pipe\other-1234",
        r"\\server\pipe\processkit-cli-1234",
        r"\\.\pipe\processkit-cli-",
        r"\\.\pipe\processkit-cli-bad\suffix",
    ] {
        assert!(!endpoint_is_valid(endpoint), "must reject {endpoint:?}");
    }
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
        .register_plain("dup-race", Some("endpoint-a"), SystemTime::now())
        .expect("register the first run");

    let endpoint = resolve_in_registry(&registry, "kill", "dup-race")
        .expect("the sole live run resolves before the race window opens");
    assert_eq!(endpoint, "endpoint-a");

    // The race: a second run registers under the same run_id in the window
    // between the client's initial resolve and its dispatch.
    let second = registry
        .register_plain("dup-race", Some("endpoint-b"), SystemTime::now())
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
        .register_plain("dup-endpointless", Some("endpoint-a"), SystemTime::now())
        .expect("register the run that has an endpoint");
    let without_endpoint = registry
        .register_plain("dup-endpointless", None, SystemTime::now())
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

/// (F-01) The control clients refuse on anything that is not a confirmed-live
/// match — but *what they say* must not outrun what the registry established.
/// A confirmed-stale entry is reported as a gone runner; an entry whose liveness
/// probe could not run at all must not be, because that is exactly the
/// unconfirmed positive claim `Health::Unprobed` (T-206) exists to stop
/// `list`/`prune`/`wait` from making about the same record — and an operator
/// cross-checking a "the runner is gone" refusal with `list` (as
/// `docs/troubleshooting.md` prescribes) would be shown `unprobed` there.
/// Both wordings are still the same reserved `CONTROL` (103) refusal.
#[test]
fn resolve_in_registry_does_not_call_an_unprobeable_entry_a_gone_runner() {
    let dir = scratch_registry_dir("unprobed-vs-stale");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

    write_stale_entry(&dir, "gone", "gone-run");
    write_unprobeable_entry(&dir, "opaque", "opaque-run");

    // The confirmed-dead case is unchanged: the probe ran and found no holder, so
    // naming the runner gone is a fact the registry actually established.
    let stale = resolve_in_registry(&registry, "cancel", "gone-run")
        .expect_err("a stale entry is not a live target");
    assert_eq!(stale.code(), exit::CONTROL);
    let stale_message = stale.to_string();
    assert!(
        stale_message.contains("stale") && stale_message.contains("the runner is gone"),
        "a confirmed-stale entry still reports the runner as gone: {stale_message}"
    );

    // The unconfirmed case: same refusal, same code — different claim.
    for action in ["inspect", "cancel", "kill"] {
        let err = resolve_in_registry(&registry, action, "opaque-run")
            .expect_err("an unprobeable entry is not a live target either");
        assert_eq!(err.code(), exit::CONTROL);
        let message = err.to_string();
        assert!(
            message.contains(action) && message.contains("opaque-run"),
            "still names the action and the run: {message}"
        );
        assert!(
            message.contains("unprobed"),
            "carries the same vocabulary `list`/`prune`/`wait` use for this case: {message}"
        );
        assert!(
            !message.contains("the runner is gone") && !message.contains("is stale"),
            "must not assert a death the probe never established: {message}"
        );
    }

    // The mutating verbs' pre-dispatch re-check drives the same resolver, so it
    // cannot reintroduce the claim on its own path either.
    let reconfirm = reconfirm_target(&registry, "kill", "opaque-run", "endpoint-whatever")
        .expect_err("the re-check refuses an unprobeable target too");
    assert_eq!(reconfirm.code(), exit::CONTROL);
    assert!(
        !reconfirm.to_string().contains("the runner is gone"),
        "the pre-dispatch re-check words it the same way: {reconfirm}"
    );

    // One unprobeable record is enough to withhold the stronger claim, even
    // beside a confirmed-stale sibling under the *same* run id — the same
    // precedence `Registry::probe_run` gives `Unprobed` over `Finished`.
    write_stale_entry(&dir, "mixed-gone", "mixed-run");
    write_unprobeable_entry(&dir, "mixed-opaque", "mixed-run");
    let mixed = resolve_in_registry(&registry, "kill", "mixed-run")
        .expect_err("neither matching record is live");
    let mixed_message = mixed.to_string();
    assert!(
        mixed_message.contains("unprobed") && !mixed_message.contains("the runner is gone"),
        "an unprobeable record is not outvoted by a confirmed-stale sibling: {mixed_message}"
    );

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
        .register_plain("solo-run", Some("endpoint-solo"), SystemTime::now())
        .expect("register the run");

    let endpoint =
        resolve_in_registry(&registry, "cancel", "solo-run").expect("the sole live run resolves");

    reconfirm_target(&registry, "cancel", "solo-run", &endpoint)
        .expect("no racing registration occurred, so the re-check passes");

    drop(first);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `reconfirm_target` closes the window between the client's initial resolve and
/// its dispatch, but re-review (see `docs/registry.md`, "Run id resolution") kept
/// finding a further residual gap: `reconfirm_target` is a synchronous scan, while
/// the verb itself goes out through a later `.await` on the write half
/// (`converse`), so a duplicate could in principle register in between.
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
    // (`converse`), the other by run A's server loop (`serve_one`).
    let (client_stream, server_stream) = tokio::io::duplex(1024);

    let first = registry
        .register_plain("dup-post-race", Some("endpoint-a"), SystemTime::now())
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
        .register_plain("dup-post-race", Some("endpoint-b"), SystemTime::now())
        .expect("register the racing duplicate after the re-check passed");

    let members = || vec![Member::from_pid(1)];
    let source = SnapshotSource::new(
        "dup-post-race",
        "job_object",
        Some(1),
        SystemTime::UNIX_EPOCH,
        "/runs/dup-post-race.jsonl",
        None,
        &members,
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // Drive both sides of the already-open connection concurrently, exactly as
    // `mutate_async` does after `reconfirm_target` returns.
    let (serve_result, ack) = tokio::join!(
        serve_one(server_stream, &source, &tx),
        converse::<_, ControlAck>(client_stream, ControlCommand::Cancel.verb()),
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

/// The Unix socket address stays short and owner-only regardless of any
/// external path: macOS allows only a short `sun_path`, so `bind` uses its own
/// short-lived directory rather than a caller-supplied one.
#[cfg(unix)]
#[tokio::test]
async fn unix_socket_path_stays_short_and_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let server = imp::ControlServer::bind()
        .expect("binding the control socket does not depend on external paths");
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

/// [`snapshot_mutation_targets`]'s projection, proved directly rather
/// than only through `cancel --all`/`kill --all`'s end-to-end behavior — the same
/// three-way fixture [`crate::wait::tests::snapshot_target_paths_include_only_confirmed_live_entries`]
/// drives for the aggregate `wait --all` barrier: a confirmed-`Health::Live`
/// entry is in scope, while a confirmed-`Health::Stale` entry and one that is
/// only `Health::Unprobed` are both excluded outright — `--all` acts only on
/// confirmed-live entries, never a wider or narrower bar than the single-run
/// form's own [`resolve_in_registry`] applies.
#[test]
fn snapshot_mutation_targets_include_only_confirmed_live_entries() {
    let dir = scratch_registry_dir("snapshot-live");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

    let live = registry
        .register_plain("live-run", Some("endpoint-live"), SystemTime::now())
        .expect("register a live run");
    write_stale_entry(&dir, "stale-stem", "stale-run");
    write_unprobeable_entry(&dir, "unprobed-stem", "unprobed-run");

    let targets = snapshot_mutation_targets(&registry, &[]).expect("scan the fixture registry");

    assert_eq!(
        targets
            .iter()
            .map(|target| target.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["live-run"],
        "only the confirmed-live entry is in the snapshot: {targets:?}"
    );

    drop(live);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (T-217) A live entry that has not (yet, or ever) published a control endpoint
/// is still in the `--all` snapshot — endpoint presence is not part of the
/// snapshot's identity/health bar, mirroring `resolve_in_registry`'s own "count
/// live entries before ever looking at endpoints" discipline (K-016). Its
/// eventual per-target mutation fails on its own (no control endpoint), reported
/// in the outcome list, rather than the entry being silently excluded from the
/// snapshot up front.
#[test]
fn snapshot_mutation_targets_include_a_live_entry_with_no_endpoint() {
    let dir = scratch_registry_dir("snapshot-live-no-endpoint");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");

    let live = registry
        .register_plain("endpointless-run", None, SystemTime::now())
        .expect("register a live run that never published an endpoint");

    let targets = snapshot_mutation_targets(&registry, &[]).expect("scan the fixture registry");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].run_id, "endpointless-run");
    assert_eq!(targets[0].endpoint, None);

    drop(live);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (T-217) The aggregate `--all` report's JSON shape: a successful outcome omits
/// `error` entirely (not `null`), and a failed one carries it — the per-run
/// detail an aggregate exit code alone cannot convey.
#[test]
fn control_all_outcome_serializes_error_only_on_failure() {
    let ok = ControlAllOutcome {
        run_id: "run-a".to_string(),
        accepted: true,
        status: ControlAllStatus::Accepted,
        error: None,
    };
    let ok_json = serde_json::to_string(&ok).expect("a successful outcome serializes");
    let ok_value: serde_json::Value = serde_json::from_str(&ok_json).expect("valid JSON");
    assert_eq!(ok_value["run_id"], "run-a");
    assert_eq!(ok_value["accepted"], true);
    assert_eq!(ok_value["status"], "accepted");
    assert!(
        ok_value.get("error").is_none(),
        "a successful outcome omits `error` entirely: {ok_json}"
    );

    let failed = ControlAllOutcome {
        run_id: "run-b".to_string(),
        accepted: false,
        status: ControlAllStatus::Failed,
        error: Some("cannot kill run `run-b`: its registry entry is stale".to_string()),
    };
    let failed_json = serde_json::to_string(&failed).expect("a failed outcome serializes");
    let failed_value: serde_json::Value = serde_json::from_str(&failed_json).expect("valid JSON");
    assert_eq!(failed_value["run_id"], "run-b");
    assert_eq!(failed_value["accepted"], false);
    assert_eq!(failed_value["status"], "failed");
    assert_eq!(
        failed_value["error"],
        "cannot kill run `run-b`: its registry entry is stale"
    );

    let already_gone = ControlAllOutcome {
        run_id: "run-c".to_string(),
        accepted: false,
        status: ControlAllStatus::AlreadyGone,
        error: None,
    };
    let gone_json =
        serde_json::to_string(&already_gone).expect("an already-gone outcome serializes");
    let gone_value: serde_json::Value = serde_json::from_str(&gone_json).expect("valid JSON");
    assert_eq!(gone_value["accepted"], false);
    assert_eq!(gone_value["status"], "already_gone");
    assert!(gone_value.get("error").is_none());
}

/// (T-217) A whole array of [`ControlAllOutcome`]s — the exact shape
/// `cancel --all` / `kill --all` print — round-trips through JSON as a list, the
/// aggregate counterpart to the single-run form's bare [`ControlAck`] object.
#[test]
fn control_all_outcome_list_serializes_as_a_json_array() {
    let outcomes = vec![
        ControlAllOutcome {
            run_id: "run-a".to_string(),
            accepted: true,
            status: ControlAllStatus::Accepted,
            error: None,
        },
        ControlAllOutcome {
            run_id: "run-b".to_string(),
            accepted: false,
            status: ControlAllStatus::Failed,
            error: Some("cannot cancel run `run-b`: no run with that id is registered".to_string()),
        },
    ];
    let line = serde_json::to_string(&outcomes).expect("the outcome list serializes");
    let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
    let array = value.as_array().expect("the report is a JSON array");
    assert_eq!(array.len(), 2);
    assert_eq!(array[0]["run_id"], "run-a");
    assert_eq!(array[0]["accepted"], true);
    assert_eq!(array[1]["run_id"], "run-b");
    assert_eq!(array[1]["accepted"], false);
    assert!(array[1]["error"].is_string());
}

#[tokio::test]
async fn aggregate_target_that_finishes_before_dispatch_is_already_gone() {
    let dir = scratch_registry_dir("aggregate-target-gone");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("short-run", Some("endpoint-now-gone"), SystemTime::now())
        .expect("register the live target");
    let mut targets = snapshot_mutation_targets(&registry, &[]).expect("snapshot live targets");
    let target = targets.pop().expect("the target is in the snapshot");

    drop(registration);

    let outcome = mutate_snapshot_target(&registry, &target, ControlCommand::Kill)
        .await
        .expect("an already-finished target is a successful aggregate outcome");
    assert!(matches!(outcome, SnapshotMutation::AlreadyGone));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn endpointless_target_that_finishes_before_dispatch_is_already_gone() {
    let dir = scratch_registry_dir("aggregate-endpointless-gone");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("short-run", None, SystemTime::now())
        .expect("register the live target without an endpoint");
    let mut targets = snapshot_mutation_targets(&registry, &[]).expect("snapshot live targets");
    let target = targets.pop().expect("the target is in the snapshot");

    drop(registration);

    let outcome = mutate_snapshot_target(&registry, &target, ControlCommand::Kill)
        .await
        .expect("an already-finished endpointless target is successful");
    assert!(matches!(outcome, SnapshotMutation::AlreadyGone));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unprobeable_snapshot_record_remains_a_hard_failure() {
    let dir = scratch_registry_dir("aggregate-unprobed-target");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");
    write_unprobeable_entry(&dir, "opaque", "target");
    let entry = registry
        .entries()
        .expect("scan registry")
        .pop()
        .expect("unprobeable entry remains visible");
    let target = MutationTarget {
        run_id: entry.record.run_id,
        record_path: entry.path,
        endpoint: entry.record.endpoint,
    };

    let err = snapshot_target_state(&registry, &target, "kill")
        .expect_err("unknown liveness is not proof the target ended");
    assert_eq!(err.code(), exit::CONTROL);
    assert!(err.to_string().contains("could not be re-probed"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_snapshot_record_with_changed_identity_remains_a_hard_failure() {
    let dir = scratch_registry_dir("aggregate-changed-target");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("target", Some("endpoint-a"), SystemTime::now())
        .expect("register live target");
    let mut targets = snapshot_mutation_targets(&registry, &[]).expect("snapshot live targets");
    let target = targets.pop().expect("target is in the snapshot");

    let mut replacement: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&target.record_path).expect("read target record"),
    )
    .expect("parse target record");
    replacement["run_id"] = serde_json::Value::String("replacement".to_string());
    std::fs::write(
        &target.record_path,
        serde_json::to_vec(&replacement).expect("serialize replacement"),
    )
    .expect("replace target identity while its lock remains live");

    let err = snapshot_target_state(&registry, &target, "kill")
        .expect_err("a live replacement must not inherit the snapshot action");
    assert_eq!(err.code(), exit::CONTROL);
    assert!(err.to_string().contains("changed identity"));

    drop(registration);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_replacement_record_is_not_mistaken_for_an_already_gone_target() {
    let dir = scratch_registry_dir("aggregate-corrupt-replacement");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");
    let registration = registry
        .register_plain("target", Some("endpoint-a"), SystemTime::now())
        .expect("register the live target");
    let mut targets = snapshot_mutation_targets(&registry, &[]).expect("snapshot live targets");
    let target = targets.pop().expect("the target is in the snapshot");

    std::fs::write(&target.record_path, b"not valid JSON")
        .expect("replace the record with corrupt content");

    let err = snapshot_target_state(&registry, &target, "kill")
        .expect_err("a still-present corrupt record is not proof the target ended");
    assert_eq!(err.code(), exit::CONTROL);
    assert!(err.to_string().contains("no longer passes validation"));

    drop(registration);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_snapshot_record_is_an_already_gone_target() {
    let dir = scratch_registry_dir("aggregate-stale-target");
    let registry = registry::Registry::open_in(dir.clone()).expect("open registry");
    write_stale_entry(&dir, "target-stem", "target");
    let entry = registry
        .entries()
        .expect("scan the registry")
        .pop()
        .expect("the stale entry exists");
    let target = MutationTarget {
        run_id: entry.record.run_id,
        record_path: entry.path,
        endpoint: entry.record.endpoint,
    };

    assert_eq!(
        snapshot_target_state(&registry, &target, "kill")
            .expect("confirmed-stale means already gone"),
        SnapshotTargetState::AlreadyGone
    );

    let _ = std::fs::remove_dir_all(&dir);
}
