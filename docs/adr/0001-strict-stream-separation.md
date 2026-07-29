# 0001: Keep child streams and runner diagnostics separate

- Status: Accepted
- Date: 2026-07-29 (retrospective)

## Context

The CLI is both a transparent command runner and a structured lifecycle-event
producer. Mixing either JSONL or runner diagnostics into child stdout would corrupt
payload protocols and make faithful stream forwarding impossible.

## Decision

Forward child stdout only to runner stdout and child stderr only to runner stderr.
Write lifecycle events only to the required `--jsonl` file. Write runner-owned
human diagnostics to stderr, never to child stdout. Keep capture files per stream.

The normative event contract remains the [JSONL schema](../schema.md); I/O modes
and capture behavior are documented in [Standard I/O and capture](../io-and-capture.md).
The implementation is split between the
[event writer](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/events.rs),
[capture tee](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/capture.rs), and
[run launch path](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/run/launch.rs).

## Alternatives considered

- Emit JSONL on stdout. Rejected because it aliases machine events with arbitrary
  child output.
- Merge child stdout and stderr into one diagnostic stream. Rejected because it
  destroys ordering-independent stream identity and breaks consumers.
- Prefix relayed output. Rejected because pass-through bytes must remain unmangled.

## Consequences

Consumers can parse child stdout without filtering runner records and can tail the
JSONL file independently. The CLI must maintain separate pumps and capture metadata,
and every new diagnostic path must be reviewed for its destination.
