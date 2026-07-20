//! Test-support worker for the end-to-end containment tier (`tests/e2e.rs`).
//!
//! Gated behind the `e2e` Cargo feature so normal and published builds never
//! include it (see `Cargo.toml`). It is **not** part of the shipped surface — the
//! product binary is `processkit-cli` (`src/main.rs`). This helper exists only so
//! the e2e tier can drive a precise, cross-platform process shape: a `root` that
//! leaks a long-lived grandchild, a `spin` worker that reports its own PID and
//! heartbeats, and a trivial `exit` leaf.
//!
//! Why a compiled helper rather than a shell script: reporting a process's own
//! PID portably. A `cmd` batch file cannot print its PID, and the tier needs the
//! grandchild's exact PID to observe teardown *from outside* the runner. Every
//! long-lived mode is bounded by `--sleep-secs`, so even a catastrophic test abort
//! self-heals instead of leaking a process forever.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("root") => root(&args[1..]),
        Some("spin") => spin(&args[1..]),
        Some("exit") => leaf_exit(&args[1..]),
        other => {
            eprintln!("e2e-helper: expected a mode (root|spin|exit), got {other:?}");
            ExitCode::from(2)
        }
    }
}

/// Root of a leaked tree: spawn a long-lived grandchild that must outlive us,
/// record its PID for the harness, then exit with `--code`. `--root-sleep-secs`
/// keeps us alive first (so the runner stays blocked in its wait for the
/// abrupt-kill scenario); `--hold-stdout` hands the runner's piped stdout down to
/// the grandchild so it keeps that handle open after we exit.
fn root(args: &[String]) -> ExitCode {
    let code = u8_flag(args, "--code", 0);
    let root_sleep = u64_flag(args, "--root-sleep-secs", 0);
    let grandchild_sleep = u64_flag(args, "--grandchild-sleep-secs", 120);
    let hold_stdout = has_flag(args, "--hold-stdout");

    let exe = std::env::current_exe().expect("resolve current exe");
    let mut cmd = Command::new(&exe);
    cmd.arg("spin")
        .arg("--sleep-secs")
        .arg(grandchild_sleep.to_string())
        .stdin(Stdio::null());
    // The grandchild keeps the runner's piped stdout only for the held-pipe
    // scenario; otherwise it must not, so a clean teardown is never conflated with
    // a runner hung on a leaked pipe handle.
    if hold_stdout {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let child = cmd.spawn().expect("spawn grandchild");
    let grandchild_pid = child.id();
    // Drop the handle without waiting: the grandchild must outlive us so the
    // *container's* teardown, not our own reaping, is what removes it. Dropping a
    // `std` `Child` neither kills nor waits the process.
    drop(child);

    if let Some(path) = flag_value(args, "--pidfile") {
        write_atomic(Path::new(path), &grandchild_pid.to_string());
    }
    // A marker on stdout proves the root actually ran (and, with `--hold-stdout`,
    // that the pipe reached the child).
    println!("e2e-helper root: spawned grandchild {grandchild_pid}");

    if root_sleep > 0 {
        sleep(Duration::from_secs(root_sleep));
    }
    ExitCode::from(code)
}

/// A long-lived worker: record our own PID (if `--pidfile`), append to a
/// heartbeat file on a fixed cadence (if `--heartbeat`) so an external observer
/// can watch liveness by the file growing, and stay alive until a bounded
/// deadline — bounded so a teardown regression self-heals instead of leaking.
fn spin(args: &[String]) -> ExitCode {
    let seconds = u64_flag(args, "--sleep-secs", 120);
    if let Some(path) = flag_value(args, "--pidfile") {
        write_atomic(Path::new(path), &std::process::id().to_string());
    }
    let heartbeat = flag_value(args, "--heartbeat").map(PathBuf::from);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Some(path) = &heartbeat {
            append_heartbeat(path);
        }
        sleep(Duration::from_millis(100));
    }
    ExitCode::SUCCESS
}

/// Append one byte to the heartbeat file (best-effort), so an external observer
/// sees the worker's liveness as a steadily growing file.
fn append_heartbeat(path: &Path) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(b"x");
        let _ = file.flush();
    }
}

/// A trivial leaf that exits immediately with `--code` — the churned child of the
/// rapid launch/exit/relaunch (PID-reuse) scenario.
fn leaf_exit(args: &[String]) -> ExitCode {
    ExitCode::from(u8_flag(args, "--code", 0))
}

/// The value following `name` in `args`, if present.
fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Whether the bare flag `name` is present in `args`.
fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

/// A `u64` flag, or `default` when absent or unparseable.
fn u64_flag(args: &[String], name: &str, default: u64) -> u64 {
    flag_value(args, name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// A `u8` flag (an exit code), or `default` when absent or unparseable.
fn u8_flag(args: &[String], name: &str, default: u8) -> u8 {
    flag_value(args, name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Write `contents` to `path` via a temp file + rename, so a concurrent reader
/// never observes a half-written PID.
fn write_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, contents).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}
