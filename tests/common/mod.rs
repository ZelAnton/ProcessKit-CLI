//! Shared fixtures for the through-the-binary integration tests.
//!
//! Every test here drives the *built binary* (`env!("CARGO_BIN_EXE_…")`), not the
//! library, because the value this crate adds over ProcessKit-rs is the binary
//! plus its contracts (`AGENTS.md`, "Testing tiers").

#![allow(dead_code)] // Each `tests/*.rs` is its own crate and uses a subset.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Absolute path to the freshly built `processkit-cli` binary under test.
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_processkit-cli")
}

/// A unique, empty scratch directory under the OS temp dir. Unique per (pid,
/// sequence) so concurrent tests never collide; the caller may leave it behind
/// (the OS temp dir is transient).
pub fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "processkit-cli-it-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Build a `run` invocation of the binary: `run --jsonl <tmp> <flags…> --
/// <program> <args…>`, with `envs` set on the child.
///
/// A throwaway `--jsonl` path is always supplied — it is required by the parser
/// but not consumed in this task, so nothing is written there. `flags` are extra
/// runner options placed *before* `--` (e.g. `--timeout`, `2s`); everything in
/// `program_and_args` lands verbatim after `--`. `envs` are set on the child; the
/// runner inherits its own environment onto the spawned program, which is how the
/// teardown fixtures pass file paths down to a grandchild. The caller decides
/// whether to `.output()` (wait) or `.spawn()` (drive it interactively).
pub fn command_with_flags<I, S>(
    dir: &Path,
    envs: &[(&str, &Path)],
    flags: &[&str],
    program_and_args: I,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let jsonl = dir.join("events.jsonl");
    let mut cmd = Command::new(bin());
    cmd.arg("run").arg("--jsonl").arg(&jsonl);
    cmd.args(flags);
    cmd.arg("--");
    cmd.args(program_and_args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd
}

/// Invoke `run <program> <args…>` through the binary and wait for it to finish.
pub fn run<I, S>(dir: &Path, envs: &[(&str, &Path)], program_and_args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_flags(dir, envs, &[], program_and_args)
}

/// Invoke `run` with extra runner `flags` (e.g. `--timeout`/`--grace`) and wait
/// for it to finish.
pub fn run_with_flags<I, S>(
    dir: &Path,
    envs: &[(&str, &Path)],
    flags: &[&str],
    program_and_args: I,
) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    command_with_flags(dir, envs, flags, program_and_args)
        .output()
        .expect("spawn the runner binary")
}

/// The platform shell invocation (`program` + first arg) that runs `script` as a
/// single inline command string: `cmd /c <script>` on Windows, `sh -c <script>`
/// elsewhere.
pub fn shell_inline(script: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), script.into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }
}
