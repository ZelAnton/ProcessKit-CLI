//! Bounded stdout/stderr capture to files, teed *alongside* the live echo.
//!
//! `--capture-dir <dir>` turns on a per-stream transcript: the child's stdout and
//! stderr are written to `<dir>/stdout.log` and `<dir>/stderr.log` while the live
//! echo to the runner's own stdout/stderr continues unchanged (`AGENTS.md`,
//! "Streams are strictly separated"; the task's "don't break live echo"). Capture
//! rides ProcessKit's existing per-stream tee — a [`CaptureTee`] wraps the same
//! `tokio::io::stdout()`/`stderr()` sink `run` already tees to and mirrors every
//! echoed byte into a bounded capture file — so no second output-reading path is
//! invented and the child's back-pressure is exactly the live echo's (the tee is
//! awaited on ProcessKit's line pump). The pump's own memory bound is ProcessKit's
//! [`OutputBufferPolicy`](processkit::OutputBufferPolicy): `run` hands the kernel a
//! byte-capped policy so a single never-terminated line cannot grow the pump's
//! in-flight assembly buffer without limit — the runner writes no draining/limiting
//! of its own.
//!
//! For each stream three facts are recorded and surfaced in the JSONL
//! `output_captured` event (see [`crate::events`] and `docs/schema.md`): the full
//! byte counter (every decoded byte the stream produced), the SHA-256 of the bytes
//! actually written to the file, and an **explicit** truncation flag — set when the
//! stream outran the per-stream file ceiling, never inferred from the file's size.
//! A consumer therefore tells "captured in full" from "clipped at the limit" from
//! the flag alone, and can verify the file it holds against the recorded digest.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

use crate::events::CaptureInfo;
use crate::hash::Sha256;

/// Per-stream ceiling on bytes written to a capture file. Output past it is
/// counted (so the full byte counter stays honest) but not written, and the
/// stream's `truncated` flag is set. Bounds the on-disk transcript so a runaway
/// child cannot fill the disk through the capture files; the live echo is never
/// bounded. Not currently configurable — a `--capture-max-bytes` knob is a
/// possible future addition.
const CAPTURE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The byte ceiling handed to ProcessKit's [`OutputBufferPolicy`] for the pump's
/// **in-flight** line assembly. Deliberately far larger than [`CAPTURE_MAX_BYTES`]
/// so every realistically-sized line still reaches the tee (and thus the capture
/// file and the live echo); it only bounds the pathological single never-terminated
/// line the kernel would otherwise assemble whole. This is the memory bound the
/// task requires be taken from the kernel policy rather than hand-rolled.
pub const CAPTURE_INFLIGHT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// One stream's capture file plus its running metadata. Behind an `Arc<Mutex<…>>`
/// so the [`CaptureTee`] on ProcessKit's pump task and the runner reading the final
/// metadata share one state; the lock is only ever held for a synchronous file
/// write, never across an `.await`.
struct StreamCapture {
    file: std::fs::File,
    path: PathBuf,
    /// Every decoded byte the stream produced — the "full byte counter", which
    /// exceeds the file size once the stream is truncated.
    seen: u64,
    /// Bytes actually written to the file (`= min(seen, CAPTURE_MAX_BYTES)` while
    /// writes succeed) — the length the SHA-256 covers.
    written: u64,
    /// Running digest of the bytes written to the file.
    hasher: Sha256,
    /// Set once `seen` exceeds the ceiling: an explicit signal, not a size compare.
    truncated: bool,
    /// Latched on the first file write error so we stop touching a broken file
    /// (best-effort: capture never aborts the run).
    write_error: bool,
}

impl StreamCapture {
    fn new(path: PathBuf) -> std::io::Result<Self> {
        let file = std::fs::File::create(&path)?;
        Ok(Self {
            file,
            path,
            seen: 0,
            written: 0,
            hasher: Sha256::new(),
            truncated: false,
            write_error: false,
        })
    }

    /// Fold `bytes` (already echoed live) into the capture: count them, write the
    /// portion that fits under the ceiling, hash exactly what was written, and flag
    /// truncation once the stream outruns the ceiling.
    fn absorb(&mut self, bytes: &[u8]) {
        self.seen = self.seen.saturating_add(bytes.len() as u64);
        if !self.write_error && self.written < CAPTURE_MAX_BYTES {
            let room = (CAPTURE_MAX_BYTES - self.written) as usize;
            let take = room.min(bytes.len());
            match self.file.write_all(&bytes[..take]) {
                Ok(()) => {
                    self.hasher.update(&bytes[..take]);
                    self.written += take as u64;
                }
                // A file write failure disables further capture for this stream but
                // never disturbs the live echo or the run — the recorded digest and
                // byte count then reflect what reached disk.
                Err(_) => self.write_error = true,
            }
        }
        if self.seen > CAPTURE_MAX_BYTES {
            self.truncated = true;
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
        )
    }
}

type Shared = Arc<Mutex<StreamCapture>>;

/// A run's capture: the two per-stream files and their shared metadata. The runner
/// builds one when `--capture-dir` is set, hands each stream's [`CaptureTee`] to
/// the matching `stdout_tee`/`stderr_tee`, keeps this handle, and reads
/// [`finalize`](Self::finalize) once the run has ended.
pub struct Capture {
    stdout: Shared,
    stderr: Shared,
}

impl Capture {
    /// Open (creating the directory and truncating the two files) the capture for
    /// `dir`. Fails closed — like the `--jsonl` file, a capture the operator asked
    /// for but that cannot be created is reported *before* the child is spawned,
    /// never silently dropped.
    pub fn create(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let stdout = Arc::new(Mutex::new(StreamCapture::new(dir.join("stdout.log"))?));
        let stderr = Arc::new(Mutex::new(StreamCapture::new(dir.join("stderr.log"))?));
        Ok(Self { stdout, stderr })
    }

    /// The tee sink for stdout: `echo` (the live-echo target) fanned out to the
    /// stdout capture file.
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

/// A per-stream tee that writes each byte to the live echo *and* mirrors it into a
/// bounded capture file. Handed to `Command::stdout_tee`/`stderr_tee`; ProcessKit's
/// line pump drives it, awaiting each write (so back-pressure and stream framing are
/// exactly the plain-echo tee's).
///
/// The capture mirrors precisely the bytes the echo accepted, so the file can never
/// double-count or lose the tail of a partial write. If the echo sink ever errors
/// (e.g. the runner's own stdout was closed), the error is swallowed and capture
/// continues to the file alone — a broken live echo must not cost the transcript,
/// and, critically, an error returned from a tee would disable it for the rest of
/// the run.
pub struct CaptureTee<W> {
    echo: W,
    /// Latched once the echo sink errors: thereafter bytes go only to the file.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a stream's capture directly (bypassing the async tee) to exercise the
    /// counting / ceiling / hashing logic without a live process.
    fn drive(path: PathBuf, chunks: &[&[u8]]) -> StreamCapture {
        let mut cap = StreamCapture::new(path).expect("create capture file");
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
    fn empty_stream_is_the_empty_hash_and_untruncated() {
        let path = temp_path("empty");
        let mut cap = drive(path, &[]);
        let info = cap.info();
        assert_eq!(info.bytes(), 0);
        assert!(!info.truncated());
        assert_eq!(info.sha256(), crate::hash::sha256_hex(b""));
    }
}
