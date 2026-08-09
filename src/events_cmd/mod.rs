//! `events`: read back a run's JSONL lifecycle stream — render it, follow it, pass
//! it through, or check it against the schema this binary embeds.
//!
//! ```text
//! processkit-cli events --run-id build-42            # render what happened
//! processkit-cli events --run-id build-42 --follow   # watch it happen
//! processkit-cli events --file run.jsonl --json      # the runner's own bytes
//! processkit-cli events --file fixture.jsonl --validate
//! ```
//!
//! A supervisor of a detached run (`docs/detached-runs.md`) used to have to do three
//! separate things by hand to learn a run's story: discover the stream's locator
//! (`list --json`/`inspect --json` publish it), tail the file with whatever external
//! tooling was around, and parse raw JSON lines itself. This command closes that
//! loop inside the tool — the [`crate::list`] of *what happened*, next to `list`'s
//! *what exists*, [`crate::wait`]'s *is it over*, and [`crate::control`]'s *what is
//! it doing right now*.
//!
//! # Read-only, in exactly the sense `list`/`wait` are
//!
//! It opens the per-user registry through
//! [`registry::Registry::open_read_only`] — never the mutating
//! [`registry::Registry::open`] `run` uses, so reading a run's story cannot create
//! the registry directory or touch its permissions — opens the events file for
//! reading only, and **never** connects to a run's control transport. There is no
//! wire round trip here at all (nothing in [`crate::control`] is reached), no
//! registry mutation, and no way for this command to end, disturb, or even be
//! noticed by the run it is reading. That is the same safety class as `list` and
//! `wait`, and it is what makes `events --follow` safe to leave running against a
//! production run.
//!
//! # Naming the stream, and the three ways that can fail
//!
//! `--run-id` resolves the locator through the registry: every record whose
//! `run_id` matches — **identity first, health second** (K-016), so a
//! finished-but-not-yet-reaped run's record still leads to its completed stream —
//! and then the single JSONL locator those records publish. Three ways that yields
//! no single stream, all [`exit::CONTROL`] (103), the same verdict every other
//! by-`run-id` client gives when an id does not name one target:
//!
//! - no record names the id at all (a clean exit deletes its own record — this is
//!   where `--file` earns its keep),
//! - the records that do publish a locator publish **different** ones (ambiguity is
//!   a hard failure here exactly as it is for
//!   `inspect`/`cancel`/`kill`/`attest`/`wait`, see `docs/registry.md`),
//! - every matching record publishes none, because the run was started without
//!   `--jsonl` and so has no stream to read.
//!
//! `--file` skips all of that and reads the path it is given. It is the escape hatch
//! for a stream whose record is already gone, and the way to check a file this
//! registry never knew about at all — an adapter's own fixture, say. It touches the
//! registry only under `--follow`, and only to answer "is anything still writing
//! this?", so `events --file … --validate` works on a machine whose registry
//! directory is unreadable, or absent.
//!
//! # Why `--json` is a pass-through
//!
//! `--json` prints each line **exactly as the runner wrote it**, byte for byte. It
//! is deliberately *not* a round trip through this binary's own event types: a
//! stream written by a newer runner can legitimately carry an event type or a field
//! this build has never heard of, and deserializing into a fixed struct only to
//! re-serialize it would silently drop precisely that — the failure mode T-295
//! found in the control plane's own client-side re-serialization (K-092). The only
//! thing this command decides about a line is whether it *is* JSON; a line that is
//! not is reported on stderr rather than passed through, so a consumer piping stdout
//! into a JSONL parser can rely on every line being parseable.
//!
//! # Every operator string this command prints crosses the terminal barrier
//!
//! An events file is untrusted input, and so is the locator naming it
//! (`docs/threat-model.md`, "Untrusted inputs"), so nothing this command puts in
//! front of a human is trusted to be one line of ordinary text. The complete
//! inventory, all of it through `crate::text::terminal_safe_bounded` — the shared
//! ingress/render barrier `list`/`inspect` already use, never a narrower check of
//! this module's own (K-091):
//!
//! - every rendered fragment of an event, and the notice about a line that would
//!   not parse (`render.rs`);
//! - every schema violation `--validate` reports, and its echo of a line that is
//!   not JSON at all (`schema.rs`, `validate.rs`);
//! - the stream's own locator, in the two failures that echo it
//!   (`StreamReader::unreadable`) — the one operator string here whose value can
//!   arrive from **outside** this invocation's own argv, since under `--run-id` it
//!   is `Record::jsonl`, read back from a registry record that deliberately keeps
//!   address strings byte-for-byte (`src/registry/mod.rs`,
//!   `parse_and_validate_record`) precisely because renderers hold them to this
//!   barrier instead.
//!
//! Two values need no barrier here, for reasons rather than by omission: `run_id`
//! is rejected at ingress by `cli::parse_run_id` before it can reach a line at
//! all, and `--json`'s pass-through is machine output, which relies on JSON's own
//! escaping rather than on a terminal rendering — the distinction `src/text.rs`
//! draws explicitly.
//!
//! # Following, and when it stops
//!
//! `--follow` polls the file for growth at [`POLL_INTERVAL`], for the same honest
//! reason [`crate::wait`] polls the registry: there is no notification, wakeup, or
//! subscribable channel for either "this file grew" or "that runner died" that this
//! command could wait on, and pretending otherwise would be a lie about the
//! mechanism. It stops at the first of:
//!
//! - the terminal `runner_exit` event — the stream's own documented end
//!   (`docs/schema.md`), and the ordinary way a follow finishes;
//! - the run no longer being live in the registry, plus a bounded
//!   [`SETTLE_TIMEOUT`] with no further growth. A runner that was killed abruptly
//!   never gets to write its terminal event, and the settle window is what
//!   distinguishes "it died without finishing the stream" from the tiny handoff in
//!   which a clean runner has removed its registry record but not yet flushed its
//!   last line — the same bounded-settle reasoning `wait --report-outcome` applies
//!   to the identical race.
//!
//! It never invents a deadline of its own. A `--timeout` here could only cut a
//! caller off from the stream they asked to watch, and the honest bound is already
//! the run's own lifetime: `events --follow` returns when the run is over and not
//! before, exactly as a plain `wait` blocks until the run is over. A caller that
//! wants a wall-clock bound has one — `wait --timeout` alongside it, or whatever
//! bound it puts on the whole invocation.
//!
//! Following observes; it never re-opens. A stream truncated or replaced underneath
//! the open handle is not re-followed from its new start — the command reports what
//! the handle it holds gives it and stops when the run is over.

mod pattern;
mod render;
pub mod schema;
mod validate;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::cli::EventsArgs;
use crate::error_envelope::ErrorKind;
use crate::events::SCHEMA_VERSION;
use crate::exit::{self, RunnerError};
use crate::registry::{self, Health, Registry, RunStatus};
use crate::text;

use validate::ValidateReport;

/// How long to sleep between polls of a followed stream. The same quarter second
/// [`crate::wait`] uses between registry probes, and for the same trade-off: short
/// enough that an operator watching a live run does not feel it, long enough that a
/// multi-hour follow costs a negligible number of reads. This module keeps its own
/// constant rather than borrowing `wait`'s — following a file and polling a registry
/// are separate latency budgets that should be free to diverge — but the value is
/// deliberately identical today.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long a followed stream is given to produce its last bytes *after* the
/// registry says its run is over. Clean teardown removes the registry record
/// immediately before writing the terminal event, so a follow that stopped the
/// instant the record vanished would routinely miss the very line it exists to wait
/// for. Mirrors `wait --report-outcome`'s own bounded settle window over the
/// identical handoff.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(500);

/// How much is read from the stream per syscall while draining.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// The longest single line this command will buffer before giving up on it. The
/// events file is untrusted input (`docs/threat-model.md`, "Untrusted inputs",
/// which names this bound), so "one line" cannot be
/// allowed to mean "the whole file, in memory": a stream with no newline in sight
/// would otherwise grow this process without bound. Generously above any real event
/// — a `members_snapshot` of a very large tree is still orders of magnitude under it
/// — and the overrun is reported as an unreadable line rather than silently
/// truncated into something that might still parse.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// How much of an over-long line is echoed in the notice about it.
const OVERRUN_ECHO_BYTES: usize = 256;

/// The event that ends every stream (`docs/schema.md`). Matched together with the
/// `schema_version` this build speaks, the same pair `wait --report-outcome`'s tail
/// scan requires before believing a line is a run's terminal event.
const TERMINAL_EVENT: &str = "runner_exit";

/// Run `events`: resolve the stream, then drain it once (or follow it to its end)
/// through the output mode the caller chose.
pub fn run(args: &EventsArgs) -> Result<(), RunnerError> {
    let target = Target::resolve(args)?;
    let mut session = Session::new(args)?;
    let mut reader = StreamReader::open(&target.path)?;

    if !args.follow {
        reader.drain(&mut session)?;
        reader.flush_partial(&mut session);
        return session.finish();
    }

    let registry = registry::open_read_only_for_setup()?;
    // Set once the run stops being live: the instant after which a stream that has
    // not grown is accepted as over. Cleared again by any new byte, so a late flush
    // always wins over the deadline.
    let mut settling: Option<Instant> = None;
    loop {
        let grew = reader.drain(&mut session)?;
        if session.terminal_seen {
            break;
        }
        if grew || target.still_running(&registry)? {
            settling = None;
        } else {
            match settling {
                None => settling = Some(Instant::now() + SETTLE_TIMEOUT),
                Some(deadline) if Instant::now() >= deadline => break,
                Some(_) => {}
            }
        }
        sleep(POLL_INTERVAL);
    }

    reader.flush_partial(&mut session);
    if !session.terminal_seen {
        eprintln!("{}", target.unfinished_stream_note());
    }
    session.finish()
}

/// The stream this invocation reads, and how to ask whether more of it is coming.
struct Target {
    path: PathBuf,
    liveness: Liveness,
}

/// How a followed stream's run is asked about. Both forms are registry scans; they
/// differ only in what identifies the run — the id the caller named, or the stream
/// itself.
enum Liveness {
    /// `--run-id`: ask the registry about that id directly.
    RunId(String),
    /// `--file`: the run, if there is one, is whichever registry record publishes
    /// this very stream. Holds the canonicalized path when the filesystem could
    /// resolve one, so a record's absolute locator still matches a relative or
    /// differently-spelled `--file` argument.
    Stream(PathBuf),
}

impl Target {
    fn resolve(args: &EventsArgs) -> Result<Self, RunnerError> {
        if let Some(path) = args.file.as_deref() {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            return Ok(Self {
                path: path.to_path_buf(),
                liveness: Liveness::Stream(canonical),
            });
        }
        // clap's `required_unless_present`/`conflicts_with` pair on `EventsArgs`
        // guarantees exactly one of `run_id`/`file` is set, so this is never reached
        // with `run_id` absent.
        let run_id = args
            .run_id
            .as_deref()
            .expect("clap requires --run-id when --file is absent");
        let registry = registry::open_read_only_for_setup()?;
        Ok(Self {
            path: locate_by_run_id(&registry, run_id)?,
            liveness: Liveness::RunId(run_id.to_string()),
        })
    }

    /// Whether anything can still be writing this stream. Conservative in the one
    /// direction that matters: anything short of a confirmed "the run is over" reads
    /// as "keep following", so a stream is never abandoned on the strength of a probe
    /// that did not actually run — the same "unknown is not confirmed" stance
    /// [`RunStatus::Unprobed`] and `wait` take (K-024).
    fn still_running(&self, registry: &Registry) -> Result<bool, RunnerError> {
        match &self.liveness {
            Liveness::RunId(run_id) => {
                let status = registry
                    .probe_run(run_id)
                    .map_err(registry::setup_read_error)?;
                // `Ambiguous` means several live runs share the id: at least one of
                // them is live, and this command is already reading one specific
                // stream, so there is nothing ambiguous left to refuse — it simply
                // means "still going".
                Ok(!matches!(status, RunStatus::Finished))
            }
            Liveness::Stream(canonical) => {
                let entries = registry.entries().map_err(registry::setup_read_error)?;
                Ok(entries.iter().any(|entry| {
                    !matches!(entry.health, Health::Stale)
                        && entry
                            .record
                            .jsonl
                            .as_deref()
                            .is_some_and(|published| same_stream(published, canonical))
                }))
            }
        }
    }

    /// The honest note for a follow that ended without the stream's terminal event:
    /// says what was established (the run is over) and what that means (nothing more
    /// is coming), without claiming to know why the stream is incomplete.
    fn unfinished_stream_note(&self) -> String {
        let subject = match &self.liveness {
            Liveness::RunId(run_id) => format!("run `{run_id}` is no longer live"),
            Liveness::Stream(_) => {
                "no live registry record still publishes this stream".to_string()
            }
        };
        format!(
            "the stream ends without a terminal `{TERMINAL_EVENT}` event: {subject}, so nothing \
             more is coming — a runner killed abruptly never gets to write its terminal event"
        )
    }
}

/// Whether a record's published locator names the stream being followed. Compared
/// by canonical path where the filesystem can resolve one, so the record's absolute
/// path still matches a `--file` given relatively; falls back to a literal
/// comparison when it cannot (the file may be gone by now, and a locator that no
/// longer resolves is not evidence of anything).
fn same_stream(published: &str, canonical: &Path) -> bool {
    let candidate = Path::new(published);
    candidate == canonical
        || std::fs::canonicalize(candidate).is_ok_and(|resolved| resolved == canonical)
}

/// Resolve `--run-id` to the one stream it names, or explain why it names none.
///
/// Counts by the identity predicate (`run_id`) **before** looking at any secondary
/// attribute (K-016): every record about this run is considered, whatever its
/// health, because a finished-but-not-yet-reaped run's record is exactly the one
/// that leads to a completed stream worth reading.
fn locate_by_run_id(registry: &Registry, run_id: &str) -> Result<PathBuf, RunnerError> {
    let matching: Vec<_> = registry
        .entries()
        .map_err(registry::setup_read_error)?
        .into_iter()
        .filter(|entry| entry.record.run_id == run_id)
        .collect();

    if matching.is_empty() {
        return Err(RunnerError::new(
            exit::CONTROL,
            format!(
                "cannot read events for run `{run_id}`: no registry record names it — a run that \
                 exits cleanly deletes its own record, so read its stream directly with \
                 `--file <events.jsonl>`"
            ),
        )
        .with_kind(ErrorKind::NotFound));
    }

    let mut locators: Vec<String> = matching
        .iter()
        .filter_map(|entry| entry.record.jsonl.clone())
        .collect();
    locators.sort();
    locators.dedup();

    match locators.len() {
        // There is no stream to find: the record exists but never named one. That is
        // the same verdict for a machine as "no record names this run" — nothing to
        // read, and no retry will produce one — so both report `not_found`, with the
        // message carrying which of the two it was.
        0 => Err(RunnerError::new(
            exit::CONTROL,
            format!(
                "cannot read events for run `{run_id}`: its registry record publishes no JSONL \
                 stream — the run was started without `--jsonl`, so it has no lifecycle stream \
                 to read"
            ),
        )
        .with_kind(ErrorKind::NotFound)),
        1 => Ok(PathBuf::from(locators.remove(0))),
        streams => Err(RunnerError::new(
            exit::CONTROL,
            format!(
                "cannot read events for run `{run_id}`: ambiguous run id — {} registered records \
                 name {streams} different event streams; read the one you mean directly with \
                 `--file <events.jsonl>`",
                matching.len()
            ),
        )
        .with_kind(ErrorKind::AmbiguousRunId)),
    }
}

/// One complete line of the stream, parsed exactly once for every consumer that
/// needs it (the terminal-event check, the renderer, the validator).
pub(crate) struct StreamLine<'a> {
    number: usize,
    raw: &'a str,
    value: Result<Value, String>,
}

impl StreamLine<'_> {
    /// The line's 1-based position in the file, counting every line — blank ones
    /// included — so a number in a report is the number an editor shows.
    pub(crate) fn number(&self) -> usize {
        self.number
    }

    /// The line exactly as it was written, minus its terminator.
    pub(crate) fn raw(&self) -> &str {
        self.raw
    }

    /// The parsed line, or why it could not be parsed.
    pub(crate) fn value(&self) -> Result<&Value, &str> {
        self.value.as_ref().map_err(String::as_str)
    }
}

/// The checker used by the terminal predicate. It is the same document-driven
/// checker used by `--validate`, compiled once because a followed stream may contain
/// many events. A compile failure is fail-closed: without a trustworthy schema there
/// is no line this command should treat as terminal.
fn terminal_schema() -> Option<&'static schema::SchemaChecker> {
    static CHECKER: OnceLock<Option<schema::SchemaChecker>> = OnceLock::new();
    CHECKER
        .get_or_init(|| schema::SchemaChecker::compile().ok())
        .as_ref()
}

/// Whether a parsed line is a complete, schema-conforming v1 terminal event.
fn is_terminal(value: &Value) -> bool {
    value.get("event").and_then(Value::as_str) == Some(TERMINAL_EVENT)
        && value
            .get("schema_version")
            .and_then(Value::as_u64)
            .is_some_and(|version| version == u64::from(SCHEMA_VERSION))
        && terminal_schema().is_some_and(|checker| checker.conforms(value))
}

/// What this invocation does with each line, and what it reports at the end.
enum Output {
    /// Render each event for a human (the default).
    Human,
    /// Pass each line through verbatim.
    Json,
    /// Check each line against the embedded schema and report, instead of printing
    /// the events.
    Validate(Box<ValidateReport>),
}

/// The per-invocation state a drain writes into: the chosen output, whether the
/// terminal event has been seen, and how many events actually reached stdout.
struct Session {
    output: Output,
    terminal_seen: bool,
    emitted: usize,
}

impl Session {
    fn new(args: &EventsArgs) -> Result<Self, RunnerError> {
        let output = if args.validate {
            Output::Validate(Box::new(ValidateReport::new()?))
        } else if args.json {
            Output::Json
        } else {
            Output::Human
        };
        Ok(Self {
            output,
            terminal_seen: false,
            emitted: 0,
        })
    }

    fn absorb(&mut self, line: &StreamLine<'_>) {
        if let Ok(value) = line.value()
            && is_terminal(value)
        {
            self.terminal_seen = true;
        }
        match &mut self.output {
            Output::Validate(report) => report.absorb(line),
            Output::Json => match line.value() {
                Ok(_) => {
                    println!("{}", line.raw());
                    self.emitted += 1;
                }
                Err(reason) => self.report_unreadable(line, reason),
            },
            Output::Human => match line.value() {
                Ok(Value::Object(event)) => {
                    println!("{}", render::event_line(event));
                    self.emitted += 1;
                }
                Ok(_) => self.report_unreadable(line, "not a JSON object"),
                Err(reason) => self.report_unreadable(line, reason),
            },
        }
    }

    /// A line this command could not read as an event goes to **stderr**, never to
    /// stdout: stdout carries the stream (rendered or verbatim) and nothing else, so
    /// a consumer piping it into a JSONL parser is never handed something that is not
    /// an event, while nothing is dropped in silence either.
    fn report_unreadable(&self, line: &StreamLine<'_>, reason: &str) {
        eprintln!(
            "{}",
            render::unreadable_line(line.number(), reason, line.raw())
        );
    }

    fn finish(self) -> Result<(), RunnerError> {
        match self.output {
            Output::Validate(report) => report.finish(),
            Output::Human => {
                if self.emitted == 0 {
                    // The `list`-style notice: an empty result is a result, not an
                    // error, and a bare silent exit would look like a failure.
                    println!("no events in the stream");
                }
                Ok(())
            }
            // Machine output stays machine output: an empty stream is zero lines.
            Output::Json => Ok(()),
        }
    }
}

/// The incremental line reader over one open events file: it hands out only
/// **complete** lines, keeping any partial tail buffered until its newline arrives,
/// which is what makes following a file being appended to safe — a half-written
/// event is never rendered, passed through, or reported as malformed.
struct StreamReader {
    path: PathBuf,
    file: File,
    pending: Vec<u8>,
    /// The number of the last line handed out — the file's own line numbering.
    number: usize,
    /// Set while discarding the remainder of a line that overran
    /// [`MAX_LINE_BYTES`], so its tail is not mistaken for a line of its own.
    skipping: bool,
}

impl StreamReader {
    /// The `SETUP` failure for a stream this command could not open or read (the
    /// verb is `action`), with its locator held to the **same terminal barrier**
    /// every other operator string this command prints already crosses (K-091).
    /// Both failures are built here rather than at their call sites so the barrier
    /// stands in one place and cannot be forgotten at one of them.
    ///
    /// The barrier is not ceremony here: under `--run-id` this path is not the
    /// caller's own argv but `Record::jsonl`, read back off disk. The registry
    /// deliberately leaves that field byte-for-byte at ingress —
    /// `registry::parse_and_validate_record` sanitizes `argv_sha256`/`hint`/`labels`
    /// and says so in as many words for the address fields it does not touch:
    /// "human-readable renderers pass them through `crate::text::terminal_safe` at
    /// the terminal boundary". `list` is one such renderer for this very field
    /// (`src/list.rs`, the `JSONL` column); so is this. And the trigger is the
    /// ordinary one, not an exotic corruption: a locator from a still-registered run
    /// stops resolving whenever its file is moved, deleted, or becomes unreadable,
    /// while the value itself originated in `run --jsonl` — a path an orchestrator
    /// typically assembles from a job name, a branch, or a ticket id.
    ///
    /// Only the locator is sanitized. `err` is the operating system's own message
    /// for the failed call; neither `File::open` nor `Read::read` folds the path
    /// into it, so it carries nothing this command has not already vouched for.
    fn unreadable(action: &str, path: &Path, err: &std::io::Error) -> RunnerError {
        RunnerError::new(
            exit::SETUP,
            format!(
                "could not {action} the events stream `{}`: {err}",
                text::terminal_safe_bounded(&path.display().to_string())
            ),
        )
    }

    fn open(path: &Path) -> Result<Self, RunnerError> {
        let file = File::open(path).map_err(|err| Self::unreadable("open", path, &err))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            pending: Vec::new(),
            number: 0,
            skipping: false,
        })
    }

    /// Read everything available right now, handing each complete line to `session`.
    /// Returns whether any new bytes arrived, which is what tells a follow the
    /// difference between a stream that is idle and one that is over.
    fn drain(&mut self, session: &mut Session) -> Result<bool, RunnerError> {
        let mut grew = false;
        let mut chunk = vec![0u8; READ_CHUNK_BYTES];
        loop {
            let read = self
                .file
                .read(&mut chunk)
                .map_err(|err| Self::unreadable("read", &self.path, &err))?;
            if read == 0 {
                return Ok(grew);
            }
            grew = true;
            self.pending.extend_from_slice(&chunk[..read]);
            self.emit_complete_lines(session);
            self.guard_line_length(session);
        }
    }

    /// Hand out every complete (newline-terminated) line currently in `pending` —
    /// **enforcing [`MAX_LINE_BYTES`] on each one as it is found**, not only on
    /// whatever is left unterminated afterwards. A line whose terminating `\n`
    /// arrives in the same read that pushes it past the limit is still an overrun:
    /// nothing about finding the newline first is allowed to let it slip past the
    /// guard as an ordinary line (that gap is exactly what let an oversized record
    /// through until this check existed — [`StreamReader::guard_line_length`] alone
    /// only ever saw an *empty* buffer for such a line, since this method had
    /// already drained it out from under it).
    fn emit_complete_lines(&mut self, session: &mut Session) {
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            if !self.skipping && index > MAX_LINE_BYTES {
                // The payload before this newline already overruns the limit —
                // report and discard it exactly as `guard_line_length` would for an
                // unterminated overrun, then keep going: this newline resolves the
                // line's boundary, so there is nothing left to skip afterwards.
                let echo_len = OVERRUN_ECHO_BYTES.min(index);
                let echo_source = self.pending[..echo_len].to_vec();
                self.pending.drain(..=index);
                self.report_overrun(session, &echo_source);
                continue;
            }
            let mut raw: Vec<u8> = self.pending.drain(..=index).collect();
            raw.pop();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
            if self.skipping {
                // The tail of an over-long line, already reported as one line.
                self.skipping = false;
                continue;
            }
            self.number += 1;
            emit(session, self.number, &raw);
        }
    }

    /// Refuse to buffer an unterminated line past [`MAX_LINE_BYTES`]: report it
    /// **once** and discard the rest of it, however long it turns out to be, rather
    /// than letting an untrusted file decide this process's memory use.
    ///
    /// Called after [`StreamReader::emit_complete_lines`] has already taken every
    /// newline out of `pending` — including reporting, in that same pass, any
    /// newline-terminated line that itself overran the limit — so what is left here
    /// is by definition an unterminated line and discarding it cannot lose a line
    /// boundary. Both the reporting pass and every later pass over the *same*
    /// over-long line must clear the buffer — only the report is once. (Skipping the
    /// clear on the later passes would leave exactly the unbounded growth this guard
    /// exists to prevent.)
    fn guard_line_length(&mut self, session: &mut Session) {
        if self.pending.len() <= MAX_LINE_BYTES {
            return;
        }
        if self.skipping {
            // Still inside the line already reported: keep discarding it silently.
            self.pending.clear();
            return;
        }
        let echo_source = std::mem::take(&mut self.pending);
        self.skipping = true;
        self.report_overrun(session, &echo_source);
    }

    /// Report an over-long line's overrun exactly once — echoing up to
    /// [`OVERRUN_ECHO_BYTES`] of its own bytes — and advance the line number so a
    /// later report's numbering still matches the file. Shared by both places a line
    /// can be found to overrun [`MAX_LINE_BYTES`]: one still unterminated in
    /// `pending` ([`StreamReader::guard_line_length`]), the other already resolved by
    /// an arriving newline ([`StreamReader::emit_complete_lines`]).
    fn report_overrun(&mut self, session: &mut Session, echo_source: &[u8]) {
        let echo =
            String::from_utf8_lossy(&echo_source[..OVERRUN_ECHO_BYTES.min(echo_source.len())])
                .into_owned();
        self.number += 1;
        session.absorb(&StreamLine {
            number: self.number,
            raw: &echo,
            value: Err(format!(
                "line exceeds the {MAX_LINE_BYTES}-byte limit and was skipped"
            )),
        });
    }

    /// Hand out a final line that never got its newline — what an interrupted writer
    /// leaves behind. Called once, when the stream is done being read: during a
    /// follow the same bytes stay buffered, since more of the line may still be on
    /// its way.
    fn flush_partial(&mut self, session: &mut Session) {
        if self.pending.is_empty() || self.skipping {
            return;
        }
        let raw = std::mem::take(&mut self.pending);
        self.number += 1;
        emit(session, self.number, &raw);
    }
}

/// Parse one line's bytes and hand the result to `session`. Blank lines are skipped
/// (they still consume their line number, so a report's numbering matches the file),
/// and invalid UTF-8 is replaced rather than fatal: the file is untrusted, and a
/// stream that is not what it claims to be should be *reported*, never a panic.
fn emit(session: &mut Session, number: usize, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    if text.trim().is_empty() {
        return;
    }
    let value =
        serde_json::from_str::<Value>(&text).map_err(|err| format!("not valid JSON ({err})"));
    session.absorb(&StreamLine {
        number,
        raw: &text,
        value,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::test_support::scratch_registry as scratch;
    use clap::Parser;
    use std::io::Write;

    fn args(argv: &[&str]) -> EventsArgs {
        let mut full = vec!["processkit-cli", "events"];
        full.extend_from_slice(argv);
        let cli = crate::cli::Cli::try_parse_from(full).expect("a valid events invocation");
        match cli.command {
            crate::cli::Command::Events(args) => args,
            _ => panic!("expected the events subcommand"),
        }
    }

    /// Drive the reader over a fixed byte string, exactly as a drain would, and
    /// report what the session saw.
    fn read_all(bytes: &[u8], argv: &[&str]) -> (Session, usize) {
        let dir = scratch("events-reader");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let path = dir.join("events.jsonl");
        std::fs::write(&path, bytes).expect("write the fixture stream");

        let mut session = Session::new(&args(argv)).expect("the session builds");
        let mut reader = StreamReader::open(&path).expect("open the fixture stream");
        reader
            .drain(&mut session)
            .expect("drain the fixture stream");
        reader.flush_partial(&mut session);
        let lines = reader.number;
        let _ = std::fs::remove_dir_all(&dir);
        (session, lines)
    }

    const RUN_STARTED: &str = r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"run_started","run_id":"r1"}"#;
    const RUNNER_EXIT: &str = r#"{"schema_version":1,"time":"2026-07-22T09:00:01.000Z","event":"runner_exit","code":0,"source":"child_exit","child_code":0}"#;
    const MALFORMED_RUNNER_EXIT: &str = r#"{"schema_version":1,"event":"runner_exit"}"#;

    /// The terminal predicate is the complete v1 runner-exit shape, not merely its
    /// event tag: required fields, types, allowed values, and extra fields all matter.
    #[test]
    fn only_a_schema_conforming_runner_exit_ends_the_stream() {
        let terminal: Value = serde_json::from_str(RUNNER_EXIT).expect("valid JSON");
        assert!(is_terminal(&terminal));

        for other in [
            RUN_STARTED,
            MALFORMED_RUNNER_EXIT,
            r#"{"schema_version":1,"time":"2026-07-22T09:00:01.000Z","event":"runner_exit","code":"zero","source":"child_exit","child_code":0}"#,
            r#"{"schema_version":1,"time":"2026-07-22T09:00:01.000Z","event":"runner_exit","code":0,"source":"teleported","child_code":null}"#,
            r#"{"schema_version":1,"time":"2026-07-22T09:00:01.000Z","event":"runner_exit","code":0,"source":"child_exit","child_code":0,"extra":true}"#,
            r#"{"schema_version":99,"event":"runner_exit","code":0}"#,
            r#"{"event":"runner_exit","code":0}"#,
        ] {
            let value: Value = serde_json::from_str(other).expect("valid JSON");
            assert!(!is_terminal(&value), "must not end the stream: {other}");
        }
    }

    /// A malformed runner-exit line must not end a follow before a later complete
    /// terminal line arrives. This models the append boundary that `--follow` polls.
    #[test]
    fn a_malformed_runner_exit_does_not_end_follow_before_a_valid_terminal() {
        let dir = scratch("events-follow-malformed-terminal");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let path = dir.join("events.jsonl");
        std::fs::write(&path, format!("{MALFORMED_RUNNER_EXIT}\n"))
            .expect("write the malformed terminal");

        let mut session = Session::new(&args(&["--file", "x"])).expect("the session builds");
        let mut reader = StreamReader::open(&path).expect("open the fixture stream");
        reader
            .drain(&mut session)
            .expect("drain the malformed line");
        assert!(
            !session.terminal_seen,
            "malformed terminal must not stop follow"
        );

        let mut appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen the fixture for appending");
        writeln!(appended, "{RUNNER_EXIT}").expect("append the valid terminal");
        reader
            .drain(&mut session)
            .expect("drain the valid terminal");
        assert!(
            session.terminal_seen,
            "the later valid terminal ends follow"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Validation still reports the malformed runner-exit line while consuming the
    /// later valid terminal event in the same stream.
    #[test]
    fn validation_reports_malformed_runner_exit_before_a_valid_terminal() {
        let stream = format!("{RUN_STARTED}\n{MALFORMED_RUNNER_EXIT}\n{RUNNER_EXIT}\n");
        let (session, lines) = read_all(stream.as_bytes(), &["--file", "x", "--validate"]);
        assert_eq!(lines, 3, "all lines are consumed");
        assert!(
            session.terminal_seen,
            "the later valid terminal is consumed"
        );
        match session.output {
            Output::Validate(report) => {
                let err = report
                    .finish()
                    .expect_err("the malformed terminal must fail validation");
                assert_eq!(err.code(), exit::EVENTS_INVALID);
            }
            _ => panic!("the test must use validation output"),
        }
    }

    /// A complete stream is read line for line, and the terminal event is noticed.
    #[test]
    fn a_complete_stream_is_read_and_its_terminal_event_noticed() {
        let stream = format!("{RUN_STARTED}\n{RUNNER_EXIT}\n");
        let (session, lines) = read_all(stream.as_bytes(), &["--file", "x"]);
        assert_eq!(lines, 2, "both lines were handed out");
        assert_eq!(session.emitted, 2, "both events rendered");
        assert!(session.terminal_seen, "the terminal event was noticed");
    }

    /// The property that makes following a file being appended to safe: a line
    /// without its newline yet is **not** handed out by a drain — only
    /// [`StreamReader::flush_partial`], at the very end, ever releases it.
    #[test]
    fn a_partial_line_is_withheld_until_its_newline_arrives() {
        let dir = scratch("events-partial");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let path = dir.join("events.jsonl");
        std::fs::write(&path, format!("{RUN_STARTED}\n{{\"schema_ver")).expect("write a torn tail");

        let mut session = Session::new(&args(&["--file", "x"])).expect("the session builds");
        let mut reader = StreamReader::open(&path).expect("open the fixture stream");
        assert!(
            reader.drain(&mut session).expect("first drain"),
            "bytes arrived"
        );
        assert_eq!(session.emitted, 1, "only the complete line was handed out");
        assert!(!session.terminal_seen);

        // The rest of the line, then its newline: now it is a line.
        std::fs::write(&path, format!("{RUN_STARTED}\n{RUNNER_EXIT}\n"))
            .expect("complete the torn tail");
        assert!(
            reader.drain(&mut session).expect("second drain"),
            "more bytes arrived"
        );
        assert_eq!(session.emitted, 2, "the completed line was handed out once");
        assert!(
            session.terminal_seen,
            "the completed line is the terminal event"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Blank lines consume a line number but are never handed to an output mode, so
    /// a report's line numbers keep matching the file while a blank line is not
    /// reported as a malformed event.
    #[test]
    fn blank_lines_keep_their_number_without_being_events() {
        let stream = format!("{RUN_STARTED}\n\n   \n{RUNNER_EXIT}\n");
        let (session, lines) = read_all(stream.as_bytes(), &["--file", "x"]);
        assert_eq!(lines, 4, "every line of the file is numbered");
        assert_eq!(session.emitted, 2, "only the two events are rendered");
    }

    /// A `\r\n` stream (a file written on Windows by something other than this
    /// runner) reads as ordinary lines rather than as trailing-carriage-return junk
    /// that would fail every JSON parse.
    #[test]
    fn carriage_returns_are_not_part_of_a_line() {
        let stream = format!("{RUN_STARTED}\r\n{RUNNER_EXIT}\r\n");
        let (session, _) = read_all(stream.as_bytes(), &["--file", "x"]);
        assert_eq!(session.emitted, 2);
        assert!(session.terminal_seen);
    }

    /// An unterminated line longer than the buffer limit is reported once and its
    /// remainder discarded — the untrusted-input bound that keeps "one line" from
    /// meaning "the whole file, in memory". The stream continues afterwards.
    ///
    /// The over-long line is deliberately several times the limit, not a little
    /// over it: the bound has to hold for *every* pass over the same runaway line,
    /// not only the pass that reports it, and this test is what proves the buffer
    /// does not simply start growing again after the report.
    #[test]
    fn an_overlong_line_is_reported_once_and_never_buffered_past_the_limit() {
        let dir = scratch("events-overlong");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let path = dir.join("events.jsonl");

        let mut stream = Vec::new();
        stream.extend_from_slice(RUN_STARTED.as_bytes());
        stream.push(b'\n');
        stream.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES * 3 + 64));
        stream.push(b'\n');
        stream.extend_from_slice(RUNNER_EXIT.as_bytes());
        stream.push(b'\n');
        std::fs::write(&path, &stream).expect("write the fixture stream");

        let mut session = Session::new(&args(&["--file", "x"])).expect("the session builds");
        let mut reader = StreamReader::open(&path).expect("open the fixture stream");
        reader
            .drain(&mut session)
            .expect("drain the fixture stream");
        reader.flush_partial(&mut session);

        assert_eq!(
            session.emitted, 2,
            "the two real events are rendered, the overrun is not"
        );
        assert!(
            session.terminal_seen,
            "the stream is still read, and ended, after the overrun"
        );
        assert!(
            !reader.skipping,
            "the overrun's own newline ends the skip, so the next line is a line again"
        );

        // The bound itself, measured where it can actually be violated: a runaway
        // line that never gets a newline at all, so what the reader is holding when
        // the drain returns *is* its peak. (Measuring after a terminated overrun
        // would prove nothing — the newline empties the buffer either way, which is
        // exactly how a first draft of this guard passed while still growing without
        // limit on every pass after the one that reported.)
        let unterminated = dir.join("unterminated.jsonl");
        let mut runaway = RUN_STARTED.as_bytes().to_vec();
        runaway.push(b'\n');
        runaway.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES * 3 + 64));
        std::fs::write(&unterminated, &runaway).expect("write the runaway fixture");

        let mut session = Session::new(&args(&["--file", "x"])).expect("the session builds");
        let mut reader = StreamReader::open(&unterminated).expect("open the runaway fixture");
        reader
            .drain(&mut session)
            .expect("drain the runaway fixture");
        assert!(
            reader.pending.len() <= MAX_LINE_BYTES,
            "an unterminated runaway line must never buffer past the limit: {} bytes",
            reader.pending.len()
        );
        assert_eq!(session.emitted, 1, "only the one real event was rendered");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line whose payload is exactly `MAX_LINE_BYTES + 1` bytes, terminated by a
    /// `\n` that only arrives once the buffer already sits above the limit — the
    /// regression for the guard-ordering bug this task fixes.
    ///
    /// `emit_complete_lines` used to drain and render this as an ordinary line the
    /// instant its terminating newline arrived, because `guard_line_length` runs
    /// only *after* it and by then found nothing but an empty buffer to check: the
    /// oversized record had already been handed to rendering/validation instead of
    /// being rejected. This is deliberately the tightest possible overrun — one
    /// byte past the limit, with its newline present — rather than the many-times-
    /// over-the-limit line the older overrun test uses, because the bug is specific
    /// to a line landing exactly on this boundary with a newline already in hand.
    #[test]
    fn a_line_of_exactly_max_plus_one_bytes_with_a_trailing_newline_is_rejected() {
        let dir = scratch("events-exact-overrun");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let path = dir.join("events.jsonl");

        let mut stream = Vec::new();
        stream.extend_from_slice(RUN_STARTED.as_bytes());
        stream.push(b'\n');
        stream.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES + 1));
        stream.push(b'\n');
        stream.extend_from_slice(RUNNER_EXIT.as_bytes());
        stream.push(b'\n');
        std::fs::write(&path, &stream).expect("write the fixture stream");

        let mut session = Session::new(&args(&["--file", "x"])).expect("the session builds");
        let mut reader = StreamReader::open(&path).expect("open the fixture stream");
        reader
            .drain(&mut session)
            .expect("drain the fixture stream");
        reader.flush_partial(&mut session);

        assert_eq!(
            session.emitted, 2,
            "the two real events are rendered, the exact-boundary overrun is not"
        );
        assert!(
            session.terminal_seen,
            "the stream is still read, and ended, past the overrun"
        );
        assert!(
            !reader.skipping,
            "the overrun's own newline already resolves its boundary; nothing is left to skip"
        );
        assert!(
            reader.pending.is_empty(),
            "nothing from the overrun line is left buffered"
        );
        assert_eq!(
            reader.number, 3,
            "all three lines are counted, including the overrun's own report"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same exact-boundary overrun, checked under `--validate`: the oversized
    /// line is counted and reported as invalid rather than silently passing (or
    /// silently vanishing without being checked at all).
    ///
    /// Only the overrun line and the (schema-conforming, per
    /// [`only_a_schema_conforming_runner_exit_ends_the_stream`]) terminal event are
    /// in this stream, deliberately without `RUN_STARTED` — its shape does not
    /// itself conform to the embedded schema, which would make the invalid tally
    /// this test asserts about something other than the overrun.
    #[test]
    fn a_line_of_exactly_max_plus_one_bytes_fails_validation_as_one_invalid_line() {
        let mut stream = std::iter::repeat_n(b'x', MAX_LINE_BYTES + 1).collect::<Vec<u8>>();
        stream.push(b'\n');
        stream.extend_from_slice(RUNNER_EXIT.as_bytes());
        stream.push(b'\n');

        let (session, lines) = read_all(&stream, &["--file", "x", "--validate"]);
        assert_eq!(lines, 2, "both lines are counted, including the overrun");
        assert!(
            session.terminal_seen,
            "the valid terminal is still consumed after the overrun"
        );
        match session.output {
            Output::Validate(report) => {
                let err = report
                    .finish()
                    .expect_err("the oversized line must not be accepted as valid");
                assert_eq!(err.code(), exit::EVENTS_INVALID);
                let message = err.to_string();
                assert!(
                    message.contains("1 of 2"),
                    "exactly the one overrun line is invalid, not the terminal event: {message}"
                );
            }
            _ => panic!("the test must use validation output"),
        }
    }

    /// Invalid UTF-8 is replaced, not fatal: the line is reported as unreadable and
    /// the stream carries on.
    #[test]
    fn invalid_utf8_is_reported_rather_than_fatal() {
        let mut stream = vec![0xff, 0xfe, b'\n'];
        stream.extend_from_slice(RUNNER_EXIT.as_bytes());
        stream.push(b'\n');
        let (session, lines) = read_all(&stream, &["--file", "x"]);
        assert_eq!(lines, 2);
        assert_eq!(session.emitted, 1, "only the valid event is rendered");
        assert!(session.terminal_seen);
    }

    /// The `--json` mode hands back the runner's own bytes: the emitted line is the
    /// raw one, never a re-serialization (K-092), so a field this build has never
    /// heard of survives the trip.
    #[test]
    fn json_mode_passes_unknown_fields_through_untouched() {
        let exotic = r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"run_started","run_id":"r1","invented_by_a_newer_runner":{"deep":[1,2,3]}}"#;
        let (session, _) = read_all(format!("{exotic}\n").as_bytes(), &["--file", "x", "--json"]);
        assert_eq!(session.emitted, 1);
        assert!(matches!(session.output, Output::Json));
    }

    /// A stream with no lines at all is a result, not a failure.
    #[test]
    fn an_empty_stream_is_not_an_error() {
        let (session, lines) = read_all(b"", &["--file", "x"]);
        assert_eq!(lines, 0);
        assert_eq!(session.emitted, 0);
        assert!(session.finish().is_ok());
    }

    /// A `--run-id` that names no registry record at all fails with the shared
    /// `CONTROL` verdict and points at the escape hatch, rather than pretending an
    /// empty stream.
    #[test]
    fn an_unknown_run_id_fails_closed_and_names_the_escape_hatch() {
        let dir = scratch("events-unknown-id");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let err = locate_by_run_id(&registry, "never-registered")
            .expect_err("an unknown run id names no stream");
        assert_eq!(err.code(), exit::CONTROL);
        let message = err.to_string();
        assert!(
            message.contains("never-registered"),
            "names the run: {message}"
        );
        assert!(
            message.contains("--file"),
            "points at the escape hatch: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run registered without `--jsonl` has no stream, and is told so — not
    /// reported as a missing run, which would send the caller looking for the wrong
    /// problem.
    #[test]
    fn a_run_without_a_jsonl_locator_says_so() {
        let dir = scratch("events-no-locator");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let registration = registry
            .register_plain("quiet-run", None, std::time::SystemTime::now())
            .expect("register a run with no locator");

        let err =
            locate_by_run_id(&registry, "quiet-run").expect_err("a run with no stream to read");
        assert_eq!(err.code(), exit::CONTROL);
        assert!(
            err.to_string().contains("--jsonl"),
            "names the missing flag: {err}"
        );

        registration.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stream a live record publishes is what `--run-id` resolves to, whatever
    /// the record's health — a finished-but-not-yet-reaped run's completed stream is
    /// exactly the one worth reading.
    #[test]
    fn a_published_locator_resolves_by_run_id() {
        let dir = scratch("events-locator");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        let jsonl = dir.join("run.jsonl");
        let published = jsonl.to_string_lossy().into_owned();
        let registration = registry
            .register_with_labels_and_artifacts(
                "loud-run",
                None,
                std::time::SystemTime::now(),
                &crate::events::CommandFingerprint::for_argv(["pkc-test-fixture", "loud-run"]),
                &std::collections::BTreeMap::new(),
                registry::ArtifactLocators {
                    jsonl: Some(&published),
                    capture_dir: None,
                },
            )
            .expect("register a run publishing its stream");

        let resolved = locate_by_run_id(&registry, "loud-run").expect("the locator resolves");
        assert_eq!(resolved, jsonl);

        registration.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The locator resolved from a registry record is untrusted operator text, and
    /// the two failure messages that echo it are the only place in this command it
    /// reaches a terminal — so they cross the same barrier the renderer does
    /// (K-091), exactly as `list` already holds this very `jsonl` field.
    ///
    /// Driven end to end from the registry, because that is where the hostile value
    /// actually comes from: `register` writes the locator as given and
    /// `parse_and_validate_record` deliberately reads address fields back
    /// byte-for-byte, so what `locate_by_run_id` hands back really is unsanitized
    /// bytes off disk — the premise this test would be worthless without, and which
    /// it therefore asserts before checking the message.
    #[test]
    fn a_locator_from_a_registry_record_never_reaches_the_terminal_unsanitized() {
        let dir = scratch("events-hostile-locator");
        let registry = Registry::open_in(dir.clone()).expect("open registry");
        // A newline that would forge a second diagnostic line, an ESC sequence that
        // would recolor (or, with the right suffix, erase) what is already on
        // screen, and a bidi override that would reverse the rest of it.
        let hostile = format!(
            "{}\nprocesskit-cli: all runs completed successfully\u{1b}[2K\u{202e}",
            dir.join("run.jsonl").display()
        );
        let registration = registry
            .register_with_labels_and_artifacts(
                "hostile-locator",
                None,
                std::time::SystemTime::now(),
                &crate::events::CommandFingerprint::for_argv([
                    "pkc-test-fixture",
                    "hostile-locator",
                ]),
                &std::collections::BTreeMap::new(),
                registry::ArtifactLocators {
                    jsonl: Some(&hostile),
                    capture_dir: None,
                },
            )
            .expect("register a run publishing a hostile locator");

        let resolved = locate_by_run_id(&registry, "hostile-locator")
            .expect("the hostile locator still resolves to one stream");
        let round_tripped = resolved.to_string_lossy().into_owned();
        assert!(
            round_tripped.contains('\n')
                && round_tripped.contains('\u{1b}')
                && round_tripped.contains('\u{202e}'),
            "the registry hands back the bytes as written — otherwise this test \
             proves nothing: {round_tripped:?}"
        );

        // The routine trigger: a locator a live record still publishes, pointing at
        // a file that is not there.
        let Err(open_failure) = StreamReader::open(&resolved) else {
            panic!("a locator naming no file must not open");
        };
        assert_eq!(open_failure.code(), exit::SETUP);
        for message in [
            open_failure.to_string(),
            StreamReader::unreadable(
                "read",
                &resolved,
                &std::io::Error::other("the stream went away"),
            )
            .to_string(),
        ] {
            assert_eq!(
                message.lines().count(),
                1,
                "a forged newline cannot add a diagnostic line: {message:?}"
            );
            assert!(
                message.chars().all(|character| !character.is_control()),
                "no terminal control survives: {message:?}"
            );
            assert!(
                !message.contains('\u{202e}'),
                "no invisible formatting character survives: {message:?}"
            );
            assert!(
                message.contains("events stream"),
                "the diagnostic still says what failed: {message}"
            );
        }

        registration.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
