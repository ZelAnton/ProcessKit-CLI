//! Through-the-binary tests for the `run` subcommand: exit-code fidelity, live
//! stream pass-through with strict separation, the spawn-failure code, timeout
//! and Ctrl-C cancel as distinguishable runner-imposed endings, the `--grace`
//! pause, and kernel-backed teardown of a leaked descendant. These prove behavior
//! the library-level ProcessKit-rs suite cannot: the *binary's* own contracts
//! (`AGENTS.md`, "Testing tiers"). The full end-to-end scenario matrix is a
//! separate task (T-010); this is the base proof through the shipped binary.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{bin, events_path, run, run_with_flags, scratch, shell_inline};
use serde_json::Value;

/// The core rule: a completed run forwards the child's exact code (see
/// `docs/exit-codes.md`). Zero stays zero.
#[test]
fn forwards_a_zero_exit_code() {
    let dir = scratch("exit0");
    let out = run(&dir, &[], shell_inline("exit 0"));
    assert_eq!(out.status.code(), Some(0), "a clean child must exit 0");
}

/// A non-zero child code is forwarded verbatim — not clamped, not aliased onto a
/// runner-own code.
#[test]
fn forwards_a_nonzero_exit_code() {
    let dir = scratch("exit7");
    let out = run(&dir, &[], shell_inline("exit 7"));
    assert_eq!(
        out.status.code(),
        Some(7),
        "the child's code must pass through unchanged"
    );
}

/// Child stdout and stderr are echoed live and stay strictly separated — child
/// stdout to our stdout, child stderr to our stderr — and no runner diagnostic
/// ever leaks into the child's stdout (`AGENTS.md`, "Streams are strictly
/// separated").
#[test]
fn passes_child_streams_through_without_mixing() {
    let dir = scratch("streams");
    let script = if cfg!(windows) {
        "echo OUT&echo ERR 1>&2"
    } else {
        "echo OUT; echo ERR 1>&2"
    };
    let out = run(&dir, &[], shell_inline(script));
    assert!(out.status.success(), "the child exits cleanly");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(stdout.contains("OUT"), "child stdout reaches our stdout");
    assert!(
        !stdout.contains("ERR"),
        "child stderr must not bleed into our stdout: {stdout:?}"
    );
    assert!(stderr.contains("ERR"), "child stderr reaches our stderr");
    assert!(
        !stdout.contains("processkit-cli"),
        "no runner diagnostic may appear on the child's stdout: {stdout:?}"
    );
}

/// `--inherit-stdin` gives the child the runner's input handle without changing
/// the output or lifecycle contracts. The parent pipe makes this deterministic on
/// Windows and Unix while exercising the same inheritance mode a terminal uses.
#[test]
fn inherited_stdin_reaches_the_child_and_preserves_the_terminal_event() {
    let dir = scratch("inherit-stdin");
    let mut runner =
        common::command_with_flags(&dir, &[], &["--inherit-stdin"], stdin_reader_program(&dir))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the runner with a piped stdin");
    let mut stdin = runner
        .stdin
        .take()
        .expect("the runner receives the test pipe");
    stdin
        .write_all(b"inherited line\n")
        .expect("write one line for the child");
    drop(stdin);

    let out = runner
        .wait_with_output()
        .expect("the runner exits after the child reads stdin");
    assert_eq!(out.status.code(), Some(0), "child exit is forwarded");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stdin:inherited line"),
        "the child read the inherited line: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_child_exit_event(&dir);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--stdin-file` keeps input bytes out of the command tail and streams them via
/// ProcessKit, closing the child's stdin once the file reaches EOF.
#[test]
fn stdin_file_reaches_the_child_and_preserves_the_terminal_event() {
    let dir = scratch("stdin-file");
    let input = dir.join("input.txt");
    std::fs::write(&input, b"file line\n").expect("write stdin fixture");
    let input_flag = path_arg(&input);

    let out = run_with_flags(
        &dir,
        &[],
        &["--stdin-file", &input_flag],
        stdin_reader_program(&dir),
    );
    assert_eq!(out.status.code(), Some(0), "child exit is forwarded");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stdin:file line"),
        "the child read the file's line: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_child_exit_event(&dir);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A missing input file fails before the command starts, so the child cannot
/// accidentally run with a different stdin mode than the caller requested.
#[test]
fn missing_stdin_file_is_a_pre_run_setup_failure() {
    let dir = scratch("stdin-file-missing");
    let missing = path_arg(&dir.join("does-not-exist.txt"));

    let out = run_with_flags(
        &dir,
        &[],
        &["--stdin-file", &missing],
        shell_inline("echo child-must-not-start"),
    );
    assert_eq!(out.status.code(), Some(111));
    assert!(out.stdout.is_empty(), "no child output may be forwarded");

    let events = read_run_events(&dir);
    assert!(
        !events.iter().any(|event| event["event"] == "run_started"),
        "the child must not start when stdin setup fails"
    );
    let terminal = events.last().expect("terminal runner_exit event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "setup");
    assert_eq!(terminal["code"], 111);
    assert!(terminal["child_code"].is_null());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A program that cannot be started is a runner-own failure, so the runner exits
/// with the reserved `SPAWN` code (101) and reports the reason on stderr — never
/// on stdout.
#[test]
fn missing_program_uses_the_spawn_code() {
    let dir = scratch("nofile");
    let out = run(&dir, &[], ["processkit_cli_no_such_program_xyz"]);
    assert_eq!(
        out.status.code(),
        Some(101),
        "a spawn failure exits with the reserved SPAWN code"
    );
    assert!(
        out.stdout.is_empty(),
        "a spawn failure writes nothing to the child's stdout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("processkit-cli"),
        "the failure is reported on stderr: {stderr:?}"
    );
}

/// The headline guarantee: after `run` returns, a descendant the child leaked and
/// abandoned does not survive. The child spawns a detached grandchild that
/// appends to a heartbeat file on a ~1s cadence, then the child exits. Once `run`
/// returns, the owning `ProcessGroup` has dropped and reaped the whole tree, so
/// the heartbeat stops: the file's size must not grow any further. This holds
/// regardless of teardown timing — a leaked grandchild would keep appending.
#[test]
fn tears_down_a_leaked_descendant() {
    let dir = scratch("teardown");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_root_script(&dir, &grandchild);

    let program_and_args: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(&root)]
    } else {
        vec!["/bin/sh".into(), path_arg(&root)]
    };

    let out = run(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        program_and_args,
    );
    // The child (root) exits cleanly after launching the grandchild; the runner
    // forwards that 0.
    assert_eq!(out.status.code(), Some(0), "the root child exits cleanly");

    // By the time `run` returned the group had already been torn down, so the
    // grandchild is dead. It must have run at least once first (else the fixture
    // never launched it and the test would prove nothing).
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have started heartbeating before teardown"
    );

    // A leaked grandchild would append several more times in this window; a torn
    // down one cannot grow the file at all.
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a leaked descendant kept heartbeating after run returned — teardown failed \
         (grew from {size_at_return} to {size_later} bytes)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Size of `path` in bytes, or 0 when it does not exist yet.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `--capture-dir` records the child's stdout/stderr to `stdout.log`/`stderr.log`
/// **without** breaking the live echo, keeps the two streams separate, and — the
/// load-bearing property for this task (K-005) — still cannot hang when a leaked
/// descendant keeps an output handle open past the root's exit: the pump drain
/// stays time-bounded, so `run` returns promptly rather than blocking on the
/// grandchild's whole lifetime. The `output_captured` event reports each stream's
/// path, full byte counter, content hash, and an explicit (here `false`) truncation
/// flag.
#[test]
fn capture_records_streams_without_hanging_on_a_leaked_descendant() {
    let dir = scratch("capture");
    let heartbeat = dir.join("heartbeat.txt");
    let capture_dir = dir.join("capture");
    let grandchild = write_grandchild_script(&dir);
    let root = write_capture_root_script(&dir);

    let program_and_args: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(&root)]
    } else {
        vec!["/bin/sh".into(), path_arg(&root)]
    };

    let capture_flag = path_arg(&capture_dir);
    let start = Instant::now();
    let out = run_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--capture-dir", &capture_flag],
        program_and_args,
    );
    let elapsed = start.elapsed();

    // The root echoes and leaks the grandchild, then exits cleanly; the runner
    // forwards that 0 through the capture path.
    assert_eq!(out.status.code(), Some(0), "the root child exits cleanly");

    // No hang: the grandchild holds the child's stdout pipe and lives ~30s, but the
    // bounded pump drain lets `run` return in a small multiple of the ~5s teardown
    // window — nowhere near the grandchild's lifetime.
    assert!(
        elapsed < Duration::from_secs(25),
        "capture must not wait out the leaked descendant: run took {elapsed:?}"
    );

    // Live echo is preserved with capture on: the child's stdout still reaches the
    // runner's stdout, strictly separated from stderr.
    let live_stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        live_stdout.contains("CAPTURED_OUT"),
        "live echo of stdout must survive capture: {live_stdout:?}"
    );
    assert!(
        !live_stdout.contains("CAPTURED_ERR"),
        "child stderr must not bleed into the runner's stdout: {live_stdout:?}"
    );

    // The capture files hold the same output, separated per stream.
    let stdout_log =
        std::fs::read_to_string(capture_dir.join("stdout.log")).expect("stdout.log must exist");
    let stderr_log =
        std::fs::read_to_string(capture_dir.join("stderr.log")).expect("stderr.log must exist");
    assert!(
        stdout_log.contains("CAPTURED_OUT") && !stdout_log.contains("CAPTURED_ERR"),
        "stdout.log captures only stdout: {stdout_log:?}"
    );
    assert!(
        stderr_log.contains("CAPTURED_ERR") && !stderr_log.contains("CAPTURED_OUT"),
        "stderr.log captures only stderr: {stderr_log:?}"
    );

    // The `output_captured` event reports coherent per-stream metadata.
    let events = read_run_events(&dir);
    let captured = events
        .iter()
        .find(|e| e["event"] == "output_captured")
        .expect("an output_captured event when --capture-dir is set");
    let stdout_meta = &captured["stdout"];
    assert!(
        stdout_meta["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("stdout.log")),
        "the event names the stdout capture file: {captured}"
    );
    assert_eq!(
        stdout_meta["bytes"].as_u64(),
        Some(file_len(&capture_dir.join("stdout.log"))),
        "an untruncated stream's byte counter equals its file size"
    );
    assert!(
        is_sha256_hex(&stdout_meta["sha256"]),
        "the stdout capture carries a hex content hash: {captured}"
    );
    assert_eq!(
        stdout_meta["truncated"], false,
        "a small stream is captured in full, not truncated: {captured}"
    );
    assert_eq!(captured["stderr"]["truncated"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Parse the emitted JSONL event stream for `dir`, one object per non-empty line.
fn read_run_events(dir: &Path) -> Vec<Value> {
    let path = events_path(dir);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read events file {}: {err}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("each event line is valid JSON"))
        .collect()
}

/// A natural stdin-consuming run remains a normal child exit: cleanup completes
/// and the terminal event retains the child's code rather than minting a runner code.
fn assert_child_exit_event(dir: &Path) {
    let events = read_run_events(dir);
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "cleanup_finished"),
        "stdin does not bypass containment cleanup: {events:?}"
    );
    let terminal = events.last().expect("a terminal event");
    assert_eq!(terminal["event"], "runner_exit");
    assert_eq!(terminal["source"], "child_exit");
    assert_eq!(terminal["code"], 0);
    assert_eq!(terminal["child_code"], 0);
}

/// A batch file avoids cmd.exe's single-line variable-expansion rules while the
/// POSIX script uses the same one-line input contract.
fn stdin_reader_program(dir: &Path) -> Vec<String> {
    if cfg!(windows) {
        let script = dir.join("read-stdin.bat");
        std::fs::write(
            &script,
            "@echo off\r\nset /p line=\r\necho stdin:%line%\r\n",
        )
        .expect("write Windows stdin reader");
        vec!["cmd".into(), "/c".into(), path_arg(&script)]
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "IFS= read -r line; printf 'stdin:%s\\n' \"$line\"".into(),
        ]
    }
}

/// Whether `v` is a JSON string of 64 lowercase-hex characters (a SHA-256 digest).
fn is_sha256_hex(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Write a root script that echoes a marker to stdout *and* stderr, launches the
/// detached heartbeat grandchild (which keeps the inherited stdout handle open past
/// the root's exit — the leaked-descendant shape), and exits. Used to prove capture
/// records both streams without hanging on the survivor.
fn write_capture_root_script(dir: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = dir.join("capture_root.bat");
        let body = "@echo off\r\n\
             echo CAPTURED_OUT\r\n\
             echo CAPTURED_ERR 1>&2\r\n\
             start \"\" /b \"%GRANDCHILD%\"\r\n";
        std::fs::write(&path, body).expect("write capture_root.bat");
        path
    } else {
        let path = dir.join("capture_root.sh");
        let body = "#!/bin/sh\n\
             echo CAPTURED_OUT\n\
             echo CAPTURED_ERR 1>&2\n\
             sh \"$GRANDCHILD\" &\n\
             exit 0\n";
        std::fs::write(&path, body).expect("write capture_root.sh");
        path
    }
}

/// A program argument as a lossless platform string (paths are never re-parsed by
/// a shell here, so lossy UTF-8 is fine for the temp paths the fixture builds).
fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Write the grandchild script: a bounded heartbeat loop (append, wait ~1s) so a
/// leaked instance keeps growing the file while a reaped one stops. Bounded to
/// ~30 iterations so a teardown regression self-terminates instead of running
/// forever.
fn write_grandchild_script(dir: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = dir.join("grandchild.bat");
        // CRLF and the `do ( … )` block shape are what cmd's batch parser expects.
        let body = "@echo off\r\n\
             for /L %%i in (1,1,30) do (\r\n\
             \x20 echo x>>\"%HB%\"\r\n\
             \x20 ping -n 2 127.0.0.1 >nul\r\n\
             )\r\n";
        std::fs::write(&path, body).expect("write grandchild.bat");
        path
    } else {
        let path = dir.join("grandchild.sh");
        let body = "#!/bin/sh\n\
             i=0\n\
             while [ \"$i\" -lt 30 ]; do\n\
             \x20 printf x >> \"$HB\"\n\
             \x20 sleep 1\n\
             \x20 i=$((i + 1))\n\
             done\n";
        std::fs::write(&path, body).expect("write grandchild.sh");
        path
    }
}

/// Write the root script: launch the grandchild detached (so it outlives the
/// root) and exit immediately, leaving a live descendant behind. The grandchild
/// deliberately keeps the inherited stdout handle, which is exactly the "leaked
/// descendant holds the pipe" shape teardown must still resolve.
fn write_root_script(dir: &Path, grandchild: &Path) -> std::path::PathBuf {
    let _ = grandchild; // path travels via the GRANDCHILD env var, not argv.
    if cfg!(windows) {
        let path = dir.join("root.bat");
        let body = "@echo off\r\nstart \"\" /b \"%GRANDCHILD%\"\r\n";
        std::fs::write(&path, body).expect("write root.bat");
        path
    } else {
        let path = dir.join("root.sh");
        let body = "#!/bin/sh\nsh \"$GRANDCHILD\" &\nexit 0\n";
        std::fs::write(&path, body).expect("write root.sh");
        path
    }
}

/// Write a root script that launches the detached heartbeat grandchild and then
/// *stays alive* (a long sleep), so a runner-imposed ending (a `--timeout` or a
/// `Ctrl-C`) is what stops it — the shape the teardown-on-timeout/cancel proofs
/// need, in contrast to [`write_root_script`]'s immediately-exiting root.
fn write_sleeping_root_script(dir: &Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let path = dir.join("sleeping_root.bat");
        let body = "@echo off\r\n\
             start \"\" /b \"%GRANDCHILD%\"\r\n\
             ping -n 300 127.0.0.1 >nul\r\n";
        std::fs::write(&path, body).expect("write sleeping_root.bat");
        path
    } else {
        let path = dir.join("sleeping_root.sh");
        let body = "#!/bin/sh\nsh \"$GRANDCHILD\" &\nsleep 300\n";
        std::fs::write(&path, body).expect("write sleeping_root.sh");
        path
    }
}

/// A `--timeout` that elapses is a **distinguishable, runner-imposed** ending: the
/// runner exits with the reserved `TIMEOUT` code (106, never the child's own),
/// explains it on stderr, and — the headline guarantee — tears the whole tree
/// down. The child sleeps long past the deadline while a detached grandchild
/// heartbeats; once the runner returns the heartbeat must stop.
#[test]
fn timeout_reports_the_timeout_code_and_tears_down_the_tree() {
    let dir = scratch("timeout");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_sleeping_root_script(&dir);

    let program_and_args: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/c".into(), path_arg(&root)]
    } else {
        vec!["/bin/sh".into(), path_arg(&root)]
    };

    let out = run_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--timeout", "2s"],
        program_and_args,
    );

    // A runner-imposed timeout takes the reserved code, not a forwarded child code.
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout must exit with the reserved TIMEOUT code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("timed out"),
        "the timeout must be explained on stderr: {stderr:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("processkit-cli"),
        "no runner diagnostic may appear on the child's stdout"
    );

    // The grandchild must have heartbeat before teardown (else the fixture proved
    // nothing) and must be gone now: a torn-down tree cannot grow the file.
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have started heartbeating before the timeout"
    );
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a descendant survived the timeout teardown (grew from {size_at_return} to {size_later})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Windows honesty: with no soft-terminate tier in the ProcessKit kernel yet, a
/// timeout on Windows must *say so plainly* — it names the atomic Job Object kill
/// and never claims a graceful soft-terminate was performed (`docs/ROADMAP.md`:
/// "Windows cancellation must report its hard-kill fallback honestly").
#[cfg(windows)]
#[test]
fn windows_timeout_reports_the_hard_kill_fallback_honestly() {
    let dir = scratch("wintimeout");
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "1s"],
        ["cmd", "/c", "ping -n 300 127.0.0.1 >nul"],
    );
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout exits with the TIMEOUT code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Windows"),
        "the degradation is named: {stderr:?}"
    );
    assert!(
        stderr.contains("Job Object"),
        "the atomic kill is named: {stderr:?}"
    );
    assert!(
        stderr.contains("no soft-terminate"),
        "honesty: no soft tier is stated: {stderr:?}"
    );
    assert!(
        !stderr.contains("sent SIGTERM"),
        "must not claim a soft signal was delivered on Windows: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Unix soft path: where a real soft-terminate exists, the timeout message states
/// the `SIGTERM` was sent and the grace was waited — the honest counterpart to the
/// Windows fallback above.
#[cfg(unix)]
#[test]
fn unix_timeout_reports_a_real_soft_signal() {
    let dir = scratch("unixtimeout");
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "1s"],
        shell_inline("sleep 300"),
    );
    assert_eq!(
        out.status.code(),
        Some(106),
        "a timeout exits with the TIMEOUT code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SIGTERM"),
        "the real soft signal is named: {stderr:?}"
    );
    assert!(
        stderr.contains("grace"),
        "the grace window is named: {stderr:?}"
    );
    assert!(
        !stderr.contains("Windows"),
        "the Unix message must not mention the Windows fallback: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--grace` really holds a pause between the soft signal and the hard kill (Unix,
/// where the soft path exists). The child *ignores* `SIGTERM`, so the runner must
/// wait the full grace before the kill-on-drop `SIGKILL`: the run cannot end until
/// roughly `timeout + grace`, well past the deadline alone.
#[cfg(unix)]
#[test]
fn grace_holds_the_pause_before_the_hard_kill() {
    let dir = scratch("grace");
    let start = std::time::Instant::now();
    // Trap (ignore) SIGTERM in the shell; the busy `sleep 1` loop re-arms after the
    // one-shot broadcast kills its in-flight sleep, so the tree outlives the soft
    // signal and only dies at the post-grace SIGKILL.
    let out = run_with_flags(
        &dir,
        &[],
        &["--timeout", "1s", "--grace", "3s"],
        shell_inline("trap '' TERM; while :; do sleep 1; done"),
    );
    let elapsed = start.elapsed();
    assert_eq!(
        out.status.code(),
        Some(106),
        "a SIGTERM-ignoring child is still a timeout, torn down by the hard kill"
    );
    // Deadline alone would end near ~1s; honoring the 3s grace pushes it past ~3.5s.
    assert!(
        elapsed >= Duration::from_millis(3500),
        "grace was not honored: the run ended after {elapsed:?}, expected >= ~3.5s"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `Ctrl-C` mid-run is a **distinguishable** ending: the runner exits with the
/// reserved `CANCELLED` code (107 — not a timeout, not a child code) and tears the
/// tree down. Unix-only: it delivers a real `SIGINT` (the interactive Ctrl-C) to
/// the runner process; an isolated Ctrl-C cannot be sent to a single child on
/// Windows, so that platform is covered by the honest-message and unit tests.
#[cfg(unix)]
#[test]
fn cancel_via_ctrl_c_reports_the_cancel_code_and_tears_down_the_tree() {
    use std::process::Stdio;

    let dir = scratch("cancel");
    let heartbeat = dir.join("heartbeat.txt");
    let grandchild = write_grandchild_script(&dir);
    let root = write_sleeping_root_script(&dir);

    let child = common::command_with_flags(
        &dir,
        &[
            ("HB", heartbeat.as_path()),
            ("GRANDCHILD", grandchild.as_path()),
        ],
        &["--grace", "1s"],
        vec!["/bin/sh".to_string(), path_arg(&root)],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn the runner");

    // Let the grandchild start heartbeating so the SIGINT lands mid-run.
    wait_until(|| file_len(&heartbeat) > 0, Duration::from_secs(10));

    // Deliver the interactive Ctrl-C the runner listens for — to the runner alone
    // (its pid), not a process group, so only the runner sees it.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(rc, 0, "failed to deliver SIGINT to the runner");

    let out = child.wait_with_output().expect("runner did not exit");
    assert_eq!(
        out.status.code(),
        Some(107),
        "a Ctrl-C cancel must exit with the reserved CANCELLED code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cancelled"),
        "the cancel must be explained on stderr: {stderr:?}"
    );

    // The tree must be gone: the heartbeat cannot grow after the runner returned.
    let size_at_return = file_len(&heartbeat);
    assert!(
        size_at_return > 0,
        "the grandchild must have heartbeat before the cancel"
    );
    sleep(Duration::from_secs(3));
    let size_later = file_len(&heartbeat);
    assert_eq!(
        size_later, size_at_return,
        "a descendant survived the cancel teardown (grew from {size_at_return} to {size_later})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Poll `cond` until it holds or `timeout` elapses (then panic). A tiny spin used
/// by the cancel test to wait for the grandchild to come alive.
#[cfg(unix)]
fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < timeout,
            "condition was not met within {timeout:?}"
        );
        sleep(Duration::from_millis(50));
    }
}

/// The binary path is stable — a cheap guard that the fixture points at a real
/// executable before the heavier scenarios run.
#[test]
fn binary_under_test_exists() {
    assert!(
        Path::new(bin()).is_file(),
        "the built binary should exist at {}",
        bin()
    );
}
