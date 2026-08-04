//! Behavior of the global `--error-format json` failure envelope, through the
//! **built binary**.
//!
//! The envelope's *shape* is pinned elsewhere, by the published schema document and
//! golden fixture (`fixtures/schema/cli/error.{schema.json,jsonl}`, driven by
//! `tests/machine_output.rs`). This file covers what a fixture cannot: that the flag
//! is accepted wherever it is written, that asking for it changes **only** stderr,
//! that not asking for it changes nothing at all, and that the kinds the golden does
//! not pin are really wired to the code paths they name.
//!
//! Every scenario here is deterministic and process-cheap on purpose — a failed
//! lookup needs no live runner — except where the failure genuinely requires one.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use common::{bin, scratch};
use serde_json::{Value, json};

use processkit_cli::registry::test_support::{
    write_stale_entry_with_stream, write_unprobeable_entry,
};

/// Invoke the binary against an isolated scratch registry.
fn cli(registry: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("PROCESSKIT_CLI_REGISTRY_DIR", registry)
        .output()
        .unwrap_or_else(|err| panic!("spawn `processkit-cli {}`: {err}", args.join(" ")))
}

/// stderr as text, with Windows line endings normalized away.
fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n")
}

/// Parse the one envelope a failed invocation printed on stderr, asserting the exit
/// code on the way. The envelope is required to be the **last** line rather than the
/// only one: for `run`, the child's echoed stderr shares this stream (see
/// `src/error_envelope.rs`, "Where it is printed"), and for every other command the
/// runner may have emitted a `processkit-cli: warning: …` line, which is not a
/// failure and keeps its prose.
fn envelope(out: &Output, code: i32, what: &str) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{what} must exit {code}; stderr: {}",
        stderr_of(out)
    );
    let text = stderr_of(out);
    let last = text
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .unwrap_or_else(|| panic!("{what} printed nothing on stderr"));
    let value: Value = serde_json::from_str(last)
        .unwrap_or_else(|err| panic!("{what} ends with one JSON envelope: {last}: {err}"));
    assert_eq!(
        value["error_version"],
        json!(1),
        "every envelope carries the version a consumer pins: {last}"
    );
    assert_eq!(
        value["code"],
        json!(code),
        "the envelope's code is the process's own exit status: {last}"
    );
    value
}

/// A registry directory holding one entry whose liveness can never be established —
/// the state `wait` refuses to read as finished, and `inspect` refuses to act on.
fn registry_with_unprobeable_entry(tag: &str) -> std::path::PathBuf {
    let dir = scratch(tag);
    let registry = dir.join("registry");
    write_unprobeable_entry(&registry, "build-44", "build-44");
    registry
}

#[test]
fn the_default_stays_byte_for_byte_the_prose_it_always_was() {
    // The regression that matters most: this feature is opt-in, and an operator (or
    // a script grepping stderr) who never asked for it must not be able to tell it
    // shipped.
    let registry = registry_with_unprobeable_entry("envelope-default");

    let implicit = cli(&registry, &["inspect", "--run-id", "build-99"]);
    let explicit = cli(
        &registry,
        &["inspect", "--run-id", "build-99", "--error-format", "human"],
    );

    assert_eq!(
        implicit.status.code(),
        Some(103),
        "stderr: {}",
        stderr_of(&implicit)
    );
    assert_eq!(
        stderr_of(&implicit),
        stderr_of(&explicit),
        "`--error-format human` is exactly the default, not a second rendering"
    );
    assert!(
        stderr_of(&implicit).starts_with("processkit-cli: "),
        "the default prose keeps its historical prefix: {}",
        stderr_of(&implicit)
    );
    assert!(
        !stderr_of(&implicit).contains("error_version"),
        "no envelope leaks into the default path: {}",
        stderr_of(&implicit)
    );
}

#[test]
fn the_envelopes_message_is_exactly_the_prose_it_replaces() {
    // `message` is the prose, verbatim — which is what makes it safe to say the
    // envelope *replaces* the line rather than summarizing it, and why nothing is
    // lost by opting in.
    let registry = registry_with_unprobeable_entry("envelope-message");

    let prose = cli(&registry, &["inspect", "--run-id", "build-99"]);
    let machine = cli(
        &registry,
        &["inspect", "--run-id", "build-99", "--error-format", "json"],
    );

    let expected = stderr_of(&prose)
        .trim_end_matches('\n')
        .trim_start_matches("processkit-cli: ")
        .to_string();
    let value = envelope(&machine, 103, "`inspect` against an unregistered run");
    assert_eq!(value["message"], json!(expected));
}

#[test]
fn the_flag_is_honored_wherever_it_is_written() {
    // `global = true` in one place; three real invocations here, because a consumer
    // appending its own flags after the subcommand must get the same answer as one
    // that sets them up front.
    let registry = registry_with_unprobeable_entry("envelope-position");

    let before = cli(
        &registry,
        &["--error-format", "json", "inspect", "--run-id", "build-99"],
    );
    let after = cli(
        &registry,
        &["inspect", "--run-id", "build-99", "--error-format", "json"],
    );
    let between = cli(
        &registry,
        &["inspect", "--error-format", "json", "--run-id", "build-99"],
    );

    let first = envelope(&before, 103, "the flag before the subcommand");
    assert_eq!(
        first,
        envelope(&after, 103, "the flag after the subcommand"),
        "position must not change a single field"
    );
    assert_eq!(
        first,
        envelope(&between, 103, "the flag among the subcommand's own flags"),
        "position must not change a single field"
    );
    assert_eq!(first["kind"], json!("not_found"));
    assert_eq!(first["operation"], json!("inspect"));
}

#[test]
fn a_waiter_that_gives_up_is_its_own_kind_and_is_retryable() {
    // An entry that can never be probed is never read as finished, so `wait` runs to
    // its own deadline — the cheapest honest way to reach WAIT_TIMEOUT (112) without
    // a live runner. The kind must not be the run's `timeout`: the run was not
    // touched, which is exactly the confusion this taxonomy exists to prevent.
    //
    // `fixtures/schema/cli/error.jsonl` pins this envelope's *shape* from the same
    // scenario (`tests/machine_output.rs`); what is asserted here is the reading —
    // which kind, and why a retry is the intended response — which a golden line
    // cannot state.
    let registry = registry_with_unprobeable_entry("envelope-wait");

    let out = cli(
        &registry,
        &[
            "wait",
            "--run-id",
            "build-44",
            "--timeout",
            "150ms",
            "--error-format",
            "json",
        ],
    );
    let value = envelope(&out, 112, "`wait` giving up on an unprobeable entry");
    assert_eq!(value["kind"], json!("wait_timeout"));
    assert_eq!(value["operation"], json!("wait"));
    assert_eq!(value["run_id"], json!("build-44"));
    assert_eq!(
        value["retryable"],
        json!(true),
        "the run is still live and untouched, so waiting again is the intended response"
    );
}

#[test]
fn an_unreadable_registry_is_named_as_such_rather_than_as_a_generic_setup_failure() {
    // Pointing the registry at a regular file makes the directory scan fail the same
    // way an unreadable registry directory would, on every platform. The exit code is
    // the SETUP (111) it always was; the kind is what tells an operator *which*
    // prerequisite failed.
    let dir = scratch("envelope-registry");
    let not_a_directory = dir.join("registry-is-a-file");
    fs::write(&not_a_directory, b"").expect("write the not-a-directory fixture");

    let out = cli(&not_a_directory, &["list", "--error-format", "json"]);
    let value = envelope(&out, 111, "`list` against an unreadable registry");
    assert_eq!(value["kind"], json!("registry"));
    assert_eq!(value["operation"], json!("list"));
    assert_eq!(
        value["run_id"],
        Value::Null,
        "a whole-registry command names no single run"
    );
}

#[test]
fn an_unreadable_stream_is_a_setup_failure_not_a_verdict_about_the_document() {
    // The distinction `events --validate`'s own exit code makes (114 means "checked
    // and invalid", 111 means "could not check") is carried by the kind too.
    let dir = scratch("envelope-setup");
    let registry = dir.join("registry");
    let missing = dir.join("no-such-stream.jsonl");

    let out = cli(
        &registry,
        &[
            "events",
            "--file",
            &missing.to_string_lossy(),
            "--validate",
            "--error-format",
            "json",
        ],
    );
    let value = envelope(&out, 111, "`events --validate` on a missing stream");
    assert_eq!(value["kind"], json!("setup"));
    assert_eq!(value["operation"], json!("events"));
}

#[test]
fn several_streams_under_one_run_id_are_refused_as_ambiguous() {
    // The registry does not enforce run-id uniqueness, so a reader that found two
    // different streams under one id must refuse rather than pick — and say so in a
    // way an adapter can branch on, since the fix (use a unique id, or `--file`) is
    // the caller's.
    let dir = scratch("envelope-ambiguous");
    let registry = dir.join("registry");
    write_stale_entry_with_stream(&registry, "build-45a", "build-45", "/samples/a.jsonl");
    write_stale_entry_with_stream(&registry, "build-45b", "build-45", "/samples/b.jsonl");

    let out = cli(
        &registry,
        &["events", "--run-id", "build-45", "--error-format", "json"],
    );
    let value = envelope(&out, 103, "`events` against an ambiguous run id");
    assert_eq!(value["kind"], json!("ambiguous_run_id"));
    assert_eq!(value["run_id"], json!("build-45"));
    assert_eq!(
        value["retryable"],
        json!(false),
        "the caller has to change the invocation; retrying cannot help"
    );
}

#[test]
fn a_run_that_never_starts_reports_the_event_streams_own_vocabulary() {
    // `run` honors the flag like every other subcommand — the flag is global, and a
    // global option that some subcommand quietly ignored would make `probe`'s
    // `run:--error-format` token a promise the binary does not keep. The kind is
    // `spawn_error`, spelled exactly as the terminal `runner_exit` event's `source`
    // would spell the same ending: this envelope mirrors that vocabulary instead of
    // forking it.
    let dir = scratch("envelope-run");
    let registry = dir.join("registry");

    let jsonl = dir.join("events.jsonl");
    let out = cli(
        &registry,
        &[
            "run",
            "--jsonl",
            &jsonl.to_string_lossy(),
            "--error-format",
            "json",
            "--",
            "processkit-cli-no-such-program-b7f3",
        ],
    );
    let value = envelope(&out, 101, "`run` against a program that does not exist");
    assert_eq!(value["kind"], json!("spawn_error"));
    assert_eq!(value["operation"], json!("run"));
    assert_eq!(
        value["run_id"],
        Value::Null,
        "the invocation named no run id, and the generated one is minted inside the run"
    );

    // The id the *caller* chose is echoed when there is one.
    let named = cli(
        &registry,
        &[
            "run",
            "--jsonl",
            &jsonl.to_string_lossy(),
            "--run-id",
            "build-46",
            "--error-format",
            "json",
            "--",
            "processkit-cli-no-such-program-b7f3",
        ],
    );
    let named = envelope(&named, 101, "a named `run` against a missing program");
    assert_eq!(named["run_id"], json!("build-46"));
}

#[test]
fn asking_for_the_envelope_changes_nothing_a_successful_command_prints() {
    // The whole invariant behind putting this on stderr: a caller may turn the flag
    // on permanently, for every invocation, without any risk to the stdout it parses.
    let dir = scratch("envelope-success");
    let registry = dir.join("registry");
    write_unprobeable_entry(&registry, "build-44", "build-44");

    let plain = cli(&registry, &["list", "--json"]);
    let with_flag = cli(&registry, &["list", "--json", "--error-format", "json"]);

    assert_eq!(plain.status.code(), Some(0));
    assert_eq!(with_flag.status.code(), Some(0));
    assert_eq!(
        plain.stdout, with_flag.stdout,
        "a successful command's stdout is untouched by the error format"
    );
    assert!(
        !String::from_utf8_lossy(&with_flag.stdout).contains("error_version"),
        "and nothing about the envelope appears when nothing failed"
    );
}
