//! Drift checks for the installable `using-processkit-cli` agent skill.

mod common;

use std::process::Command;

use processkit_cli::{events, exit};

const SKILL: &str = include_str!("../skills/using-processkit-cli/SKILL.md");

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
    ] {
        assert!(
            SKILL.contains(&format!("{name} ({code})")),
            "skill exit-code fact drifted for {name}"
        );
    }
}
