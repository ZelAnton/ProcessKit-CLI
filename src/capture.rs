//! Bounded stdout/stderr capture to files, teed *alongside* the runner's echo sink.
//!
//! `--capture-dir <dir>` turns on a per-stream transcript: the child's stdout and
//! stderr are written to `<dir>/stdout.log` and `<dir>/stderr.log`, independent of
//! whether the runner's own echo is live or suppressed (`AGENTS.md`, "Streams are
//! strictly separated"; the task's "don't break live echo"). Capture rides
//! ProcessKit's existing decoded per-stream tee — a [`CaptureTee`] mirrors every
//! byte that line sink observes into a bounded capture file. Live echo is a
//! separate raw tee in `src/run/launch.rs`, so it can forward pipe chunks without
//! paying a write and mutex round-trip per decoded line while this established
//! capture path keeps its byte accounting, hashes, and truncation semantics.
//! `--no-echo` changes only whether the raw sink is installed, never what capture
//! records.
//! The pump's own memory bound is ProcessKit's
//! [`OutputBufferPolicy`](processkit::OutputBufferPolicy): `run` hands the kernel a
//! byte-capped policy so a single never-terminated line cannot grow the pump's
//! in-flight assembly buffer without limit — the runner writes no draining/limiting
//! of its own.
//!
//! For each stream four facts are recorded and surfaced in the JSONL
//! `output_captured` event (see [`crate::events`] and `docs/schema.md`): the full
//! byte counter (every decoded byte the stream produced), the SHA-256 of the bytes
//! actually written to the file, an **explicit** truncation flag — set when the
//! stream outran the per-stream file ceiling, never inferred from the file's size —
//! and an **explicit** write-error flag — set when a file write failed mid-stream so
//! capture stopped touching a broken file before the stream ended. The two flags are
//! independent conditions (a stream can be both ceiling-truncated and write-errored),
//! and each is reported directly rather than inferred: a consumer tells "captured in
//! full" (`truncated` and `write_error` both false) from "clipped at the limit" from
//! "cut short by a disk write error" on the flags alone, and can verify the file it
//! holds against the recorded digest (which always covers exactly the bytes that
//! reached disk).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::AsyncWrite;
use tokio::sync::Notify;

use crate::events::CaptureInfo;
use crate::hash::Sha256;

/// Default per-stream ceiling on bytes written to a capture file, used when
/// `run`'s `--capture-max-bytes` is not given. Output past the (possibly
/// user-configured) ceiling is counted (so the full byte counter stays honest)
/// but not written, and the stream's `truncated` flag is set. Bounds the on-disk
/// transcript so a runaway child cannot fill the disk through the capture files;
/// the live echo is never bounded. `pub` so `src/cli.rs`'s `--capture-max-bytes`
/// docstring and `src/run/launch.rs`'s default-resolution call site can both cite this
/// single constant instead of duplicating the value (T-181).
pub const CAPTURE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The byte ceiling handed to ProcessKit's [`OutputBufferPolicy`] for the pump's
/// **in-flight** line assembly. Deliberately far larger than [`CAPTURE_MAX_BYTES`]
/// so every realistically-sized line still reaches the tee (and thus the capture
/// file and the live echo); it only bounds the pathological single never-terminated
/// line the kernel would otherwise assemble whole. This is the memory bound the
/// task requires be taken from the kernel policy rather than hand-rolled.
///
/// **Deliberately independent of `--capture-max-bytes` (T-181), not derived from
/// it.** The two ceilings bound different things: this one bounds the pump's
/// *in-memory* assembly of one still-unterminated line (a pathological-input
/// concern, sized once for the whole binary), while `--capture-max-bytes` bounds
/// the *on-disk* per-stream transcript size (an operator's disk-budget choice,
/// per invocation). Deriving this from `--capture-max-bytes` would tie the
/// in-flight memory ceiling to an unrelated on-disk budget — e.g. an operator
/// asking for a deliberately small `--capture-max-bytes` (a stricter disk cap)
/// would have no reason to also want a smaller in-flight line-assembly buffer,
/// and the reverse (a large `--capture-max-bytes` for a long build log) would
/// otherwise inflate the in-flight buffer far past what any real line needs. Kept
/// as its own constant instead.
///
/// **Byte accounting under ProcessKit 3 (verified, not assumed).** ProcessKit 3
/// made its *cumulative* output-byte counter raw — the bytes read from the pipe,
/// including line terminators and pre-decode invalid UTF-8 — by accumulating it
/// at the read boundary (`seen_bytes`) instead of per decoded line. That counter
/// feeds exactly two things, and this runner uses **neither**: the fail-loud
/// `OverflowMode::Error` ceiling (`ErrorReason::OutputTooLarge`), and the
/// `RunningProcess::stdout_bytes_seen`/`stderr_bytes_seen` readbacks. `run` hands
/// the pump `OutputBufferPolicy::bounded(0)` — a *drop* mode — whose retention and
/// per-line cap are measured on **decoded** line content, and so is the in-flight
/// assembly cap this constant sets: the pump weighs the cap against the decoded
/// text of the still-unterminated line, minus a lone trailing `\r` (which may yet
/// turn out to be half of a CRLF terminator). That arithmetic is byte-identical to
/// 2.x — it is *not* the accounting that went raw. Two consequences to keep
/// straight when reasoning about this ceiling: invalid UTF-8 decodes to U+FFFD
/// (3 bytes per replacement), so the measured length can exceed the raw bytes read
/// rather than trail them; and the cap is rechecked once per read, after a whole
/// 8 KiB chunk has decoded in, so the real memory bound is about `cap` plus one
/// read chunk, not exactly `cap`.
/// So neither ceiling shifted with the upgrade and none of the runner's own
/// counting had to move: [`CaptureTee`]'s per-stream `--capture-max-bytes` budget
/// stays what it always was — the bytes this runner writes to the transcript file
/// (see the module docs above), counted here, never read back off the kernel.
pub const CAPTURE_INFLIGHT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// The first capture stream that exceeded its configured per-stream ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureOverflow {
    pub stream: &'static str,
    pub max_bytes: u64,
}

/// A lock-free, one-shot bridge from the synchronous pump sink to the runner's
/// asynchronous ending race. The atomic value is authoritative; `Notify` only
/// wakes the single waiter, so notifications cannot be lost if overflow happens
/// just before the future starts polling.
struct OverflowSignal {
    stream: AtomicU8,
    max_bytes: u64,
    notify: Notify,
}

impl OverflowSignal {
    fn new(max_bytes: u64) -> Self {
        Self {
            stream: AtomicU8::new(0),
            max_bytes,
            notify: Notify::new(),
        }
    }

    fn trip(&self, stream: u8) {
        if self
            .stream
            .compare_exchange(0, stream, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.notify.notify_one();
        }
    }

    async fn wait(&self) -> CaptureOverflow {
        loop {
            let notified = self.notify.notified();
            let stream = self.stream.load(Ordering::Acquire);
            if stream != 0 {
                return CaptureOverflow {
                    stream: if stream == 1 { "stdout" } else { "stderr" },
                    max_bytes: self.max_bytes,
                };
            }
            notified.await;
        }
    }
}

/// One stream's capture file plus its running metadata. Behind an `Arc<Mutex<…>>`
/// so the [`CaptureTee`] on ProcessKit's pump task and the runner reading the final
/// metadata share one state; the lock is only ever held for a synchronous file
/// write, never across an `.await`.
///
/// `pub` (and likewise [`new`](Self::new)/[`absorb`](Self::absorb) below) so the
/// `benches/capture_bench.rs` microbenchmark (T-187) can drive the counting/
/// write/hash path directly, without an async runtime or the tee's `AsyncWrite`
/// plumbing in the timed loop — matching the crate's documented "future
/// benchmarks reach internal primitives directly" design (`docs/architecture.md`,
/// "Target structure"). Not a stability signal: this module stays
/// `#[doc(hidden)]` and the library carries no semver guarantee (`src/lib.rs`).
pub struct StreamCapture {
    file: std::fs::File,
    path: PathBuf,
    /// This stream's per-stream ceiling — `CAPTURE_MAX_BYTES` unless
    /// `--capture-max-bytes` overrode it (see [`Capture::create`]).
    max_bytes: u64,
    /// Every decoded byte the stream produced — the "full byte counter", which
    /// exceeds the file size once the stream is truncated.
    seen: u64,
    /// Bytes actually written to the file (`= min(seen, max_bytes)` while writes
    /// succeed) — the length the SHA-256 covers.
    written: u64,
    /// Running digest of the bytes written to the file.
    hasher: Sha256,
    /// Set once `seen` exceeds the ceiling: an explicit signal, not a size compare.
    truncated: bool,
    /// Latched on the first file write error so we stop touching a broken file
    /// (best-effort: capture never aborts the run). Surfaced in the finalized
    /// [`CaptureInfo`] as its own explicit flag, so a consumer learns the on-disk
    /// transcript is short of the byte counter without comparing sizes.
    write_error: bool,
    /// Present only for a run-owned capture; direct benchmark/unit-test instances
    /// keep their original standalone behavior.
    overflow_signal: Option<(Arc<OverflowSignal>, u8)>,
}

impl StreamCapture {
    /// `max_bytes` is this stream's per-stream ceiling — `CAPTURE_MAX_BYTES`
    /// unless `--capture-max-bytes` (T-181) overrode it.
    pub fn new(path: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        let file = std::fs::File::create(&path)?;
        Ok(Self {
            file,
            path,
            max_bytes,
            seen: 0,
            written: 0,
            hasher: Sha256::new(),
            truncated: false,
            write_error: false,
            overflow_signal: None,
        })
    }

    fn with_overflow_signal(mut self, signal: Arc<OverflowSignal>, stream: u8) -> Self {
        self.overflow_signal = Some((signal, stream));
        self
    }

    /// Fold `bytes` (already echoed live) into the capture: count them, write the
    /// portion that fits under the ceiling, hash exactly what was written, and flag
    /// truncation once the stream outruns the ceiling.
    pub fn absorb(&mut self, bytes: &[u8]) {
        self.seen = self.seen.saturating_add(bytes.len() as u64);
        if !self.write_error && self.written < self.max_bytes {
            // Saturate rather than truncate the `u64 -> usize` cast: on a 32-bit
            // target with a `--capture-max-bytes` ceiling above `usize::MAX`
            // (the flag accepts any `u64`, e.g. `8g`), an `as usize` cast would
            // wrap the remaining-room value modulo 2^32, making `room` (and thus
            // `take`) hit zero far short of the real ceiling — bytes would then
            // silently stop reaching disk without `truncated` or `write_error`
            // being set, breaking the "an incomplete capture is always visible
            // in the flags" invariant. On every 64-bit target in the release
            // matrix today this is a no-op (the subtraction always fits).
            let room = usize::try_from(self.max_bytes - self.written).unwrap_or(usize::MAX);
            let take = room.min(bytes.len());
            if write_and_account(
                &mut self.file,
                &bytes[..take],
                &mut self.hasher,
                &mut self.written,
            )
            .is_err()
            {
                // A file write failure disables further capture for this stream but
                // never disturbs the live echo or the run — the recorded digest and
                // byte count then reflect what reached disk, including any prefix
                // accepted before the failure.
                self.write_error = true;
            }
        }
        if self.seen > self.max_bytes && !self.truncated {
            self.truncated = true;
            if let Some((signal, stream)) = &self.overflow_signal {
                signal.trip(*stream);
            }
        }
    }

    /// Flush the file (best-effort) and snapshot the metadata. The digest is taken
    /// from a clone so a later (idempotent) call would still succeed.
    fn info(&mut self) -> CaptureInfo {
        let _ = self.file.flush();
        CaptureInfo::new(
            self.path.to_string_lossy().into_owned(),
            self.seen,
            self.hasher.clone().finalize_hex(),
            self.truncated,
            self.write_error,
        )
    }
}

/// Write all of `bytes`, accounting for every successful partial write before a
/// later error. `std::io::Write::write_all` does not report how much it wrote when it
/// returns `Err`, so using it here would make the digest and `written` lag behind the
/// actual capture file after a mid-buffer failure.
fn write_and_account(
    writer: &mut impl std::io::Write,
    mut bytes: &[u8],
    hasher: &mut Sha256,
    written: &mut u64,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write the complete capture buffer",
                ));
            }
            Ok(count) => {
                let accepted = &bytes[..count];
                hasher.update(accepted);
                *written += count as u64;
                bytes = &bytes[count..];
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

type Shared = Arc<Mutex<StreamCapture>>;

/// A run's capture: the two per-stream files and their shared metadata. The runner
/// builds one when `--capture-dir` is set, hands each stream's [`CaptureTee`] to
/// the matching decoded `stdout_tee`/`stderr_tee`, keeps this handle, and reads
/// [`finalize`](Self::finalize) once the run has ended.
pub struct Capture {
    stdout: Shared,
    stderr: Shared,
    overflow: Arc<OverflowSignal>,
}

impl Capture {
    /// Open (creating the directory and truncating the two files) the capture for
    /// `dir`, applying `max_bytes` as **both** streams' per-stream ceiling.
    /// Fails closed — like the `--jsonl` file, a capture the operator asked for
    /// but that cannot be created is reported *before* the child is spawned, never
    /// silently dropped.
    ///
    /// Callers pass [`CAPTURE_MAX_BYTES`] unless `run`'s `--capture-max-bytes`
    /// overrode it (see `src/run/launch.rs`), so a bare `run --capture-dir` (no
    /// `--capture-max-bytes`) is byte-for-byte the same ceiling as before T-181.
    pub fn create(dir: &Path, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let overflow = Arc::new(OverflowSignal::new(max_bytes));
        let stdout = Arc::new(Mutex::new(
            StreamCapture::new(dir.join("stdout.log"), max_bytes)?
                .with_overflow_signal(overflow.clone(), 1),
        ));
        let stderr = Arc::new(Mutex::new(
            StreamCapture::new(dir.join("stderr.log"), max_bytes)?
                .with_overflow_signal(overflow.clone(), 2),
        ));
        Ok(Self {
            stdout,
            stderr,
            overflow,
        })
    }

    /// The decoded tee sink for stdout. `echo` is normally a sink because live
    /// passthrough uses ProcessKit's independent raw tee; keeping it generic makes
    /// the capture primitive useful in isolation and preserves its tested broken-
    /// echo behavior.
    pub fn stdout_tee<W: AsyncWrite + Unpin>(&self, echo: W) -> CaptureTee<W> {
        CaptureTee::new(echo, self.stdout.clone())
    }

    /// The tee sink for stderr — see [`stdout_tee`](Self::stdout_tee).
    pub fn stderr_tee<W: AsyncWrite + Unpin>(&self, echo: W) -> CaptureTee<W> {
        CaptureTee::new(echo, self.stderr.clone())
    }

    /// Finalize both streams (flush, snapshot counters/digests) for the
    /// `output_captured` event. Called once the run has ended and the pumps have
    /// settled; on a forced ending (timeout/cancel) the pumps were aborted, so the
    /// metadata honestly reflects the partial transcript captured before teardown.
    pub fn finalize(&self) -> (CaptureInfo, CaptureInfo) {
        (info_of(&self.stdout), info_of(&self.stderr))
    }

    /// Wait until either stream first exceeds the configured ceiling.
    pub async fn overflowed(&self) -> CaptureOverflow {
        self.overflow.wait().await
    }
}

/// Snapshot one stream's metadata, tolerating a poisoned lock (a pump task that
/// panicked mid-write) by reporting an empty, honestly-truncated placeholder rather
/// than propagating the panic into the runner's terminal reporting.
fn info_of(shared: &Shared) -> CaptureInfo {
    match shared.lock() {
        Ok(mut guard) => guard.info(),
        Err(poisoned) => poisoned.into_inner().info(),
    }
}

/// A per-stream decoded tee that writes each byte to its downstream sink and mirrors
/// it into a bounded capture file. Handed to `Command::stdout_tee`/`stderr_tee`;
/// ProcessKit's line pump drives it, awaiting each write. The production downstream
/// is a sink; live passthrough is independently handled by the raw tee.
///
/// The capture mirrors precisely the bytes the echo accepted, so the file can never
/// double-count or lose the tail of a partial write. If the echo sink ever errors
/// or reports zero progress for a non-empty write (e.g. the runner's own stdout was
/// closed), capture continues to the file alone — a broken live echo must not cost
/// the transcript, and, critically, an error returned from a tee would disable it
/// for the rest of the run.
pub struct CaptureTee<W> {
    echo: W,
    /// Latched once the echo sink errors or stops progressing: thereafter bytes go
    /// only to the file.
    echo_broken: bool,
    shared: Shared,
}

impl<W: AsyncWrite + Unpin> CaptureTee<W> {
    fn new(echo: W, shared: Shared) -> Self {
        Self {
            echo,
            echo_broken: false,
            shared,
        }
    }

    /// Mirror `bytes` into the capture file (best-effort; a poisoned lock is
    /// skipped). The lock is held only for this synchronous write, never across an
    /// `.await`.
    fn absorb(&self, bytes: &[u8]) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.absorb(bytes);
        }
    }

    /// Flush the capture file (best-effort).
    fn flush_capture(&self) {
        if let Ok(mut guard) = self.shared.lock() {
            let _ = guard.file.flush();
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CaptureTee<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        // Echo already gone: accept everything and capture it to the file alone.
        if this.echo_broken {
            this.absorb(buf);
            return Poll::Ready(Ok(buf.len()));
        }
        match Pin::new(&mut this.echo).poll_write(cx, buf) {
            // AsyncWrite reserves `Ok(0)` on a non-empty buffer for a sink that can
            // no longer accept bytes. Treat it like a broken echo so `write_all`
            // cannot turn a harmless display failure into `WriteZero` and disable
            // capture for the remainder of the child stream.
            Poll::Ready(Ok(0)) if !buf.is_empty() => {
                this.echo_broken = true;
                this.absorb(buf);
                Poll::Ready(Ok(buf.len()))
            }
            // Mirror *exactly* the bytes the echo took; the pump re-offers the tail
            // on the next poll, which we mirror then — no loss, no duplication.
            Poll::Ready(Ok(n)) => {
                this.absorb(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            // Live echo failed: stop echoing, keep capturing, and never surface the
            // error (which would disable the whole tee, capture included).
            Poll::Ready(Err(_)) => {
                this.echo_broken = true;
                this.absorb(buf);
                Poll::Ready(Ok(buf.len()))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.flush_capture();
        if this.echo_broken {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut this.echo).poll_flush(cx) {
            Poll::Ready(Err(_)) => {
                this.echo_broken = true;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.flush_capture();
        if this.echo_broken {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut this.echo).poll_shutdown(cx) {
            Poll::Ready(Err(_)) => {
                this.echo_broken = true;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// The shared "last observed output" timestamp backing `run`'s `--idle-timeout`
/// deadline.
///
/// Every output chunk the runner sees a child produce re-arms the idle window by
/// stamping this clock with the current instant (see [`IdleActivityTee`]); the
/// idle-deadline future in `src/run/signals.rs` reads it to decide whether a *full* idle
/// window has elapsed with **no** output. There is exactly one clock per run, so
/// both output paths — the default echo and the `--capture-dir` tee — re-arm the
/// same timer rather than two independent ones (a requirement of T-182: an idle
/// expiry must mean the same thing in either mode).
///
/// Cheap to [`clone`](Clone) (an `Arc`), so the output sink(s) and the deadline
/// future all share one timestamp. The lock is only ever held for a single
/// timestamp read or write, never across an `.await`, so a poisoned lock (which a
/// panicking holder cannot produce here, as no user code runs under it) is
/// tolerated by reading through it rather than propagating.
#[derive(Clone)]
pub(crate) struct IdleClock {
    last_activity: Arc<Mutex<Instant>>,
}

impl Default for IdleClock {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleClock {
    /// A fresh clock whose window starts now.
    pub(crate) fn new() -> Self {
        Self {
            last_activity: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Record output activity now, re-arming the idle window. Called on every
    /// non-empty write the child's output passes through (see [`IdleActivityTee`]).
    pub(crate) fn touch(&self) {
        let now = Instant::now();
        match self.last_activity.lock() {
            Ok(mut guard) => *guard = now,
            Err(poisoned) => *poisoned.into_inner() = now,
        }
    }

    /// Re-arm the window's start to now. A semantic alias for [`touch`](Self::touch),
    /// called once at the start of the run's race so the first idle window is
    /// measured from there rather than from whenever the clock was constructed.
    pub(crate) fn reset(&self) {
        self.touch();
    }

    /// The idle time still left before the window elapses, given `idle` as the full
    /// window: `idle - (now - last_activity)`, saturating at zero once the window
    /// has been exceeded. Saturating throughout (no `Instant` arithmetic that could
    /// overflow), so an astronomically large `--idle-timeout` is handled the same
    /// way `tokio::time::sleep` caps one, never by panicking.
    pub(crate) fn remaining(&self, idle: Duration) -> Duration {
        let last = match self.last_activity.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        idle.saturating_sub(last.elapsed())
    }

    /// Wrap `inner` so every chunk written through it re-arms this clock. Used to
    /// wrap the outermost output sink on both the default-echo and `--capture-dir`
    /// tee paths (see `src/run/launch.rs`).
    pub(crate) fn tee<W>(&self, inner: W) -> IdleActivityTee<W> {
        IdleActivityTee {
            inner,
            clock: self.clone(),
        }
    }
}

/// An output sink wrapper that re-arms the `--idle-timeout` window on every chunk
/// of the child's output it forwards.
///
/// It sits **outermost** on the runner's output tee — ProcessKit's pump writes into
/// it first — so it observes every byte in *both* output paths (the default echo and
/// the `--capture-dir` tee it may wrap) and re-arms one shared [`IdleClock`], never
/// two independent timers. It only forwards to the inner sink and stamps the clock:
/// it changes no byte, buffers nothing, and swallows no error, so the live echo and
/// any capture transcript are exactly what they would be without it. Activity is
/// recorded for the bytes the inner sink *accepted* (a partial write re-offers its
/// tail on the next poll, re-arming then too), so a re-arm always reflects real
/// forward progress of the child's output.
pub(crate) struct IdleActivityTee<W> {
    inner: W,
    clock: IdleClock,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for IdleActivityTee<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        // Re-arm on observed forward progress: a chunk the inner sink actually
        // accepted is a chunk of the child's output the runner just saw.
        if let Poll::Ready(Ok(n)) = &poll
            && *n > 0
        {
            this.clock.touch();
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    struct ZeroThenPanicEcho {
        polled: bool,
    }

    impl AsyncWrite for ZeroThenPanicEcho {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            assert!(
                !self.polled,
                "a zero-progress echo must be latched as broken"
            );
            self.polled = true;
            Poll::Ready(Ok(0))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Drive a stream's capture directly (bypassing the async tee) to exercise the
    /// counting / ceiling / hashing logic without a live process, at the default
    /// (`CAPTURE_MAX_BYTES`) ceiling.
    fn drive(path: PathBuf, chunks: &[&[u8]]) -> StreamCapture {
        drive_with_ceiling(path, CAPTURE_MAX_BYTES, chunks)
    }

    /// Like [`drive`], but at an explicit per-stream ceiling — exercises the
    /// `--capture-max-bytes`-configured path (T-181).
    fn drive_with_ceiling(path: PathBuf, max_bytes: u64, chunks: &[&[u8]]) -> StreamCapture {
        let mut cap = StreamCapture::new(path, max_bytes).expect("create capture file");
        for chunk in chunks {
            cap.absorb(chunk);
        }
        cap
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "processkit-cli-capture-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join(format!("{name}.log"))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zero_progress_echo_is_latched_while_capture_continues() {
        let path = temp_path("zero-progress-echo");
        let shared = Arc::new(Mutex::new(
            StreamCapture::new(path.clone(), CAPTURE_MAX_BYTES).expect("create capture file"),
        ));
        let mut tee = CaptureTee::new(ZeroThenPanicEcho { polled: false }, shared.clone());

        tee.write_all(b"first").await.expect("capture first chunk");
        tee.write_all(b"second")
            .await
            .expect("capture after echo is latched broken");
        tee.flush().await.expect("flush capture");
        drop(tee);

        let mut guard = shared.lock().expect("capture lock");
        let info = guard.info();
        assert_eq!(info.bytes(), 11);
        assert!(!info.truncated());
        assert!(!info.write_error());
        assert_eq!(std::fs::read(path).unwrap(), b"firstsecond");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overflow_signal_is_one_shot_and_not_lost_before_waiting() {
        let root = temp_path("overflow-signal");
        let dir = root
            .parent()
            .expect("scratch parent")
            .join("overflow-signal-dir");
        let capture = Capture::create(&dir, 4).expect("create capture");

        capture.stderr.lock().expect("stderr lock").absorb(b"12345");
        capture
            .stdout
            .lock()
            .expect("stdout lock")
            .absorb(b"abcdef");

        let overflow = tokio::time::timeout(Duration::from_millis(100), capture.overflowed())
            .await
            .expect("a pre-existing overflow must wake a later waiter");
        assert_eq!(
            overflow,
            CaptureOverflow {
                stream: "stderr",
                max_bytes: 4,
            },
            "the first stream to cross the ceiling wins permanently"
        );
    }

    #[test]
    fn untruncated_capture_counts_hashes_and_writes_every_byte() {
        let path = temp_path("small");
        let mut cap = drive(path.clone(), &[b"hello ", b"world"]);
        let info = cap.info();
        assert_eq!(info.bytes(), 11, "the full byte counter sums every byte");
        assert!(
            !info.truncated(),
            "output under the ceiling is not truncated"
        );
        assert_eq!(
            info.sha256(),
            crate::hash::sha256_hex(b"hello world"),
            "the digest covers exactly the captured bytes"
        );
        // The file on disk matches what was hashed.
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
    }

    #[test]
    fn ceiling_truncates_the_file_but_the_counter_stays_full() {
        let path = temp_path("truncated");
        // One byte over the ceiling, delivered across the boundary in two chunks.
        let head = vec![b'a'; CAPTURE_MAX_BYTES as usize];
        let mut cap = drive(path.clone(), &[&head, b"Z"]);
        let info = cap.info();
        assert_eq!(
            info.bytes(),
            CAPTURE_MAX_BYTES + 1,
            "the counter reflects every produced byte, past the ceiling"
        );
        assert!(
            info.truncated(),
            "crossing the ceiling sets the explicit flag"
        );
        // The file holds exactly the ceiling's worth, and the digest matches it —
        // the trailing 'Z' was counted but not written.
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk.len() as u64, CAPTURE_MAX_BYTES);
        assert_eq!(info.sha256(), crate::hash::sha256_hex(&head));
    }

    #[test]
    fn exactly_at_the_ceiling_is_not_truncated() {
        let path = temp_path("exact");
        let full = vec![b'x'; CAPTURE_MAX_BYTES as usize];
        let mut cap = drive(path, &[&full]);
        let info = cap.info();
        assert_eq!(info.bytes(), CAPTURE_MAX_BYTES);
        assert!(
            !info.truncated(),
            "seen == ceiling is complete, not truncated"
        );
    }

    #[test]
    fn a_custom_ceiling_truncates_at_the_configured_value_not_the_default() {
        // `--capture-max-bytes` (T-181): a ceiling well below `CAPTURE_MAX_BYTES`
        // must clip there, not at the default 8 MiB.
        let custom_ceiling = 16u64;
        assert!(
            custom_ceiling < CAPTURE_MAX_BYTES,
            "the custom ceiling must be smaller than the default to prove it, not the \
             default, governs the clip"
        );
        let path = temp_path("custom-ceiling");
        let head = vec![b'a'; custom_ceiling as usize];
        let mut cap = drive_with_ceiling(path.clone(), custom_ceiling, &[&head, b"overflow"]);
        let info = cap.info();
        assert_eq!(
            info.bytes(),
            custom_ceiling + "overflow".len() as u64,
            "the full byte counter still sums every produced byte"
        );
        assert!(
            info.truncated(),
            "crossing the configured (not the default) ceiling sets the flag"
        );
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(
            on_disk.len() as u64,
            custom_ceiling,
            "the file holds exactly the configured ceiling's worth, not the default's"
        );
        assert_eq!(info.sha256(), crate::hash::sha256_hex(&head));
    }

    #[test]
    fn empty_stream_is_the_empty_hash_and_untruncated() {
        let path = temp_path("empty");
        let mut cap = drive(path, &[]);
        let info = cap.info();
        assert_eq!(info.bytes(), 0);
        assert!(!info.truncated());
        assert!(!info.write_error());
        assert_eq!(info.sha256(), crate::hash::sha256_hex(b""));
    }

    #[test]
    fn a_write_error_is_surfaced_and_is_not_a_ceiling_truncation() {
        let path = temp_path("write-error");
        let mut cap =
            StreamCapture::new(path.clone(), CAPTURE_MAX_BYTES).expect("create capture file");
        // Force a deterministic write failure on every platform: swap the writable
        // handle for a read-only one on the same file, so the next `write_all`
        // returns `Err` (EBADF / ERROR_ACCESS_DENIED) without depending on an
        // OS-specific error kind or a mid-stream disk-full condition.
        cap.file = std::fs::File::open(&path).expect("reopen the capture file read-only");
        cap.absorb(b"hello");
        let info = cap.info();
        assert!(
            info.write_error(),
            "a mid-stream file write failure is surfaced as an explicit flag"
        );
        assert!(
            !info.truncated(),
            "a write error is a distinct condition from a ceiling truncation"
        );
        assert_eq!(
            info.bytes(),
            5,
            "the full byte counter still sums every produced byte, past the write error"
        );
        // Nothing reached disk before the latch, so the digest is the empty-input
        // hash and the file is empty — the recorded digest covers exactly what was
        // written, and `bytes` (5) exceeds the file's size (0) as the flag warns.
        assert_eq!(info.sha256(), crate::hash::sha256_hex(b""));
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn a_partial_write_before_an_error_is_included_in_the_digest_and_count() {
        struct PartialThenError {
            accepted: Vec<u8>,
            first_write: usize,
        }

        impl std::io::Write for PartialThenError {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.accepted.is_empty() {
                    let count = self.first_write.min(bytes.len());
                    self.accepted.extend_from_slice(&bytes[..count]);
                    Ok(count)
                } else {
                    Err(std::io::Error::other(
                        "injected failure after a partial write",
                    ))
                }
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut writer = PartialThenError {
            accepted: Vec::new(),
            first_write: 3,
        };
        let mut hasher = Sha256::new();
        let mut written = 0;

        let result = write_and_account(&mut writer, b"abcdef", &mut hasher, &mut written);

        assert!(result.is_err(), "the injected second write fails");
        assert_eq!(writer.accepted, b"abc", "three bytes reached the writer");
        assert_eq!(written, 3, "the count includes those accepted bytes");
        assert_eq!(
            hasher.finalize_hex(),
            crate::hash::sha256_hex(b"abc"),
            "the digest covers exactly the accepted prefix"
        );
    }

    /// A fresh `touch` re-arms (nearly) the whole idle window: right after it, the
    /// remaining time is close to the full window. Generous tolerance (a 10s window,
    /// asserting >9s remains) keeps this robust against scheduling jitter.
    #[test]
    fn idle_clock_touch_rearms_the_window() {
        let clock = IdleClock::new();
        let idle = Duration::from_secs(10);
        std::thread::sleep(Duration::from_millis(20));
        clock.touch();
        let remaining = clock.remaining(idle);
        assert!(
            remaining > Duration::from_secs(9),
            "a fresh touch re-arms nearly the full window: {remaining:?}"
        );
        assert!(
            remaining <= idle,
            "the remaining window never exceeds the configured idle: {remaining:?}"
        );
    }

    /// Once the idle window has been exceeded with no touch, `remaining` saturates at
    /// zero — the signal the idle-deadline future in `run` uses to fire.
    #[test]
    fn idle_clock_remaining_saturates_to_zero_past_the_window() {
        let clock = IdleClock::new();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            clock.remaining(Duration::from_millis(1)),
            Duration::ZERO,
            "an elapsed window leaves no remaining idle time"
        );
    }
}
