//! Drift checks for the prose that restates what this binary produces: the
//! installable `using-processkit-cli` agent skill, the documents that publish the
//! subcommand inventory itself, and the one row of `fixtures/schema/cli/README.md`
//! that enumerates which failure verdicts the golden `error.jsonl` pins.
//!
//! Each tier reads markdown (and, depending on the tier, source, schema, or a golden
//! fixture) off disk and asserts it against the surface the *built binary* reports or
//! the artifact that binary generated, never against a list maintained here — a
//! hand-maintained expectation would drift in exactly the way these checks exist to
//! catch.

mod common;

use std::path::Path;
use std::process::Command;

use processkit_cli::{events, exit};

const SKILL: &str = include_str!("../skills/using-processkit-cli/SKILL.md");
const ARCHITECTURE: &str = include_str!("../docs/architecture.md");
const EXIT_CODES: &str = include_str!("../docs/exit-codes.md");
const ERROR_ENVELOPE: &str = include_str!("../src/error_envelope.rs");
const ERROR_SCHEMA: &str = include_str!("../fixtures/schema/cli/error.schema.json");
const SCHEMA_README: &str = include_str!("../fixtures/schema/cli/README.md");
const ERROR_FIXTURE: &str = include_str!("../fixtures/schema/cli/error.jsonl");

#[test]
fn skill_facts_match_the_built_compatibility_surface() {
    let output = Command::new(common::bin())
        .args(["probe", "--json"])
        .output()
        .expect("run built probe");
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("probe JSON");

    assert_eq!(report["schema_version"], events::SCHEMA_VERSION);
    assert_eq!(report["exit_code_band"]["start"], exit::RUNNER_RANGE_START);
    assert_eq!(report["exit_code_band"]["end"], exit::RUNNER_RANGE_END);
    assert!(SKILL.contains(&format!(
        "--require-schema-version {}",
        events::SCHEMA_VERSION
    )));
    assert!(SKILL.contains(&format!(
        "--require-exit-code-band {}-{}",
        exit::RUNNER_RANGE_START,
        exit::RUNNER_RANGE_END
    )));

    let surface = report["surface"].as_array().expect("surface array");
    for (token, spelling) in [
        ("run:--jsonl", "--jsonl"),
        ("run:--timeout", "--timeout"),
        ("run:--idle-timeout", "--idle-timeout"),
        ("run:--capture-overflow", "--capture-overflow"),
        ("run:--create-no-window", "--create-no-window"),
        ("run:--detach", "--detach"),
        ("wait:--timeout", "wait --run-id"),
        ("inspect:--all", "inspect --all --json"),
        ("cancel:--all", "cancel --all"),
        ("kill:--all", "kill --all"),
        ("events:--follow", "events --run-id build-42 --follow"),
        ("events:--validate", "--validate"),
    ] {
        assert!(
            surface.iter().any(|value| value == token),
            "live probe surface lost {token}"
        );
        assert!(SKILL.contains(spelling), "skill lost {spelling}");
    }

    for (name, code) in [
        ("TIMEOUT", exit::TIMEOUT),
        ("CANCELLED", exit::CANCELLED),
        ("CONTROL_CANCELLED", exit::CONTROL_CANCELLED),
        ("CONTROL_KILLED", exit::CONTROL_KILLED),
        ("WAIT_TIMEOUT", exit::WAIT_TIMEOUT),
        ("OUTPUT_OVERFLOW", exit::OUTPUT_OVERFLOW),
        ("EVENTS_INVALID", exit::EVENTS_INVALID),
    ] {
        assert!(
            SKILL.contains(&format!("{name} ({code})")),
            "skill exit-code fact drifted for {name}"
        );
    }
}

/// The subcommands this binary really has, read off the **built** surface rather
/// than from a list kept here: `probe --json`'s tokens are derived from the live
/// clap tree (`src/probe.rs`, `surface_tokens`), and a token with no `:` in it is
/// exactly a subcommand name — every flag token carries one (`run:--jsonl`), and so
/// does every capability token (`attest:peer-identity`, `run:resource-summary`),
/// deliberately.
fn built_subcommand_names() -> Vec<String> {
    let output = Command::new(common::bin())
        .args(["probe", "--json"])
        .output()
        .expect("run built probe");
    assert!(
        output.status.success(),
        "an unqualified `probe --json` is a healthy self-report"
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("probe JSON");
    let mut names: Vec<String> = report["surface"]
        .as_array()
        .expect("surface array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|token| !token.contains(':'))
        .map(str::to_owned)
        .collect();
    names.sort();
    assert!(
        names.iter().any(|name| name == "run"),
        "the surface carries one bare token per subcommand: {names:?}"
    );
    names
}

/// The `.rs` files `src/cli/` really holds — the directory `docs/architecture.md`
/// both counts and walks file by file.
fn cli_source_files() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli");
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .expect("read src/cli")
        .map(|entry| entry.expect("read a src/cli entry").file_name())
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    files.sort();
    assert!(
        files.iter().any(|file| file == "mod.rs"),
        "src/cli/ is a directory module: {files:?}"
    );
    files
}

/// The one markdown table row of `document` that starts with `prefix`.
fn table_row<'a>(document: &'a str, prefix: &str) -> &'a str {
    let mut rows = document.lines().filter(|line| line.starts_with(prefix));
    let row = rows
        .next()
        .unwrap_or_else(|| panic!("no table row starts with `{prefix}`"));
    assert!(
        rows.next().is_none(),
        "`{prefix}` must identify exactly one row"
    );
    row
}

/// The `///` block immediately above the line holding `anchor`, flattened onto one
/// line so a doc comment's own wrapping never decides what it is read to say.
fn doc_comment_above(source: &str, anchor: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let anchored = lines
        .iter()
        .position(|line| line.contains(anchor))
        .unwrap_or_else(|| panic!("`{anchor}` must appear in the source"));
    let mut doc: Vec<&str> = lines[..anchored]
        .iter()
        .rev()
        .take_while(|line| line.trim_start().starts_with("///"))
        .map(|line| line.trim_start().trim_start_matches("///").trim())
        .collect();
    doc.reverse();
    assert!(
        !doc.is_empty(),
        "`{anchor}` must carry a doc comment to check"
    );
    doc.join(" ")
}

/// The directory size `docs/architecture.md` states in prose ("A directory of ten
/// files"), as a number — spelled out, as this project's prose spells counts, or as
/// digits if that ever changes.
fn stated_file_count(row: &str) -> usize {
    const NUMERALS: [(&str, usize); 12] = [
        ("one", 1),
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("eight", 8),
        ("nine", 9),
        ("ten", 10),
        ("eleven", 11),
        ("twelve", 12),
    ];

    let tail = row
        .split("A directory of ")
        .nth(1)
        .expect("the `src/cli/` row states how many files the directory holds");
    let word = tail
        .split_whitespace()
        .next()
        .expect("the count is followed by the noun it counts");
    NUMERALS
        .iter()
        .find(|(numeral, _)| *numeral == word)
        .map(|(_, value)| *value)
        .or_else(|| word.parse().ok())
        .unwrap_or_else(|| panic!("unreadable file count `{word}` in the `src/cli/` row"))
}

/// The documents that publish the **subcommand inventory** must not drift from the
/// binary that defines it.
///
/// A new subcommand touches far more prose than its own module, and every echo site
/// below claims completeness: `docs/architecture.md`'s `src/cli/` row enumerates the
/// `Command` variants, states how many files that directory holds, and then walks it
/// file by file; `docs/exit-codes.md`'s `operation` row publishes the same names as a
/// closed vocabulary, as do the doc comment on `ErrorEnvelope::operation` (which
/// `fixtures/schema/cli/error.schema.json` names as its in-code source of truth) and
/// that schema's own `operation` enum. Each of those has been caught stale by hand at
/// least once, so none of them is left to the next reviewer's eye: they are checked
/// against the live surface instead, the same way
/// `skill_facts_match_the_built_compatibility_surface` checks the agent skill.
#[test]
fn the_documents_that_publish_the_subcommand_inventory_match_the_binary() {
    let names = built_subcommand_names();

    // The module map: the `Command` enumeration, the file count, and the per-file
    // walkthrough — the three ways one row can go stale at once.
    let cli_row = table_row(ARCHITECTURE, "| [`src/cli/`]");
    for name in &names {
        assert!(
            cli_row.contains(&format!("`{name}`")),
            "docs/architecture.md's `src/cli/` row never names the `{name}` subcommand"
        );
    }
    let files = cli_source_files();
    for file in &files {
        assert!(
            cli_row.contains(&format!("`{file}`")),
            "docs/architecture.md's `src/cli/` row walks the directory file by file, \
             but never names `{file}`"
        );
    }
    assert_eq!(
        stated_file_count(cli_row),
        files.len(),
        "docs/architecture.md states a different `src/cli/` file count than the \
         directory holds: {files:?}"
    );

    // The published `operation` vocabulary, in all three places it is written down.
    let operation_row = table_row(EXIT_CODES, "| `operation` |");
    for name in &names {
        assert!(
            operation_row.contains(&format!("`{name}`")),
            "docs/exit-codes.md's `operation` row never publishes `{name}`"
        );
    }

    let field_doc = doc_comment_above(ERROR_ENVELOPE, "pub operation: &'static str,");
    for name in &names {
        assert!(
            field_doc.contains(&format!("`{name}`")),
            "`ErrorEnvelope::operation`'s doc comment — the schema's declared in-code \
             source of truth — never lists `{name}`"
        );
    }

    let schema: serde_json::Value = serde_json::from_str(ERROR_SCHEMA).expect("error schema JSON");
    let mut published: Vec<String> =
        schema["$defs"]["errorEnvelope"]["properties"]["operation"]["enum"]
            .as_array()
            .expect("the schema publishes `operation` as a closed enum")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
    published.sort();
    assert_eq!(
        published, names,
        "fixtures/schema/cli/error.schema.json's `operation` enum and the binary's \
         subcommands disagree"
    );
}

/// The one row of `fixtures/schema/cli/README.md`'s fixture table that publishes a
/// **vocabulary** rather than a description: `error.jsonl`'s row names the envelope
/// kinds the fixture carries a line for, and adapters read it as an index of what
/// this family has actually pinned.
///
/// It is prose enumerating a set that is generated elsewhere, which is precisely the
/// shape of claim that goes stale without a single test failing — this one already
/// did, promising "every reserved code a single subcommand's own verdict carries"
/// while `wait_timeout` (112) had no line in the file. `src/error_envelope.rs`
/// guards the other end of that chain (the build's own code-to-kind table against
/// the fixture's lines); this holds the sentence itself against them, so a kind can
/// no longer be named here without a line, or pinned by a line without being named.
///
/// The row is read against the schema's published `kind` enum rather than a list
/// kept here, so the vocabulary this test recognizes cannot drift from the one the
/// document defines.
#[test]
fn the_error_fixtures_documented_coverage_names_exactly_the_kinds_it_pins() {
    let schema: serde_json::Value = serde_json::from_str(ERROR_SCHEMA).expect("error schema JSON");
    let vocabulary: Vec<&str> = schema["$defs"]["errorEnvelope"]["properties"]["kind"]["enum"]
        .as_array()
        .expect("the schema publishes `kind` as a closed enum")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();

    let mut carried: Vec<&str> = ERROR_FIXTURE
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("every error.jsonl line is valid JSON");
            let kind = value["kind"]
                .as_str()
                .expect("every error.jsonl line carries a kind")
                .to_owned();
            *vocabulary
                .iter()
                .find(|published| **published == kind)
                .unwrap_or_else(|| {
                    panic!("error.jsonl pins `{kind}`, which the schema never publishes")
                })
        })
        .collect();
    carried.sort_unstable();
    carried.dedup();

    let row = table_row(SCHEMA_README, "| `error.jsonl` |");
    let mut named: Vec<&str> = vocabulary
        .iter()
        .copied()
        .filter(|kind| row.contains(&format!("`{kind}`")))
        .collect();
    named.sort_unstable();

    assert_eq!(
        named, carried,
        "fixtures/schema/cli/README.md's `error.jsonl` row and the fixture itself must name the \
         same kinds — the row is read as this family's index of coverage, so a name it lists \
         without a line (or a line it never lists) publishes a completeness that does not exist"
    );
}
