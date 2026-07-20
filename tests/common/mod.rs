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

/// The `--jsonl` events file a `run` invocation for `dir` writes to. The
/// [`command_with_flags`] builder points `--jsonl` here, so a test reads this path
/// to inspect the emitted JSONL event stream.
pub fn events_path(dir: &Path) -> PathBuf {
    dir.join("events.jsonl")
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
/// The `--jsonl` path is always [`events_path`] under `dir`; a test that inspects
/// the JSONL event stream reads that same path. `flags` are extra runner options
/// placed *before* `--` (e.g. `--timeout`, `2s`); everything in
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
    let jsonl = events_path(dir);
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

// ---------------------------------------------------------------------------
// End-to-end containment tier support (behind the `e2e` Cargo feature).
//
// These back `tests/e2e.rs` and are compiled only under `--features e2e` — the
// same gate that builds the tier and its `e2e_helper` worker — so the default
// `cargo test` never pulls them (nor the `CARGO_BIN_EXE_e2e_helper` env this
// module reads). Re-exported flat, so the tier calls them as `common::<name>`.
// ---------------------------------------------------------------------------
// Each `tests/*.rs` is its own crate and uses a subset of these (only `e2e.rs`
// touches them), so the re-export is "unused" from the others' point of view.
#[cfg(feature = "e2e")]
#[allow(unused_imports)]
pub use e2e::*;

#[cfg(feature = "e2e")]
mod e2e {
    use std::path::{Path, PathBuf};
    use std::process::{Child, ExitStatus};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    /// Absolute path to the built end-to-end helper worker
    /// (`src/bin/e2e_helper.rs`) — a test-support process the tier drives to leak
    /// grandchildren and report their PIDs. Built alongside the tier because both
    /// require the `e2e` feature, so this env var is always set when this compiles.
    pub fn helper_bin() -> &'static str {
        env!("CARGO_BIN_EXE_e2e_helper")
    }

    /// Whether a process with `pid` currently exists — the tier's **external**
    /// liveness observation, deliberately independent of the runner's own
    /// bookkeeping (an OS process-table query, not the container's member list).
    ///
    /// Best-effort and racy against PID reuse by design: a freshly recycled PID
    /// can read as alive. Callers pair it with a leaked worker that self-bounds
    /// (see `e2e_helper`) so the answer is meaningful within the poll window.
    pub fn pid_is_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // `kill(pid, 0)` probes without delivering a signal: `Ok` => the
            // process exists; `ESRCH` => it is gone; `EPERM` => it exists under a
            // different uid (still alive).
            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if rc == 0 {
                return true;
            }
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
        #[cfg(windows)]
        {
            // No extra dependency: ask the OS process table directly. `/NH` drops
            // the header, so a matching PID yields a data row and a miss yields the
            // "No tasks..." notice.
            match std::process::Command::new("tasklist")
                .args(["/NH", "/FI", &format!("PID eq {pid}")])
                .output()
            {
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let needle = pid.to_string();
                    !text.contains("No tasks")
                        && text.lines().any(|line| {
                            line.split_whitespace()
                                .any(|token| token == needle.as_str())
                        })
                }
                // If we cannot query, do not fabricate liveness.
                Err(_) => false,
            }
        }
    }

    /// Poll `cond` until it holds or `timeout` elapses; returns whether it held.
    pub fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            sleep(Duration::from_millis(50));
        }
    }

    /// Wait until `path` exists and is non-empty (a worker has written it), or
    /// `timeout` elapses.
    pub fn wait_for_file_nonempty(path: &Path, timeout: Duration) -> bool {
        wait_until(|| file_len(path) > 0, timeout)
    }

    /// Size of `path` in bytes, or 0 when it does not exist yet.
    pub fn file_len(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// Read a PID a worker recorded (atomically) into `path`, if present and
    /// parseable.
    pub fn read_pid(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// Read and parse the runner's JSONL event stream at `path` — one JSON object
    /// per non-empty line. Panics if a line is not well-formed JSON, since a
    /// malformed stream is itself a contract violation. Lets the tier cross-check
    /// the *event contract* against the containment it observed by PID.
    pub fn read_events(path: &Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("read events file {}: {err}", path.display()));
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each event line is valid JSON"))
            .collect()
    }

    /// Wait up to `timeout` for `child` to exit, polling **without consuming** it
    /// so the handle stays available to kill on a hang. `None` means it was still
    /// running at the deadline. The caller must not pipe `child`'s stdio into an
    /// unread buffer — a full pipe could stall it — so the tier spawns with null
    /// stdio for exactly this reason.
    pub fn wait_child_bounded(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_millis(50));
        }
    }

    /// A scratch directory scoped to one scenario, removed on drop regardless of
    /// how the scenario ended (success, assertion panic, early return).
    ///
    /// Leaked **processes** are deliberately *not* chased by PID here: killing a
    /// number could hit a recycled PID — the very hazard this tier guards against.
    /// The helper workers self-terminate on a bounded timer instead, and processes
    /// the tier owns a handle for are torn down via [`ChildGuard`] (identity-safe).
    pub struct Scenario {
        pub dir: PathBuf,
    }

    impl Scenario {
        pub fn new(tag: &str) -> Self {
            Self {
                dir: super::scratch(tag),
            }
        }

        /// A path under this scenario's directory.
        pub fn path(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }
    }

    impl Drop for Scenario {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Owns one spawned [`Child`] and kills it on drop **via its handle** — an
    /// identity-safe teardown that cannot hit a recycled PID (unlike killing by
    /// number), because `std` refuses to signal a child it has already reaped.
    /// Used for processes the tier starts directly (a runner it will abort, a
    /// standalone bystander).
    pub struct ChildGuard {
        child: Option<Child>,
    }

    impl ChildGuard {
        pub fn new(child: Child) -> Self {
            Self { child: Some(child) }
        }

        /// Borrow the child (e.g. to read its PID or poll it).
        pub fn child_mut(&mut self) -> &mut Child {
            self.child.as_mut().expect("child guard holds a live child")
        }

        /// Kill and reap the child now (idempotent; safe after it already exited).
        pub fn kill_now(&mut self) {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            self.kill_now();
        }
    }
}
