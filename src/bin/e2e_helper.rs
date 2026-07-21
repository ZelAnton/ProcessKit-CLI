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
        Some("job-parent") => job_parent(&args[1..]),
        other => {
            eprintln!("e2e-helper: expected a mode (root|spin|exit|job-parent), got {other:?}");
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

/// A wrapper that places **itself** into a fresh Windows Job Object and then runs
/// the tail command (`-- <program> <args…>`) as a child, forwarding its exit code.
///
/// This backs the nested-Job-Object e2e scenario: the child it launches — a
/// `processkit-cli run` — is thus started from an environment that is *already*
/// inside a Job Object (like a terminal, build server, or IDE), and `run` must still
/// create and tear down its own **nested** container. The outer job is deliberately
/// plain (no `KILL_ON_JOB_CLOSE`), so it never reaps anything itself — proving the
/// *inner* `run` container is what contains and reaps the tree even when nested.
///
/// On non-Windows this is a plain passthrough (the scenario that uses it is
/// Windows-only), so the helper binary still builds on every platform.
fn job_parent(args: &[String]) -> ExitCode {
    let command = match tail_after_double_dash(args) {
        Some(command) if !command.is_empty() => command,
        _ => {
            eprintln!("e2e-helper job-parent: expected `-- <program> <args...>`");
            return ExitCode::from(2);
        }
    };
    #[cfg(windows)]
    {
        if let Err(err) = place_self_in_job_object() {
            eprintln!("e2e-helper job-parent: could not place self in a Job Object: {err}");
            return ExitCode::from(3);
        }
    }
    run_tail(&command)
}

/// The command tokens after the first `--` separator in `args`, if present.
fn tail_after_double_dash(args: &[String]) -> Option<Vec<String>> {
    let separator = args.iter().position(|arg| arg == "--")?;
    Some(args[separator + 1..].to_vec())
}

/// Run the tail command inheriting our stdio (so a `run` launched here echoes to
/// wherever this wrapper's stdio points) and forward its exit code.
fn run_tail(command: &[String]) -> ExitCode {
    let (program, program_args) = command
        .split_first()
        .expect("job-parent checked the command is non-empty");
    match Command::new(program).args(program_args).status() {
        // Forward the child's low exit byte (0 stays 0); the scenario only checks 0.
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(err) => {
            eprintln!("e2e-helper job-parent: could not spawn `{program}`: {err}");
            ExitCode::from(4)
        }
    }
}

/// Create a plain, unnamed Job Object and assign the current process to it, so any
/// child spawned afterwards (the `run` under test) is created inside this outer job
/// and must nest its own container within it.
#[cfg(windows)]
fn place_self_in_job_object() -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // No name and no security attributes: a private job. No limits are set, so it has
    // no `KILL_ON_JOB_CLOSE` — it contains membership only and never terminates the
    // tree itself, leaving all reaping to the inner `run` container.
    // SAFETY: both pointers are null (the documented "default" form); the returned
    // handle is checked for null and closed on the error path below.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `job` is a valid job handle just created; `GetCurrentProcess()` is a
    // pseudo-handle with full access to this process, which is what
    // AssignProcessToJobObject requires (PROCESS_SET_QUOTA | PROCESS_TERMINATE).
    let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
    if assigned == 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: `job` is the valid handle from CreateJobObjectW, closed exactly once.
        unsafe { CloseHandle(job) };
        return Err(err);
    }
    // Deliberately keep the job handle open for the rest of this short-lived process:
    // dropping our reference would be harmless (the job outlives it via its member
    // processes), but holding it keeps the outer job unambiguously current while the
    // child `run` nests inside it. The process exits moments later, releasing it.
    Ok(())
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
