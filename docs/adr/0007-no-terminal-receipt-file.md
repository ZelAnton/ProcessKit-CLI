# 0007: No terminal receipt file for a run's outcome

- Status: Accepted
- Date: 2026-08-05

## Context

Child fidelity makes a foreground `run`'s exit code ambiguous by construction: the
runner exits with the child's own code verbatim, so a `106` is either the runner's
`TIMEOUT` or a child that happened to exit `106`. The [exit-code
contract](../exit-codes.md#why-a-band-is-not-enough-on-its-own) already names the
JSONL stream as the authority that resolves it, which means a supervising adapter
reads the stream after every call.

A proposal asked for a cheaper answer: an opt-in `run --outcome-json <path>`,
atomically replaced at terminal completion with a versioned subset of `runner_exit`
plus `run_id`, a cleanup confirmation, a capture summary, and artifact locators —
never replacing the lifecycle stream, and with its absence after an abrupt runner
death remaining meaningful. This record is the decision on that proposal, taken after
the published machine-output schemas and the `events` subcommand landed.

## Decision

Do not add a terminal receipt file. `run` keeps exactly one durable outcome artifact,
the required `--jsonl` lifecycle stream, and an adapter that wants the outcome without
opening it reads the **failure envelope** instead: under the global `--error-format
json`, a runner-owned ending prints one bounded JSON object on stderr whose `kind` is
spelled exactly like the terminal `runner_exit` event's `source`, and a child's own
exit prints none. The envelope's *presence*, not the numeric value, is what separates
the two readings of a reserved-band code.

The adapter-facing form of this rule is in the integration guide,
["Telling outcomes apart"](../integration.md#3-reading-the-jsonl-stream) and
["No terminal receipt file"](../integration.md#8-decided-no-terminal-receipt-file).

## Alternatives considered

- **`run --outcome-json <path>`, the proposal.** Rejected, on four grounds.

  1. *It would not remove the read it exists to remove.* The supervising adapter this
     decision was measured against — a foreground driver of this binary — consumes
     four event types after each call: `run_started` (`root_pid`, `mechanism`,
     `abrupt_cleanup`), `members_snapshot`, `output_captured`, and `runner_exit`. It
     treats a missing `run_started` or `output_captured` as its own named failure
     reason. A *terminal* receipt carries terminal facts; the start-time facts exist
     only in the stream. To actually retire that reader the receipt would have to grow
     into a second, full serialization of the run.
  2. *The read is six lines.* A foreground run emits six events, seven with
     `--capture-dir`, and eight if a requested resource cap (`--max-memory` /
     `--max-processes` / `--cpu-quota`) also contributes its post-run
     `limit_evidence`. Every way the stream grows past those six is a caller opting
     in — those two axes plus `--snapshot-interval`'s extra `members_snapshot`
     samples — i.e. asking for more events on purpose. The full ordering rule is in
     [`docs/schema.md`](../schema.md#ordering), "Ordering".
  3. *A cheaper answer already exists.* The `--error-format json` envelope resolves
     exactly the ambiguity the proposal names, with no path to allocate, no artifact
     to clean up, and no new schema family to version.
  4. *A receipt has no honest failure mode.* It would be written after the child's
     code is already decided, so a failed write or a failed replace leaves three bad
     options: fail the run, which rewrites the child's exit code and breaks the
     contract's core rule; succeed silently, which makes the receipt's absence
     ambiguous — "the runner died abruptly" *or* "the receipt could not be written" —
     and so destroys the one property the proposal wanted to preserve; or record the
     failure in the stream, which only a reader of the stream would see. This is the
     project's existing principle applied to a new channel: a `--jsonl` write failure
     is already outside every event's reach, because no event can report the failure
     of the channel that would carry it. Unlike the stream, a receipt carries nothing
     else, so its failure is total rather than partial.

  A fifth, smaller cost is worth recording: the stream is created or truncated at the
  start of the run and its `run_started` names the `run_id`, so a reader can confirm
  the file belongs to the run it asked about. A receipt that must never appear early
  or partially cannot be truncated at start, so one left at the same path by an
  earlier run survives the current run's abrupt death and reads as valid. It is
  mitigable by comparing a `run_id` field — but that comparison is the adapter-side
  logic the receipt was supposed to save.

- **A bounded terminal read over an arbitrary stream (`events --outcome --file`).**
  Deferred, not rejected. The primitive already exists — `wait --report-outcome`
  gates on a `run_started` naming the id, then scans a bounded 64 KiB tail in reverse
  for the last well-formed `runner_exit` ([`wait.rs`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/wait.rs)) —
  but it is reachable only for a run the invocation itself observed live, and a
  finished foreground run has already deleted its own registry record, so
  `wait --report-outcome` honestly answers `status: "unknown"` for it. `events` reads
  a stream a different way on purpose and offers no terminal-only mode. If reason 2's
  six-line read is ever *measured* to matter, the answer is to expose that existing
  read-side primitive over `--file` — one flag on a read-only subcommand, reusing the
  already-published `wait --report-outcome` shape — rather than to add a second write
  path. Recorded so a revisit starts from the smaller option.

## Consequences

`run` has one durable outcome channel, and `--jsonl` stays required, so there is no
configuration in which a receipt would be an adapter's only artifact. Adapters get a
documented stream-free disambiguation that costs one flag they can leave on for every
invocation. The residual gap is named rather than papered over: there is still no
first-party bounded terminal read over an arbitrary stream file, and closing it is a
read-side change if it is ever justified. Reopening this decision should rest on a
measured cost, not an anticipated one.
