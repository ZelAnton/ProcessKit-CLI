# 0002: Redact argv by default

- Status: Accepted
- Date: 2026-07-29 (retrospective)

## Context

Command lines routinely contain tokens, connection strings, paths, and customer
data. Lifecycle files and registry records outlive terminal output and are natural
inputs to automation, so copying raw argv into them by default widens secret
exposure.

## Decision

Publish a SHA-256 argv fingerprint and a fixed, non-secret worker-shape hint by
default. Publish raw argv only under explicit `--argv-raw`. Reuse the same
fingerprint in lifecycle events and registry discovery so the two views cannot
drift. Keep environment-file values out of argv and events.

See [Command redaction](../schema.md#command-redaction) and the registry's
[command identity](../registry.md#which-run-is-which--and-what-a-record-never-carries).
The shared fingerprint and hint construction lives in the
[event model](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/events.rs)
used by both artifacts.

## Alternatives considered

- Record raw argv by default. Rejected because convenience does not justify routine
  secret persistence.
- Drop all command identity. Rejected because operators still need to correlate and
  classify runs.
- Apply heuristic substring redaction. Rejected because an allow/deny list cannot
  recognize every secret format and creates false confidence.

## Consequences

Default diagnostics support equality and known-worker classification without
disclosing arguments. Operators who opt into raw argv accept its storage risk.
Hint rules and fingerprint canonicalization are public schema behavior and require
tests when changed.
