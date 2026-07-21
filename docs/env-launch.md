# Launch contract: `CC_PROCESSKIT_RUN` and the preflight probe

This is the **normative description** of how a consumer — chiefly Orchestra —
finds and launches the `processkit-cli` binary through an environment variable,
and how it verifies, **before** running any payload, that the file it found is a
*suitable* runner. It is part of processkit-cli's public compatibility surface,
alongside the [CLI flags](../README.md#command-interface), the
[exit-code contract](exit-codes.md), and the JSONL
[`schema_version`](schema.md). The in-code source of truth for the probe is
`src/probe.rs`; this document is what external consumers pin against.

It closes the loop with the primary consumer (`docs/ROADMAP.md`, "Distribution
and Orchestra migration"): the orchestrator already has a launch contract for
finding an *interpreter*; this gives it the analogous, additive contract for
finding the *binary* runner — with a fail-closed guarantee so a misconfigured or
incompatible runner can never silently degrade into an uncontained launch.

## `CC_PROCESSKIT_RUN`

`CC_PROCESSKIT_RUN` is an environment variable that names the **absolute path to
the `processkit-cli` binary** the orchestrator should use to launch contained
commands, in preference to any other discovery method (a `PATH` lookup, a bundled
copy, a build output). It is the binary-runner analogue of the orchestrator's
existing interpreter-launch contract: set it, and the orchestrator routes its
contained launches through exactly that file.

- The value is a filesystem path to an executable, e.g.
  `C:\tools\processkit-cli.exe` or `/opt/orchestra/bin/processkit-cli`.
- When the variable is **unset**, the orchestrator falls back to its prior
  discovery method unchanged — this contract adds a path, it does not remove one.
- When the variable is **set**, the orchestrator must treat the pointed-to file as
  the intended runner and **must verify it is compatible before using it** (below).
  A set-but-unusable value is a configuration error to surface, never a reason to
  quietly launch without containment.

## What a "compatible file" is

A file is a compatible launch target only when **all** of the following hold:

1. **It exists** at the given path.
2. **It is executable** — the OS can spawn it.
3. **Its compatibility surface matches** what the consumer needs, across the three
   dimensions this project versions as one surface:
   - **CLI surface** — the subcommands and flags the consumer will invoke exist
     (for example `run`, `run:--jsonl`, `run:--capture-dir`).
   - **Exit-code band** — the reserved runner exit-code band is the one the
     consumer decodes (`100`–`119`; see [exit-codes.md](exit-codes.md)).
   - **JSONL `schema_version`** — the event schema the consumer parses is the one
     the binary emits (see [schema.md](schema.md)).

The probe below reports all three and, on request, verifies them.

## The preflight probe

```
processkit-cli probe --json
                     [--require-schema-version <N>]
                     [--require-exit-code-band <start>-<end>]
                     [--require-surface <token>]...
```

`probe` is the mechanism a consumer uses to check a candidate file **before**
launching a payload. The consumer resolves the path from `CC_PROCESSKIT_RUN` and
runs `"$CC_PROCESSKIT_RUN" probe --json` (optionally with `--require-*` flags).

- **No side effects — never a real contained process.** A probe spawns no child,
  opens no run registry, binds no control endpoint, and creates no container. It
  reads compile-time facts and the in-memory CLI definition, prints one line, and
  exits. Running it as a preflight is therefore safe, with none of a real `run`'s
  effects.
- **Deterministic and machine-readable.** It prints exactly one JSON object (one
  line) to **stdout**, in both the compatible and the incompatible case, so the
  consumer always has a parseable result. Two runs of the same binary print the
  same bytes.
- **Fixed form.** `--json` is required (it is the only output format, as for
  `inspect`).

### The report

| Field            | Type              | Notes                                                                                 |
|------------------|-------------------|---------------------------------------------------------------------------------------|
| `probe_version`  | integer           | The report's own format version (currently `1`), versioned independently of the JSONL `schema_version`, the control-plane `snapshot_version`, and the `registry_version`. |
| `binary`         | string            | The binary's package name (`processkit-cli`) — confirm the candidate is this runner and not another program that happens to accept a `probe` argument. |
| `version`        | string            | The binary's semantic version (`CARGO_PKG_VERSION`).                                   |
| `schema_version` | integer           | The JSONL event `schema_version` this binary emits (see [schema.md](schema.md)).       |
| `exit_code_band` | object            | The reserved runner exit-code band: `{"start":100,"end":119}` (see [exit-codes.md](exit-codes.md)). |
| `surface`        | array of string   | The CLI surface **tokens** this binary exposes: every subcommand name and every subcommand long flag as `<sub>:--<flag>` (e.g. `run:--capture-dir`, `inspect:--json`), sorted. Derived from the live parser, so it never drifts from the real CLI. |
| `compatible`     | boolean           | Whether every requested `--require-*` expectation is satisfied. `true` when none was requested (a healthy self-report). |
| `mismatches`     | array of string   | One human-readable reason per unmet expectation. Always present; empty when `compatible` is `true`. This is the parseable "why", so an incompatibility is never a generic error. |

Example (compatible self-report):

```json
{"probe_version":1,"binary":"processkit-cli","version":"0.1.0","schema_version":1,"exit_code_band":{"start":100,"end":119},"surface":["cancel","cancel:--run-id","inspect","inspect:--json","inspect:--run-id","kill","kill:--run-id","probe","probe:--json","probe:--require-exit-code-band","probe:--require-schema-version","probe:--require-surface","run","run:--argv-raw","run:--capture-dir","run:--create-no-window","run:--cwd","run:--grace","run:--jsonl","run:--run-id","run:--timeout"],"compatible":true,"mismatches":[]}
```

### Verifying compatibility

A consumer has two equivalent ways to reach a verdict; either is fine, and a
robust consumer may combine them:

- **Read the report and compare fields itself** — e.g. assert `binary` is
  `processkit-cli`, `schema_version` is the one it parses, `exit_code_band` is
  `100`–`119`, and `surface` contains every token it will use.
- **Let the probe verify, via `--require-*`** — pass the expectations and read a
  single exit-code verdict, no JSON parsing required:
  - `--require-schema-version <N>` — the emitted `schema_version` must equal `<N>`
    **exactly** (adapters pin one version; a different one is a breaking change).
  - `--require-exit-code-band <start>-<end>` — the reserved band must be exactly
    that (e.g. `100-119`). A malformed value is a usage error (exit `100`).
  - `--require-surface <token>` — the token must be present in `surface`
    (repeatable). Covers both subcommands (`probe`) and flags (`run:--jsonl`).

  If any expectation is unmet, `probe` prints `compatible:false` with the concrete
  `mismatches` and exits with the reserved **`PROBE_INCOMPATIBLE` (110)**.

## Fail-closed: the three outcomes

When a consumer runs the probe on the `CC_PROCESSKIT_RUN` candidate, exactly one of
four outcomes results. The **three failure outcomes are each distinct and
parseable**, and every one of them is **fail-closed**: the consumer must surface the
problem and refuse to launch, and **must never silently fall back to an uncontained
launch path**. A silent fallback would reintroduce precisely the process-leak hazard
this project exists to prevent — here, a failure is safer than a silent degrade.

| # | Candidate condition | How the consumer detects it | Outcome | Consumer action |
|---|---------------------|-----------------------------|---------|-----------------|
| 1 | **Path missing** — no file at the path | The **spawn** fails before the probe runs, with `NotFound`. | `missing` | Fail closed. Report the misconfiguration; do **not** launch uncontained. |
| 2 | **Present but not executable** — the file exists but cannot be run | The **spawn** fails with an error **other** than `NotFound` (Unix: `PermissionDenied`; Windows: the loader rejects a non-executable file). | `not-executable` | Fail closed. Distinguished from `missing` by the OS error. |
| 3 | **Present and executable but incompatible** — it runs, but its surface does not match (or it is not `processkit-cli` at all) | The probe exits `110` and prints `compatible:false` with `mismatches`; **or** the process is not a valid probe — a non-zero-non-`110` exit, a non-JSON/absent report, a missing/mismatched field, or an unknown-subcommand/usage error from a binary too old to have `probe`. | `incompatible` | Fail closed. Never treat an unparseable or wrong-surface report as "ok". |
| 4 | **Compatible** — an executable `processkit-cli` whose surface matches | The probe exits `0` and prints `compatible:true` with the expected `version`, `schema_version`, band, and surface. | `ok` | Proceed: launch payloads through this binary. |

Notes for a robust consumer:

- Treat outcome 3 as the catch-all for "the file ran but is not a runner I can
  trust": a binary predating the `probe` subcommand fails `probe` with a usage or
  unknown-subcommand error, which is itself an `incompatible` signal — fail closed.
- The exit code is a best-effort shell signal; the authoritative detail is the JSON
  report (`compatible` + `mismatches`), mirroring how the exit-code band defers to
  the JSONL `runner_exit` event (see [exit-codes.md](exit-codes.md),
  "Why a band is not enough on its own").

## Consumer migration and rollback order

The contract is designed to be adopted, and abandoned, safely:

1. **Before enabling** — ship/point `CC_PROCESSKIT_RUN` at the intended binary, but
   keep the previous discovery method active. Nothing changes yet.
2. **Verify** — run `"$CC_PROCESSKIT_RUN" probe --json` with the `--require-*`
   expectations the orchestrator needs (its parsed `schema_version`, the `100-119`
   band, and the surface tokens for the flags it uses). Only outcome `ok` (exit `0`,
   `compatible:true`) clears the candidate.
3. **Enable discovery** — once the probe reports `ok`, route contained launches
   through `CC_PROCESSKIT_RUN`. Re-run the probe as a preflight (for example at
   startup or when the value changes); on any non-`ok` outcome, fail closed and stay
   on — or revert to — the previous method rather than launching uncontained.
4. **Roll back** — if the new binary proves unsuitable, unset `CC_PROCESSKIT_RUN`
   (or point it back at a known-good binary). With the variable unset the
   orchestrator returns to its prior discovery method unchanged. Because this
   contract is purely additive — it introduces a variable, a subcommand, and one new
   reserved exit code, and changes no existing flag, code `100`–`109`, or
   `schema_version` — rolling back never disturbs any previously shipped behavior.

## Stability

- `CC_PROCESSKIT_RUN`, the `probe` subcommand and its report shape (`probe_version`),
  and the `PROBE_INCOMPATIBLE` (110) code are additive extensions of the
  compatibility surface. A breaking change to the report's shape is a **major** bump
  of `probe_version`; a new report field or a new `--require-*` check that does not
  change an existing shape is additive.
- The probe never changes the meaning of any already-shipped surface element (the
  existing flags, codes `100`–`109`, or `schema_version: 1`).
