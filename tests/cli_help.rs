//! Golden snapshots for the public command-line help surface.
//!
//! Every case invokes the built binary. An intentional CLI change is reviewed by
//! regenerating with `UPDATE_CLI_HELP_GOLDEN=1` and inspecting the fixture diff.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

const CASES: &[(&str, &[&str])] = &[
    ("root", &[]),
    ("run", &["run"]),
    ("inspect", &["inspect"]),
    ("cancel", &["cancel"]),
    ("kill", &["kill"]),
    ("wait", &["wait"]),
    ("events", &["events"]),
    ("list", &["list"]),
    ("prune", &["prune"]),
    ("probe", &["probe"]),
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/cli-help")
        .join(format!("{name}.txt"))
}

fn render(args: &[&str]) -> String {
    let output = Command::new(common::bin())
        .args(args)
        .arg("--help")
        .output()
        .expect("invoke the built binary");
    assert!(
        output.status.success(),
        "help failed for {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "help must not write diagnostics for {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("help is UTF-8")
        .replace("\r\n", "\n")
        .replace("processkit-cli.exe", "processkit-cli")
}

#[test]
fn built_binary_help_matches_the_golden_surface() {
    let update = std::env::var_os("UPDATE_CLI_HELP_GOLDEN").is_some();
    for (name, args) in CASES {
        let rendered = render(args);
        let path = fixture(name);
        if update {
            std::fs::write(&path, &rendered).expect("rewrite CLI help fixture");
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "read CLI help fixture {}: {err}; regenerate with UPDATE_CLI_HELP_GOLDEN=1",
                path.display()
            )
        });
        assert_eq!(
            rendered,
            expected.replace("\r\n", "\n"),
            "CLI help drift for `{name}`; if intentional, regenerate with \
             UPDATE_CLI_HELP_GOLDEN=1 and review the fixture diff"
        );
    }
}
