//! Test-support worker for the end-to-end containment tier (`tests/e2e.rs`).
//!
//! Gated behind the `e2e` Cargo feature so normal and published builds never
//! include it (see `Cargo.toml`). It is **not** part of the shipped surface — the
//! product binary is `processkit-cli` (`src/main.rs`). This helper exists only so
//! the e2e tier can drive a precise, cross-platform process shape: a `root` that
//! leaks a long-lived grandchild, a `spin` worker that reports its own PID and
//! heartbeats, a trivial `exit` leaf, and Windows console probes for inherited
//! stdio.
//!
//! Why a compiled helper rather than a shell script: reporting a process's own
//! PID portably. A `cmd` batch file cannot print its PID, and the tier needs the
//! grandchild's exact PID to observe teardown *from outside* the runner. Every
//! long-lived mode is bounded by `--sleep-secs`, so even a catastrophic test abort
//! self-heals instead of leaking a process forever.

use std::fs;
use std::io::{IsTerminal, Write};
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
        Some("stdio-report") => stdio_report(&args[1..]),
        Some("console-parent") => console_parent(&args[1..]),
        Some("pty-parent") => pty_parent(&args[1..]),
        other => {
            eprintln!(
                "e2e-helper: expected a mode \
                 (root|spin|exit|job-parent|stdio-report|console-parent|pty-parent), \
                 got {other:?}"
            );
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

/// Record whether this process sees each standard stream as a terminal. The
/// report goes to a separate file so observing the result never redirects the
/// very handles under test.
fn stdio_report(args: &[String]) -> ExitCode {
    let Some(path) = flag_value(args, "--report") else {
        eprintln!("e2e-helper stdio-report: expected --report <path>");
        return ExitCode::from(2);
    };
    let input = if has_flag(args, "--read-line") {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(_) => line.trim_end_matches(['\r', '\n']).to_string(),
            Err(err) => format!("read-error:{err}"),
        }
    } else {
        String::new()
    };
    let report = format!(
        "stdin={}\nstdout={}\nstderr={}\ninput={}\n",
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
        input,
    );
    write_atomic(Path::new(path), &report);
    ExitCode::SUCCESS
}

/// Launch the inherited-stdio scenario behind a fresh Unix pseudo-terminal.
/// Unlike a status-only `isatty` probe, the child also reads a line, proving the
/// inherited terminal remains usable. On process-group-backed targets this also
/// exercises the runner's foreground-group handoff.
fn pty_parent(args: &[String]) -> ExitCode {
    #[cfg(not(unix))]
    {
        let _ = args;
        eprintln!("e2e-helper pty-parent is Unix-only");
        ExitCode::from(2)
    }
    #[cfg(unix)]
    {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;

        let Some(runner) = flag_value(args, "--runner") else {
            eprintln!("e2e-helper pty-parent: expected --runner <path>");
            return ExitCode::from(2);
        };
        let Some(jsonl) = flag_value(args, "--jsonl") else {
            eprintln!("e2e-helper pty-parent: expected --jsonl <path>");
            return ExitCode::from(2);
        };
        let Some(report) = flag_value(args, "--report") else {
            eprintln!("e2e-helper pty-parent: expected --report <path>");
            return ExitCode::from(2);
        };

        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both integer descriptors; terminal and
        // window settings use their documented null/default form.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0
        {
            eprintln!(
                "e2e-helper pty-parent: openpty failed: {}",
                std::io::Error::last_os_error()
            );
            return ExitCode::from(3);
        }
        // SAFETY: openpty returned live descriptors owned by this scope.
        let mut master = unsafe { fs::File::from_raw_fd(master) };
        // SAFETY: duplicating a live slave descriptor yields independent owned
        // descriptors for stdout and stderr.
        let slave_out = unsafe { libc::dup(slave) };
        let slave_err = unsafe { libc::dup(slave) };
        if slave_out < 0 || slave_err < 0 {
            eprintln!(
                "e2e-helper pty-parent: dup failed: {}",
                std::io::Error::last_os_error()
            );
            return ExitCode::from(3);
        }
        // SAFETY: each raw descriptor is valid and transferred exactly once.
        let slave_in = unsafe { OwnedFd::from_raw_fd(slave) };
        let slave_out = unsafe { OwnedFd::from_raw_fd(slave_out) };
        let slave_err = unsafe { OwnedFd::from_raw_fd(slave_err) };

        let helper = std::env::current_exe().expect("resolve current e2e helper");
        let mut command = Command::new(runner);
        command
            .args([
                "run",
                "--inherit-stdio",
                "--timeout",
                "10s",
                "--jsonl",
                jsonl,
                "--",
            ])
            .arg(helper)
            .args(["stdio-report", "--read-line", "--report", report])
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err));
        // SAFETY: this hook runs after fork and before exec, calls only
        // async-signal-safe libc functions, and returns an io::Error on failure.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut runner = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                eprintln!("e2e-helper pty-parent: could not spawn the runner: {err}");
                return ExitCode::from(4);
            }
        };
        if let Err(err) = master
            .write_all(b"pty line\n")
            .and_then(|()| master.flush())
        {
            let _ = runner.kill();
            let _ = runner.wait();
            eprintln!("e2e-helper pty-parent: could not write test input: {err}");
            return ExitCode::from(5);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match runner.try_wait() {
                Ok(Some(status)) => return ExitCode::from(status.code().unwrap_or(1) as u8),
                Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let pid = runner.id().to_string();
                    let snapshot = Command::new("ps")
                        .args(["-o", "pid=,ppid=,pgid=,tpgid=,stat=,command=", "-p", &pid])
                        .output()
                        .map_or_else(
                            |err| format!("ps failed: {err}"),
                            |output| String::from_utf8_lossy(&output.stdout).into_owned(),
                        );
                    eprintln!(
                        "e2e-helper pty-parent: runner {pid} did not exit within 20s; ps: {snapshot:?}"
                    );
                    let _ = runner.kill();
                    let _ = runner.wait();
                    return ExitCode::from(5);
                }
                Err(err) => {
                    eprintln!("e2e-helper pty-parent: could not poll the runner: {err}");
                    return ExitCode::from(5);
                }
            }
        }
    }
}

/// Allocate a fresh Windows console, then launch `processkit-cli run
/// --inherit-stdio` with [`stdio_report`] as its child. This keeps the console
/// assertion independent of whatever redirected handles the CI runner uses.
fn console_parent(args: &[String]) -> ExitCode {
    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("e2e-helper console-parent is Windows-only");
        ExitCode::from(2)
    }
    #[cfg(windows)]
    {
        let Some(runner) = flag_value(args, "--runner") else {
            eprintln!("e2e-helper console-parent: expected --runner <path>");
            return ExitCode::from(2);
        };
        let Some(jsonl) = flag_value(args, "--jsonl") else {
            eprintln!("e2e-helper console-parent: expected --jsonl <path>");
            return ExitCode::from(2);
        };
        let Some(report) = flag_value(args, "--report") else {
            eprintln!("e2e-helper console-parent: expected --report <path>");
            return ExitCode::from(2);
        };
        if let Err(err) = allocate_fresh_console() {
            eprintln!("e2e-helper console-parent: could not allocate a console: {err}");
            return ExitCode::from(3);
        }

        let helper = std::env::current_exe().expect("resolve current e2e helper");
        match Command::new(runner)
            .args(["run", "--inherit-stdio", "--jsonl", jsonl, "--"])
            .arg(helper)
            .args(["stdio-report", "--report", report])
            .status()
        {
            Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(err) => {
                eprintln!("e2e-helper console-parent: could not spawn the runner: {err}");
                ExitCode::from(4)
            }
        }
    }
}

/// Replace inherited/redirected handles with `CONIN$`/`CONOUT$` from a newly
/// allocated console. The short-lived helper deliberately leaves these process
/// standard handles open until exit.
#[cfg(windows)]
fn allocate_fresh_console() -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AllocConsole, FreeConsole, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        SetStdHandle,
    };

    // This helper may inherit a console locally and no console in CI. Detaching
    // first makes both starting states converge on one fresh console object.
    // SAFETY: FreeConsole affects only this dedicated helper process.
    unsafe { FreeConsole() };
    // SAFETY: AllocConsole takes no arguments and initializes a private console
    // for this process; the return value is checked.
    if unsafe { AllocConsole() } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    fn open_console(name: &str, access: u32) -> std::io::Result<HANDLE> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and lives through the call; all optional
        // pointer parameters use their documented null/default form.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(handle)
        }
    }

    let input = open_console("CONIN$", GENERIC_READ | GENERIC_WRITE)?;
    let output = open_console("CONOUT$", GENERIC_READ | GENERIC_WRITE)?;
    // SAFETY: both handles are valid console handles kept open for the remaining
    // lifetime of this helper. SetStdHandle only changes this process's table.
    if unsafe { SetStdHandle(STD_INPUT_HANDLE, input) } == 0
        || unsafe { SetStdHandle(STD_OUTPUT_HANDLE, output) } == 0
        || unsafe { SetStdHandle(STD_ERROR_HANDLE, output) } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
