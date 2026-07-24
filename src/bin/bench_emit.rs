//! Test-support worker for the `bench`-gated through-the-binary benchmarks
//! (`benches/echo_overhead_bench.rs`, `benches/startup_latency_bench.rs`).
//!
//! Gated behind the `bench` Cargo feature (see `Cargo.toml`), like
//! `e2e_helper.rs` is gated behind `e2e` — normal and published builds never
//! include it. It exists so the echo-overhead scenario can drive a precise,
//! cross-platform child: `--bytes N` writes exactly `N` bytes of printable
//! filler to stdout, newline-delimited in fixed-width lines (so the runner's
//! line pump sees realistic line framing rather than one unterminated blob),
//! then exits `0`. A shell builtin (`cmd /c echo`, `yes`) cannot give a byte-
//! exact count portably and would conflate its own interpreter startup cost
//! with the runner's measured overhead.
//!
//! Buffered in one `BufWriter` and written in as few `write_all` calls as
//! practical, so the benchmark measures the runner's pump/echo/capture cost,
//! not this helper's own I/O pattern.

use std::io::{BufWriter, Write};
use std::process::ExitCode;

/// One filler line's payload width (excluding the trailing `\n`), chosen to
/// resemble a typical build-tool log line — long enough that the line count
/// for a multi-MiB run stays in the thousands, not millions.
const LINE_WIDTH: usize = 120;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bytes = u64_flag(&args, "--bytes", 0);
    if let Err(err) = emit(bytes) {
        eprintln!("bench-emit: could not write to stdout: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Write exactly `total` bytes to stdout as `\n`-terminated
/// [`LINE_WIDTH`]-byte lines (a shorter final partial line makes up any
/// remainder), then flush.
fn emit(total: u64) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(64 * 1024, stdout.lock());
    // A fixed, non-whitespace filler byte — content is never inspected by the
    // benchmarks, only its length and framing.
    let line: Vec<u8> = vec![b'x'; LINE_WIDTH];
    let mut written = 0u64;
    while written + (LINE_WIDTH as u64) < total {
        out.write_all(&line)?;
        out.write_all(b"\n")?;
        written += LINE_WIDTH as u64 + 1;
    }
    // Remainder shorter than one full line: pad the line itself, no trailing
    // newline, so the total byte count is exact.
    let remainder = (total - written) as usize;
    if remainder > 0 {
        out.write_all(&line[..remainder])?;
    }
    out.flush()
}

/// The value following `name` in `args`, if present.
fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// A `u64` flag, or `default` when absent or unparseable.
fn u64_flag(args: &[String], name: &str, default: u64) -> u64 {
    flag_value(args, name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
