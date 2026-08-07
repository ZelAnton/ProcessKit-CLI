# Architecture decision records

Architecture decision records (ADRs) preserve why a durable project choice was
made, including rejected alternatives and accepted trade-offs. They complement
the current architecture description and changelog: an ADR is thematic and remains
useful when the code around the decision moves.

## Index

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-strict-stream-separation.md) | Keep child streams and runner diagnostics separate | Accepted |
| [0002](0002-redact-argv-by-default.md) | Redact argv by default | Accepted |
| [0003](0003-live-runner-control-plane.md) | Keep control in the live runner | Accepted |
| [0004](0004-container-scoped-cleanup.md) | Scope cleanup to owned containers | Accepted |
| [0005](0005-shell-free-command-contract.md) | Keep command execution shell-free | Accepted |
| [0006](0006-registry-polled-wait.md) | Poll the registry for detached waits | Accepted |
| [0007](0007-no-terminal-receipt-file.md) | No terminal receipt file for a run's outcome | Accepted |
| [0008](0008-no-cli-external-pid-adoption.md) | Do not expose external PID adoption in the CLI | Accepted |

## Adding a decision

Use the next four-digit number and a short kebab-case title. Link the new record
from this index and `docs/SUMMARY.md`. Record supersession in both the old and new
ADR rather than rewriting history.

```markdown
# NNNN: Decision title

- Status: Proposed | Accepted | Superseded by ADR-NNNN
- Date: YYYY-MM-DD

## Context

What forces a choice, including relevant constraints.

## Decision

The durable rule in imperative, testable terms.

## Alternatives considered

The credible alternatives and why they were not selected.

## Consequences

Benefits, costs, operational effects, and follow-up obligations.
```
