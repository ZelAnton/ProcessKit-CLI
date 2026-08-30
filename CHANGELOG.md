# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Add entries to `[Unreleased]` as you work — manual bullets always win over the
git-cliff auto-fill (config: `cliff.toml`). On release, promote `[Unreleased]`
to a dated version section.

## [Unreleased]

### Added
-

### Changed
-

### Fixed
-

## [0.3.4] - 2026-08-30

### Added
-

### Changed
- **Windows executables now carry the C runtime inside them.** The published
  `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` binaries no longer need
  the Microsoft Visual C++ Redistributable on the machine that runs them.
  `.cargo/config.toml` sets `-C target-feature=+crt-static` for both targets.

### Fixed
-

## [0.3.3] - 2026-08-22

### Added
-

### Changed
- **`processkit` 3.3.4** is now resolved by both committed lockfiles while the
  manifest's `"3.3"` requirement and MSRV remain unchanged. The new upstream
  `TeardownCause` vocabulary is explicitly recorded as not projected: this CLI
  races its own timeout/cancel deadlines and uses `ProcessGroup::start` (a
  launch-only surface), while direct `stop`, `kill_all`, and `members_info`
  operations expose `ErrorReason::Io`; no `ErrorReason::Teardown` value reaches
  this runner's exit-code decision or widens its machine contract. `members_info`
  read failures continue through the existing honest `read_error` degradation,
  including Windows metadata-snapshot failures.

### Fixed
- `cleanup_finished` now carries an additive `kill_error` qualifier when the
  container hard kill fails, so a successful empty member read can no longer
  masquerade as confirmed-clean teardown. The hard-kill failure remains non-fatal:
  foreground runs keep the stderr warning and the child's exit code stays unchanged.

## [0.3.2] - 2026-08-09

### Added
- **Upstream identifier drift gate**: a new opt-in test tier (`tests/spec_drift.rs`,
  the `spec-drift` Cargo feature) holds every projection of a ProcessKit closed enum
  against the stable-identifier dictionary ProcessKit ships inside its own package
  (`spec/identifiers.json`), for the exact version `Cargo.lock` resolves. It exists
  because the previous entry was found the slow way: `Mechanism::ProcessReaper` was
  added upstream, this crate compiled unchanged, every test stayed green, and the new
  mechanism reported itself as the projection's `unknown` fallback — a gap between
  "upstream grew a value" and "we noticed" that was bounded by nothing. The gate
  covers the seven vocabularies this CLI republishes (`Mechanism`,
  `ParentDeathCleanup`, `LimitKind`, `LimitVerdict`, `SoftStopScope`, `SoftSignal`,
  `Outcome`) and checks each identifier on two surfaces: the Rust projection that
  renders it — driven with the real variant, so a value falling into the conservative
  fallback arm fails rather than passes — and every published JSON Schema `enum` that
  carries it, located by property name across `fixtures/schema/v1/schema.json` and
  `fixtures/schema/cli/*.schema.json` so a value added to the event schema but
  forgotten in the `inspect`/`attest`/`doctor` mirrors is caught too. Every remaining
  dictionary enum must be recorded as not projected, with the reason, so one added
  later cannot go unclassified. A failure names the enum and the identifier and
  changes nothing on its own: what a new upstream value means for a published
  contract is a decision, and this gate's whole job is to make sure it gets made.
  The dictionary is located through `cargo metadata`'s resolve graph — no network, no
  vendored copy — which also means the gate follows a `[patch.crates-io]` git
  checkout, so the scheduled upstream canary now sees a new identifier on ProcessKit's
  main branch rather than waiting for the release that would deliver it. A missing dictionary (a patched, vendored, or
  path dependency whose tree omits `spec/`) fails loudly and is never skipped: a green
  "no drift found" that actually means "nothing was checked" would reproduce exactly
  the blindness the tier removes. The tier is off in the default `cargo test` — it
  needs a working cargo and the dependency's unpacked source, and its verdict is
  host-independent — and runs in the new gating `spec-drift` CI job, in `canary.yml`
  against ProcessKit's main branch, and as `just spec-drift` locally. Documented in
  CONTRIBUTING.md, "Upstream identifier drift" (including what to do when it fails),
  and for adapter authors in `docs/compatibility.md`. No wire, flag, exit-code, or
  schema change: `events::abrupt_cleanup_str` gained a pure
  `abrupt_cleanup_scope_str(scope)` inner projection (so every arm can be driven on
  any host, not only the one the machine happens to report), the soft-signal fate and
  soft-stop scope projections became named functions instead of an inline match and an
  inline `.name()`, and four `run` projections are re-exported for the gate to drive.
- **`process_reaper` containment vocabulary**: project ProcessKit 3.3's FreeBSD
  process-reaper mechanism into `run_started`, `inspect`, `attest`, and `doctor`,
  with schema-v1 fixtures and validators accepting `process_reaper` plus the
  intentional `unknown` fallback. FreeBSD is documented separately from the
  POSIX process-group fallback: it has whole-tree kill and membership semantics
  but no resource-limit or statistics support, while abrupt owner death remains
  `none`.
- **`resource_summary`**: a new terminal JSONL event reporting what the contained tree
  actually **consumed** — `peak_memory_bytes`, `total_cpu_ms`, `io_read_bytes`,
  `io_write_bytes`, and `peak_process_count`. Until now the stream said *nothing* about
  resource usage: `members_snapshot` carried only `pid`/`ppid`/`name`/`start_time`, so
  peak memory and total CPU were neither readable nor reconstructible from it. The
  runner takes one `ProcessGroup::stats()` reading of what the active mechanism accounts
  for (via the `processkit` `3.2` → `3.3` bump) after the ending is decided and before
  `cleanup_finished` hard-kills the group — the same read point as `limit_evidence`, and
  immediately after it, because both facts live *in* the container and vanish with it.
  It is emitted by **every** run that spawned a child: no flag, no cap required, every
  platform, every ending (natural exit, timeout, cancel, kill, and the `foreground`
  container failure alike), exactly once. Choosing "always" over an opt-in flag is
  deliberate and recorded in `docs/schema.md`: this is one synchronous read of
  accumulators the kernel already keeps rather than a sampling cadence, so a flag would
  save one syscall and one line while leaving the platform that needs the numbers most
  — Windows, where `limit_evidence` can only ever answer `unknown` for a capped axis —
  silent unless a caller knew to ask. The honest cost is that a default run's stream is
  now **seven** lines rather than six, and this is the one growth a caller did not opt
  into; the line-count arguments in `docs/integration.md` and ADR 0007 are restated
  against the new count. Each measurement is **independently nullable**, and `null` always means
  *this mechanism does not account for it* — never a stand-in `0`, and never a value
  improved by taking a maximum over the runner's own periodic reads, which would
  describe when the runner looked rather than what the tree did. Consequently
  `peak_process_count` is **always** `null` on Windows (a Job Object keeps
  `ActiveProcesses` and `TotalProcesses`; neither is a peak), both IO counters are
  always `null` on macOS/the BSDs and the Linux process-group fallback, and on Linux
  they need the cgroup v2 `io` controller — which this CLI never enables, since
  `processkit` enables exactly the controllers a requested cap needs. On **Linux cgroup
  v2** the availability of `peak_memory_bytes`/`total_cpu_ms` is a property of the read
  point rather than of the controller set, and the documentation says so instead of
  implying completeness: those two are summed from `/proc` over the members live when
  `stats()` runs (the cgroup keeps no CPU/memory accumulator this backend reads), so a
  run that ended by its child exiting — the commonest ending — reports both as `null`
  with `read_error: false`, a run whose child leaked a surviving descendant reports
  numbers covering only that survivor, and only a runner-imposed ending (`timeout` /
  `cancelled` / `killed` / `output_overflow`, read before the soft stop) reports the
  whole tree. Windows is unaffected: a Job Object's accounting block outlives the
  processes charged to it. The two
  platforms' IO counters are explicitly **not comparable with each other** (a Job
  Object counts all read/write traffic whatever the target; cgroup `io.stat` counts only
  what crossed the block layer), and `total_cpu_ms` truncates, so a run using under a
  millisecond of CPU reports a *measured* `0` — `null` remains the only value meaning
  unknown. The normative platform matrix, and what this event does **not** prove about a
  limit, are in `docs/resource-limits.md`, "What the tree consumed".
  `active_process_count` is deliberately **not** on the event: it is a "how many right
  now" reading taken after the ending is decided, so it would report the moment the
  runner looked, and the tree size at teardown already has an honest home in
  `cleanup_started.members_before`. A failed `stats()` read does **not** skip the event:
  it is emitted with `read_error: true` and every measurement `null`, mirroring
  `members_snapshot`/`cleanup_started`/`cleanup_finished`'s existing
  honest-degradation flags — and here the flag is load-bearing rather than ceremonial,
  because an all-`null` summary is *also* a correct success — on a mechanism with no
  whole-tree accounting, and equally on a flagless Linux cgroup v2 run that ended by its
  child exiting — so nothing else could distinguish a gap from a platform fact.
  The event is additive within `schema_version = 1`: no existing field was renamed,
  retyped, or given a new meaning, and the golden fixture gained one appended line with
  every prior line byte-for-byte unchanged. `events --validate` accepts a conforming
  `resource_summary` (including the fully degraded shape) and rejects a corrupted one,
  and `probe --json` publishes a new **non-flag capability token**,
  `run:resource-summary`, so an adapter that will read the summary can pin the event at
  preflight (`probe --json --require-surface run:resource-summary`) instead of
  discovering an older binary's silence only after a run has finished without it. That
  token's presence guarantees the *event*, not any particular measurement in it — which
  axes carry numbers follows the mechanism named by `run_started.mechanism`. It is the
  second token in the `--`-less capability form after `attest:peer-identity`, and the
  first whose absence is a matter of build version rather than platform.
- `doctor [--json]`: a **runtime qualification of the host**, and the side-effecting
  counterpart to `probe`. `probe` proves *this binary* exposes the surface a consumer
  needs — it reads compile-time constants and the in-memory CLI tree, spawns nothing,
  and touches no registry, container, or transport, which is exactly what makes it
  safe as a per-launch preflight and exactly why a passing probe is not evidence that
  a *run* will work here. A binary can satisfy every `--require-*` check and still
  fail its first real run on a registry directory it cannot create, a containment
  mechanism the kernel will not hand out, or a local IPC endpoint that will not bind.
  `doctor` closes that gap by doing the thing: it performs a bounded scratch `run` of
  this binary's own harmless child (the new report-replacing
  `doctor --scratch-child <duration>`, which sleeps and does nothing else — published
  rather than hidden, so the claim that a qualification contains only this binary's
  own code can be checked, and refused by clap in combination with any other flag so
  a requested qualification can never be silently replaced by a sleep), drives that
  run as an ordinary control-plane client, and reports the facts it observed: the
  registry directory and its owner-only protection (re-read from the filesystem, by
  the same predicate the registry's own tests use), the containment `mechanism` and
  `abrupt_cleanup` level this machine really gives a run, an `inspect`/`cancel`/
  terminal-wait round-trip over the local transport, a **confirmed**-empty teardown
  (`read_error` and the remaining members, not one boolean), optionally the
  whole-tree resource controller (`--check-resource-controller`), and per-phase
  `elapsed_ms` so a slow host is diagnosable rather than a generic hang. It
  reimplements none of it — every phase drives the same production code a caller's
  own `run` and control clients take — which is what makes a pass evidence about
  *this* containment path on *this* host. On success every scratch artifact is gone
  and the report says so, having checked each; on a failed phase a **named**
  diagnostics directory is kept (`diagnostics_dir`) holding the scratch run's JSONL
  stream, the runner's stdout/stderr, and a copy of the report. The requirement flags
  (`--require-mechanism`, `--require-abrupt-cleanup`, `--require-resource-controller`)
  gate the **exit code only** — the new reserved `HOST_UNQUALIFIED` = `116`, the
  host-side twin of `PROBE_INCOMPATIBLE` (`110`) — while the report carries the
  observed facts either way; a matching `--error-format json` kind
  (`host_unqualified`) accompanies it. The report is published as a ninth
  machine-output family (`fixtures/schema/cli/doctor.schema.json` + `doctor.jsonl`)
  carrying its own `doctor_version` (currently `1`), versioned because a
  qualification report is *kept*: the failure path writes it into the diagnostics
  directory precisely so it can be read later, elsewhere, by whoever debugs the host
  rather than by whoever ran the command. See `docs/troubleshooting.md`, "Qualifying
  a host: `doctor`", `docs/integration.md` §1, and `docs/exit-codes.md`.
- `attest --run-id <id> [--json]`: a read-only control-plane command that asks a live
  run whether **the calling process** is inside its ProcessKit container. The runner
  takes the caller's identity from the control transport itself — unix socket peer
  credentials (`SO_PEERCRED` on Linux, `LOCAL_PEEREPID` on macOS, `LOCAL_PEEREID` on
  NetBSD, `getpeerucred` on Solaris/illumos) or `GetNamedPipeClientProcessId` on the
  Windows named pipe — reads it *while the connection is open* (so pid reuse cannot
  turn a departed client into a false positive), and checks it against the run's own
  live container membership through the same `members_info()` path `inspect` and the
  JSONL `members_snapshot` already use. **There is deliberately no `--pid` and no
  `--all`:** a caller-supplied pid would only prove that some chosen process is a
  member, which says nothing about the asker. This turns an adapter's
  environment-string convention ("the caller belongs to run X") into an invariant the
  runner checks. Three outcomes: `member` (exit `0`), `not_a_member` (the **new**
  reserved exit code `NOT_A_MEMBER` = `115`), and `peer_identity_unsupported` (the
  existing `CONTROL` = `103`) — a platform that cannot obtain a kernel-authenticated
  peer identity **fails closed** rather than degrading to an unproven "ok", and a
  consumer rules that out at preflight with the new capability token
  `probe --json --require-surface attest:peer-identity`. Two new `--error-format json`
  kinds accompany them (`not_a_member`, `peer_identity_unsupported`). The attestation
  is published as an eighth machine-output family
  (`fixtures/schema/cli/attest.schema.json` + `attest.jsonl`) carrying its own
  `attestation_version` (currently `1`), which the client checks strictly — a security
  verdict is refused rather than read under semantics its sender never promised.
  Scope, honestly stated: this is a **containment fact inside the existing
  same-OS-user threat model, not authentication between hostile peers**, and what
  `member` covers follows the run's containment mechanism (whole-tree for a Job
  Object or cgroup; the process group for the POSIX fallback, which enumerates only
  its leaders) — see `docs/control-plane.md`, "`attest`", and `docs/threat-model.md`.
- `run --run-id-env <KEY>`, an opt-in flag that sets one child environment variable
  to the run's **final** id — the explicit `--run-id` when one was given, otherwise
  the id the runner generated — so a child and its descendants can name the run
  they belong to. The value is the same one `run_started.run_id`, the registry
  record, and every control-plane reply carry. It replaces the "mint an id yourself
  and pass it twice" plumbing (`--run-id <id> --env KEY=<id>`) every supervising
  adapter carried: one value instead of two copies that can drift, and the only way
  to hand the child a *generated* id, which was previously not knowable outside the
  run until the run had already started. Strictly opt-in — no key is injected by
  default, so a run without the flag has exactly the child environment it always
  had. The injection is applied **after** all four `--env-*` flags (`--env-clear`,
  `--env-remove`, `--env-file`, `--env`), so it wins over a file entry or a removal
  of the same key whichever order the flags were written; the one combination that
  is *refused* rather than resolved is an explicit `--env <KEY>=…` for the same key,
  which fails at parse time as a `USAGE` (100) error before anything runs (asking
  for two values of one variable is a caller mistake, and the outcome must not
  depend on argument order either way); "the same key" follows the platform's own
  rule, so on Windows — where environment names are case-insensitive — a pair
  differing only in case is that same collision and is refused too. `<KEY>` is
  held to the same rule as an
  `--env` KEY, through the same validator. The value is **correlation data, not a
  credential**: it identifies a run, proves nothing about who started it, and is
  forgeable by anything that can set an environment variable. New probe surface
  token `run:--run-id-env`. See `README.md`, "Environment", and
  `docs/running-commands.md`, "Publishing the run id to the child".
- `--error-format <human|json>`, the CLI's first **global** option: accepted before
  or after the subcommand, honored by every subcommand, and off by default. Under
  `--error-format json` a post-parse failure prints exactly one bounded, versioned
  JSON object on **stderr** instead of the `processkit-cli: <message>` prose —
  `error_version`, `code`, `kind`, `operation`, `run_id`, `retryable`, `message` —
  so an adapter branches on a published `kind` rather than on English. The point is
  that an exit code is coarse: `CONTROL` (103) alone covers eight situations, and
  `kind` splits it into `not_found` / `stale` / `unprobed` / `ambiguous_run_id` /
  `control_unreachable` / `ipc_deadline` / `incompatible_contract` /
  `peer_identity_unsupported` (and splits
  `SETUP` 111 into `registry` versus `setup`). **No exit code was minted or
  changed**: the taxonomy is a finer axis over the existing band, and for a failing
  `run` its values are the terminal `runner_exit` event's own `source` spellings
  rather than a second vocabulary for the same endings. A `run --detach` still relays
  the reserved *code* of the copy it respawned, but not a meaning for it: a code
  `run` itself never mints (`110`/`112`/`114`/`115`/`116`, or one no build assigns
  yet) reports `kind: "unknown"` rather than borrowing another subcommand's
  verdict, since that copy can be a different build. `message` is deliberately
  *not* part of the contract and may be reworded in any release. Invariants: stdout
  is never touched (a command that prints a report and then fails, like
  `probe --json` exiting 110, still prints exactly what it always did), the default
  stderr prose is byte-for-byte unchanged, and the exit code is unchanged. The
  shape is published as `fixtures/schema/cli/error.schema.json` with a golden
  `error.jsonl` beside it and its own `error_version` (currently `1`) — a
  versioned family in that directory, for the reason the others are versioned: a
  captured stderr line is routinely read out of its invoking context. One
  documented gap: clap's *parse-time* usage errors (exit 100) stay human-readable
  in v1, since they happen before the binary knows what it was asked to do. See
  `docs/exit-codes.md`, "Machine-readable failures: `--error-format json`", and
  `docs/integration.md` §7.
- `events`, a read-only subcommand that reads a run's JSONL lifecycle stream back:
  it resolves the stream through the per-user registry (`--run-id`, the same
  locator `list --json` publishes) or takes an explicit `--file <events.jsonl>` for
  a stream whose registry record is already gone, then renders each event for a
  human (default), passes the runner's own lines through byte for byte (`--json`),
  follows a growing stream to its terminal `runner_exit` (`--follow`), or checks
  every line against the event schema this binary embeds (`--validate`). Like
  `list`/`wait` it opens the registry read-only, never contacts a run's control
  transport, and mutates nothing.
- `EVENTS_INVALID` (`114`), the reserved exit code `events --validate` returns when
  a checked stream does not conform to that schema — a verdict about a document,
  distinct from `SETUP` (111) for a stream that could not be read at all and from
  `CONTROL` (103) for a `--run-id` that names no single stream. Codes `117`–`119`
  remain reserved (`115` is now `NOT_A_MEMBER`, see `attest` above; `116` is now
  `HOST_UNQUALIFIED`, see `doctor` above). The check adds **no runtime
  dependency**: it interprets the embedded schema document over the keyword subset
  that document uses, refuses to run on anything it does not implement, and is held
  to a real JSON Schema engine's verdict — line for line, over the golden fixture
  and a generated mutation corpus — by the test tier.
- `run --snapshot-interval <duration>`, an opt-in cadence that re-emits the
  `members_snapshot` lifecycle event while the child runs, so a long, quiet, or
  detached run records how its process tree evolved instead of only its shape at
  spawn. The event gained two always-present fields — `reason` (`spawn` for the
  post-spawn snapshot every run emits, `interval` for a re-sample) and
  `read_error` — on **every** run, flagged or not; both are additive schema v1
  changes, like the `timeout` event's own `reason`, but an adapter that pinned
  this event's exact field set rather than the fields it reads will see them on
  the default path too. The cadence samples the container's member list rather
  than the output pump, so it composes with `--inherit-stdio`, is forwarded by
  `--detach`, and stops as soon as the run's ending is decided, so no snapshot
  ever lands in the teardown tail. The stream it produces is deliberately
  unbounded (`duration / interval` lines); `docs/running-commands.md` records
  that decision with the sizing arithmetic for choosing an interval.
- `members_snapshot` now reports a failed member read in the stream instead of
  skipping the sample: the event is emitted with `read_error: true` and an empty
  `members` array, matching `cleanup_started`/`cleanup_finished`'s existing
  `read_error` convention. The previous stderr-only warning could not reach a
  detached run's operator at all (its stderr is `null`), which left a failed
  sample indistinguishable from an unchanged tree in the one artifact such a run
  has. As a side effect the post-spawn `members_snapshot` now appears exactly
  once in every stream, as `docs/schema.md`'s ordering contract states, where a
  failed read previously removed it.
- Checksum-derived winget, Scoop, and Homebrew distributor manifests attached
  to every release after the platform archives finish uploading.
- Automatic publication of those Homebrew and Scoop files into the project's own
  tap/bucket repositories, as the release workflow's last job. It is off until
  an operator creates the target repository and adds a token secret scoped to it
  (`HOMEBREW_TAP_TOKEN` / `SCOOP_BUCKET_TOKEN`, each channel independent), skips
  with a notice while unconfigured, pushes nothing when the target already holds
  identical bytes, and cannot fail a release even when a configured channel
  breaks. No tap or bucket is published yet, so every install command stays as
  documented in the package-manager availability table; winget remains a
  deliberate manual submission, since its review lives in `microsoft/winget-pkgs`
  and no automated step there could report real availability.
- An adoption-oriented positioning guide comparing ProcessKit CLI with common
  deadline, process-group, service-manager, container, init, and PowerShell
  alternatives without overstating the platform-specific cleanup guarantees.
- A ProcessKit-family mdBook documentation site, including the shared cover and
  theme, rendered-link validation, and GitHub Pages deployment from `main`.
- A user-focused Pages guide set covering installation, cookbook workflows,
  command execution, I/O and bounded capture, detached runs, timeouts,
  resource limits, platforms, containers, compatibility upgrades, and robust
  external-process execution from automation agents.
- `run --windows-graceful-ctrl-break`, an opt-in cooperative `CTRL_BREAK` tier for
  Windows console children before Job Object escalation.
- Structured `cleanup_finished.shutdown` observations from ProcessKit's pre-stop
  capability probe and `ShutdownReport`.
- `run --env-file` for pre-spawn UTF-8 environment files whose values stay out of
  the runner's argv, with explicit `--env` overrides.
- Operator `run --label` metadata in lifecycle events, registry discovery, and
  conjunctive `--label` filters for `cancel --all`, `kill --all`, and `wait --all`.
- A POSIX PTY e2e assertion that verifies foreground process-group restoration
  after an inherited-stdio run returns.
- `cargo-binstall` metadata for one-command installation from the existing
  prebuilt GitHub Release archives on every published target.
- A scheduled, manually dispatchable, non-gating canary that builds and tests
  against ProcessKit's current git `main` on Linux and Windows.
- Exact-match `list --label` filters and `list --health live|stale|unprobed`,
  composable across human and JSON discovery output.
- Absolute JSONL and optional capture-directory locators in the owner-only registry,
  `list`, and inspect snapshot, completing detached-run artifact discovery.
- Fleet-wide `inspect --all --json` with conjunctive label filters and one
  snapshot-addressed result per live run, including honest per-run errors.
- Opt-in `run --capture-overflow cancel` protection for runaway output, with an
  additive `output_overflow` JSONL event, graceful teardown, and reserved code 113.
- Per-commit Criterion history artifacts and automatic same-OS comparison against
  the latest successful `main`, with non-gating warnings above a 20% median increase.
- Checksum-verifying `install.sh` and `install.ps1` one-command installers with
  platform detection, version pinning, custom destinations, and safe overwrite refusal.
- An installable `using-processkit-cli` agent skill with Codex metadata, a Claude
  Code marketplace entry, contained-run recipes, and live contract drift tests.
- An indexed ADR journal with a reusable template and six retrospective records for
  the project's settled stream, redaction, control, cleanup, shell, and wait choices.
- ADR 0007 and an integration-guide section recording the decision **not** to add a
  terminal receipt file (`run --outcome-json`): `run` keeps one durable outcome
  artifact, the required `--jsonl` stream, and an adapter that wants a reserved-band
  exit code disambiguated without opening it reads the `--error-format json` envelope
  instead — present for a runner-owned ending, absent for the child's own exit. No
  flag, event, schema, or exit code changed.
- Cross-platform runnable examples for compatibility preflight, foreground event
  parsing, detached supervision, and label-scoped fleet cancellation, smoke-tested in CI.
- `wait --report-outcome` for a single observed run, returning terminal
  `runner_exit` fields as one JSON object without changing the waiter's exit code.
- A default human-readable `inspect --all` report with per-target status rows and
  expanded snapshots, while preserving the original `--json` array.
- Conjunctive `prune --label KEY=VALUE` filtering for scoped real and dry-run
  cleanup, conservatively excluding ownerless orphan locks when filtered.
- A seventh release target, `aarch64-unknown-linux-musl`, for a single
  dependency-free binary on Arm64 containers (Alpine/distroless, Graviton,
  Apple-Silicon Docker hosts), built and test-executed natively on a
  GitHub-hosted `ubuntu-24.04-arm` runner, with matching `install.sh`
  `--target` support, package-manifest generation (Homebrew Linux Arm64), and
  documentation.
- A phase-attribution benchmark for the mutating owner-only registry open, swept
  over registry sizes so the Windows DACL propagation cost is measured in-repo
  rather than inferred from an end-to-end startup number.

### Changed

- **`processkit` 3.3.1** is now the version both committed lockfiles resolve — the
  root `Cargo.lock` and `fuzz/Cargo.lock`, the second belonging to the deliberately
  out-of-workspace fuzz crate that the root verification commands never descend
  into and that Cargo has no reason to move on its own, since 3.3.0 already
  satisfied the requirement. The manifest requirement is deliberately **unchanged**
  at `"3.3"`: this is a patch inside the line that requirement already admits, and
  the comment beside the dependency names `3.3` for a lower bound
  (`limit_evidence()`/`LimitVerdict`, and `ProcessGroupStats`' `io_*`/
  `peak_process_count` fields) that 3.3.1 does not move. Of the release's three
  fixes, two provably do not reach this crate: ConPTY appears nowhere in `src/`, and
  the upstream `Pipeline` API is unused (every "pipeline" in the tree is the label
  *value* `pipeline=ci`/`pipeline=local` in tests, or one doc-comment mention of a
  shell pipeline). The upstream stable-identifier dictionary
  (`spec/identifiers.json`) is byte-identical between 3.3.0 and 3.3.1, so no
  projected vocabulary — `Mechanism`, `ParentDeathCleanup`, `Outcome`, the limit
  verdicts — drifted.
- **A hard kill that reports a failure is no longer discarded** in
  `cleanup_finished`. This is the one behavioural consequence of adopting
  `processkit` 3.3.1, and tracing it corrected the expectation it was adopted
  under. The release makes the Linux legacy/restricted-cgroup teardown report a
  **refused thaw** — the per-pid `SIGKILL` sweep freezes the subtree so a fork bomb
  cannot out-spawn it, and if the freeze cannot be cleared afterwards the tree is
  dead but the cgroup is left frozen and unusable for further spawns — where it
  previously returned `Ok(())` on the strength of an empty `cgroup.procs`. The
  concern was that this new failure would slide through `map_launch_error`'s
  wildcard arm and turn runs that used to exit `0` into `BACKEND` (102) on such
  hosts. **It cannot**: `map_launch_error` is reached only from
  `ProcessGroup::start`, so it maps *launch* failures, and no teardown-path error
  reaches the exit code at all. `kill_all`'s result was discarded outright
  (`let _ =`), and `stop`'s `Err` is already downgraded to a stderr warning plus
  `soft_signal: "failed"` while the code stays `TIMEOUT`/`CANCELLED`/etc. **No exit
  code, event, flag, or schema changes here, and `docs/exit-codes.md` is
  deliberately untouched** — its `BACKEND` (102) wording, which describes a
  container or registry that "could not be established", remains accurate, because
  all four sites that mint 102 (`create`, `attach`, and the two `foreground` ones)
  still sit before `run_started`. What the trace *did* expose is a diagnostics gap:
  with the error dropped, `cleanup_finished` reported `remaining: 0,
  read_error: false` — a **confirmed**-clean teardown — over precisely the state
  upstream had just refused to call one, since the group's own drop hits the same
  refused write and the post-kill member read cannot see a freezer at all. The kill
  result is now projected through a pure `hard_kill_warning` and, when it reports a
  failure, warned on stderr carrying upstream's message verbatim — that text, naming
  the cgroup "left FROZEN" with the refusal's errno and remedy, is what tells this
  case apart from the pre-existing undrained-tree one. It stays non-fatal on
  purpose: both classes are properties of the host's teardown rather than of the
  child's work, and forwarding the child's exit code faithfully is this runner's
  central promise. The projection is unit-tested on every host; the end-to-end
  condition is **not reproducible in CI** and is not claimed to be — it needs a
  Linux host refusing both `cgroup.kill` and `cgroup.freeze` (a pre-5.14 kernel or a
  revoked delegation), which upstream itself reaches only through crate-internal
  fault injection unavailable to dependents.
- The threat model now enumerates the events file read back by `events` as a
  fourth untrusted-input surface — naming `events --file`'s arbitrary
  caller-specified path, the hand-rolled line reader, schema interpreter, and
  pattern matcher sitting on it, and the terminal barrier every operator string
  crosses — and states explicitly that the `cargo-fuzz` tier covers `wait
  --report-outcome`'s read-back but **not** those parsers, so the document no
  longer implies more coverage than exists.
- `docs/compatibility.md`'s "Schema pinning" section now states the full set of
  changes a reader must tolerate within one schema version — new event types,
  repeats of an event type that previously occurred at most once, new fields
  including always-present ones, new values in open-ended string fields, and
  unknown fields — and names `docs/schema.md` as the normative source for all of
  them. It previously mentioned only "additive optional fields and unknown event
  fields", which covered neither multiplicity nor always-present fields.
- The zero-duration rejection message shared by `--timeout`, `--idle-timeout`,
  `wait --timeout`, and `--snapshot-interval` no longer describes only the
  deadline case ("tearing the child down immediately after spawn … omit the flag
  to leave it unbounded"), which was misleading for a rejected cadence.
- `inspect` and `inspect --all` now check the `snapshot_version` a runner declares
  instead of rendering whatever arrives. A runner answering with a version **newer**
  than the invoked binary implements is refused with `CONTROL` (103) — for `--all`,
  as a per-target `failed` entry — with a message naming the version that arrived and
  the range this build reads; previously such a reply was printed under this build's
  semantics, silently dropping whatever the newer runner added. Older runners are
  unaffected: a `snapshot_version` 1 snapshot (every release up to 0.3.1 writes one)
  is still read and rendered, with `jsonl`/`capture_dir` as `null`, so upgrading the
  CLI does not cut you off from the runs your previous binary started. The
  `snapshot_version` printed in `inspect --json` is the runner's number, so
  `fixtures/schema/cli/inspect.schema.json` now admits the range this build renders
  (`1` or `2`) instead of pinning `2`. Adapters that classify a `103` as "runner
  unreachable" should note this one means the opposite — the runner is healthy and
  its answer was rejected — and is not fixed by retrying; see `docs/control-plane.md`,
  "Snapshot version: a newer runner's reply is refused, an older one is read".
- `run` no longer rewrites the Windows registry directory's owner-only DACL when
  it already matches, removing a per-invocation cost that grew with the number of
  remembered runs (~443 ms at 1024 entries, now flat at ~0.1 ms); the directory is
  also created carrying the descriptor, and a pre-existing directory with widened
  permissions is still repaired.
- Extended the fuzz tier to the raw environment-file and operator-label parsers, including invalid-UTF-8 rejection and secret-safe diagnostic coverage.
- CI now executes the default and E2E test tiers for the shipped static musl
  target instead of only cross-compiling it.
- Update the contained-run backend to ProcessKit 3.1.0 while retaining the
  existing public CLI, lifecycle schema, and MSRV contracts.
- Split the registry into a stable facade, platform-specific implementation files,
  and an isolated test module before further record/control-plane growth.
- Split the command-line surface into one module per subcommand family plus a
  shared value-parser module before further flag and subcommand growth.
- Split the live control plane into a stable facade with separate platform,
  rendering, and test modules before adding further fleet operations.
- Human-readable registry identity and endpoint fields are visibly truncated at a
  bounded terminal-safe prefix while machine-readable JSON preserves them exactly.
- Test-only teardown wording no longer contributes a fabricated capability-scope
  adapter to production builds.
- Registry stale/unprobeable test fixtures now share one typed `Record` serializer
  and scratch-path factory across unit and through-binary tests.
- Default live output now uses ProcessKit's chunk-based raw tee, preserving exact
  child bytes while substantially reducing per-line echo overhead.
- `benches/startup_latency_bench.rs` gained a `direct` control arm timed only up to
  the point the OS reports the child process created (reaped outside the timed
  window), matching the "process created, not yet exited" boundary the runner arm
  already stopped at, so the published startup-latency number is a same-host delta
  (runner vs. direct) between two like-for-like measurements instead of a single
  absolute — cross-host absolutes were never comparable. README.md's "Benchmarks"
  section is updated with a re-measurement against `processkit` 3.3 (~22 ms direct
  vs. ~150 ms under `run`, a ~128 ms delta — what going through `run` costs beyond a
  direct launch, not a breakdown of any one sub-phase) and no longer presents the
  pre-3.2 `ProcessGroup::start` phase-trace figure (166-228 ms) as a current number;
  upstream disputed the *attribution* of that figure to the crate alone — part of
  it is the OS's own process creation happening inside `start`, not its magnitude
  (thread `msg-send-ba9dc66e1b832e104c35c9a1e75a6588`); upstream's own fix for the
  dominant share it had profiled (an all-threads `CreateToolhelp32Snapshot` walk,
  replaced by direct `NtGetNextThread` resolution in `processkit` 3.2) is also
  already included, since this crate depends on 3.3.

### Fixed
- An argv element that is not valid Unicode no longer loses its identity in the
  command diagnostics. `argv_sha256` and `--argv-raw`'s recorded `argv` were both
  derived after every element went through `to_string_lossy()`, so on Unix two
  arguments differing only in their ill-formed bytes (and on Windows two differing
  only in an unpaired surrogate) became the same U+FFFD string: two distinct live
  commands shared one fingerprint in the JSONL stream and in the registry, and
  `--argv-raw` handed back a reconstruction instead of the raw argv it promises.
  Both are now derived from each element's canonical bytes — the argument's own
  bytes on Unix, the WTF-8 encoding of its UTF-16 code units on Windows — which,
  for an element that *is* valid Unicode, are exactly its UTF-8 bytes: every
  ordinary command line fingerprints exactly as it did before, on both platforms.
  An element that cannot be written into a JSON string verbatim is recorded
  losslessly in a reversible escaped form, opened by U+0000 (a character no real
  argv element can contain, so a verbatim element is never mistaken for an escaped
  one); `docs/schema.md` states the encoding and the escape grammar normatively.
  That escaped element is the only string value in the schema that can carry
  U+0000, so it obliges an existing reader in two ways — decode it before
  reconstructing an argv, and check that a sink accepts U+0000 before storing or
  forwarding a *decoded* element (PostgreSQL refuses a `jsonb` document containing
  it, a C-string API truncates the element away). Readers that display, log, or
  store the JSONL *line* are unaffected: the wire form is the ordinary
  six-character JSON escape, so the line stays NUL-free text.
- `--capture-dir` setup is now all-or-nothing. Previously the runner created and
  emptied `stdout.log` before it tried `stderr.log`, so a second stream that could
  not be opened (a path that already named a directory, an unwritable file) left
  the run exiting `SETUP` (111) with a stray empty `stdout.log` — indistinguishable
  from a real transcript of a silent child — and, when that file already existed,
  with its contents already discarded. Both transcripts are now opened before
  either is emptied, and a failed setup rolls back the files and directories that
  attempt created. Paths the runner found rather than created are never rollback
  candidates — the rollback removes and empties nothing it did not create — so a
  setup that cannot open one of the two transcripts leaves an existing file at
  either path with its contents, and leaves an existing capture directory in
  place. A successful setup is unchanged, including its truncation of a stale
  transcript file.
- The cookbook's own JSONL reader recipe told readers to dispatch on a `type`
  field; the event stream has always named that field `event`.
- The detached-supervision examples now hold their child behind an explicit
  release marker, eliminating the inspect-versus-fast-exit race in CI smoke runs.
- Headless Windows integration fixtures no longer leave Windows Terminal error
  panes open after their contained console processes are torn down.
- Environment entries now reject whitespace and control characters in keys, and
  malformed-entry diagnostics no longer repeat potentially secret values.
- Bounded capture continues after a live echo sink reports zero write progress,
  treating it like a broken echo instead of disabling the transcript pump.
- Explicit run ids are validated consistently across every by-id command: they
  must contain 1-256 characters and no terminal control or formatting characters.
- The POSIX installer smoke test now accepts either `python3` or `python` for its
  local fixture server instead of assuming the legacy executable name exists.
- Unix registry test fixtures use the shared atomic counter type, restoring
  non-Windows all-target, Clippy, musl, and MSRV builds.

## [0.3.1] - 2026-07-26

### Added
- **New `cancel --all` / `kill --all` flags: mass teardown of every live run, not
  just one.** `cancel`/`kill` previously addressed exactly one `run_id`, so an
  orchestrator's "cancel everything" step needed a hand-rolled loop over `list
  --json`. `cancel --all` / `kill --all` (mutually exclusive with `--run-id`; exactly
  one of the two is now required, `USAGE` (100) if neither is given — the same clap
  shape `wait --all` established) close that gap: a **snapshot** of every registry
  entry confirmed live is taken once, the moment the invocation starts (the same
  snapshot discipline as `wait --all`, including its "unprobed at snapshot time never
  enters the target set" asymmetry and "a run that registers afterward is out of
  scope" trade-off), and every snapshot entry is addressed by its unique registry
  record path plus remembered endpoint rather than its non-unique `run_id`, so live
  duplicate ids are all torn down instead of becoming unreachable ambiguities. The
  client reconfirms that exact record's liveness and endpoint before dispatch. Instead
  of one ack, `--all` prints a single JSON array on stdout with `run_id`, `accepted`,
  and `status` (`accepted`, `already_gone`, or `failed`) per target, plus `error` only
  for failures. A target confirmed gone before its turn is `already_gone`: it did not
  acknowledge this invocation, but the teardown goal is already met, so it does not
  fail the aggregate. An empty snapshot is not an error — an empty report
  and exit `0`, mirroring `prune` — but a partial or full failure is never a silent
  `0`: it reuses the reserved `CONTROL` (103) code with a stderr summary, so `cancel
  --all` ahead of `wait --all`/`prune` in a teardown sequence cannot silently swallow
  a target it failed to reach. `cancel --run-id`/`kill --run-id` are byte-for-byte
  unchanged. Appears in the `probe` surface tokens automatically (`cancel:--all`,
  `kill:--all`). See README.md, "Command interface", and docs/control-plane.md,
  "`cancel --all` / `kill --all`".
- **New `wait --all` flag: a barrier on every live run, not just one.** `wait`
  previously blocked on only one `run_id`; a supervisor or CI teardown step often
  needs the aggregate version instead — cancel everything, wait until none of it is
  left, then `prune` — which used to mean hand-rolling a polling loop over `list
  --json`. `wait --all` (mutually exclusive with `--run-id`; exactly one of the two is
  now required, `USAGE` (100) if neither is given) is that barrier: it blocks until no
  run this invocation considers in scope is still live, then exits `0`. Its target set
  is a **snapshot**, fixed once at the moment `--all` starts, to exactly the registry
  entries confirmed live right then — a run that registers afterward is out of scope
  for that invocation and is never waited for (re-issue `wait --all` to catch it), the
  same "one clear rule beats an unbounded alternative" trade-off `wait --run-id`
  already documents for an unknown id reading as finished. An entry whose liveness
  cannot be re-probed on a later pass stays outstanding rather than being silently
  dropped — the exact conservative stance `wait --run-id` already applies, but only
  once an entry is in the target set: one that was itself unconfirmed live at the
  snapshot instant is excluded from that set from the start rather than waited on, a
  documented asymmetry with `--run-id`'s own always-tracked target. A bounded
  `--timeout` reports how many snapshot entries are still outstanding and, when any of
  them was only unconfirmed on the last pass, says so rather than confidently claiming
  they are all still live; unbounded, it keeps polling. Same reserved `WAIT_TIMEOUT`
  (112) as the single-run case, and — unlike it — no aggregate `CONTROL` outcome, since
  `--all` never resolves an id at all. `wait --run-id` is byte-for-byte unchanged.
  Appears in the `probe` surface tokens automatically (`wait:--all`). See README.md,
  "Command interface", and docs/registry.md, "Waiting — `wait`", "The aggregate
  barrier — `wait --all`".
- **`list` now says which run is which.** A registry entry used to carry only its
  `run_id`, health, `started_at`, and control endpoint, so an operator with several
  live runs saw rows that were indistinguishable in every way that mattered — nothing
  hinted at *what* any of them was running before picking one to `inspect`/`cancel`/
  `kill`. Each run now also publishes the two **redaction-safe** command fields its
  JSONL stream already carried: `argv_sha256`, the one-way argv fingerprint (equal for
  two entries exactly when they run the same command), and `hint`, the worker-shape
  category (`msbuild_node_reuse`, …) or `null` when the command matches no known
  shape. Both come from the very implementation the `run_started` event uses, so a run
  never fingerprints differently in the two artifacts. `list --json` reports both at
  full precision (the whole 64-character digest, joinable against the run's own
  events); the human-readable table gains `HINT` and `ARGV_SHA256` columns, the latter
  abbreviated to 12 hex characters plus `...`. **No command line is ever written to a
  registry record** — `register` is handed the fingerprint and hint, not the argv, so
  no flag (`--argv-raw` included) can put one there; `root_pid` and `cwd` were
  considered for the same purpose and deliberately refused (see `docs/registry.md`,
  "Which run is which"). The record format stays `registry_version` 1: both fields are
  optional on read, so a record written before they existed still reads (reported as
  `null`), and a record from a newer writer still reads on an older binary — the mixed
  registry a mid-upgrade user really has. Their values are untrusted deserialized data
  like every other field and are shape-checked on read; unlike a malformed
  `started_at`/`lock_file`, a malformed one of these drops the field alone and keeps
  the record, so a cosmetic value can never hide a live run from `list`, `wait`, or a
  control client.
- A concurrency stress test tier (`tests/stress.rs`, `stress` Cargo feature)
  covering the invariants that only break when many runs contend for the two
  resources every run shares — the per-user registry and the per-run control
  plane. It launches dozens of simultaneous `run` invocations against one
  registry directory and drives parallel `list`/`prune`/`wait`/`inspect`/
  `cancel`/`kill` clients at them, asserting that `prune` never reaps a live
  entry (including one still inside its reservation window), that a registry
  scan never loses or duplicates a record under concurrent writes and
  deletions, that a control client aimed at an unreachable or dying runner
  refuses with `CONTROL` (103) inside a bounded deadline instead of hanging,
  and that `wait` never misses — or invents — a completion. Every scenario
  carries a positive control, so none of those "never" assertions can pass
  vacuously. Dev-only tooling: off by default, like the `e2e` and `bench`
  tiers, so it never affects a plain `cargo build`/`cargo test`/`cargo
  publish`; CI runs it as a separate, non-gating scheduled `stress.yml`
  workflow (see CONTRIBUTING.md, "Stress tests"). No runtime behavior, CLI
  surface, exit code, or event-schema change.
- `cleanup_started`/`cleanup_finished` gained an additive `read_error` field: `true`
  when the underlying container-member read itself failed, so a `0`/empty fallback
  is never indistinguishable from a confirmed empty tree or a confirmed-clean
  teardown (mirrors `output_captured`'s `write_error`). See "Fixed" below.
- `probe --print-schema`: prints this binary's embedded JSONL event-schema
  document (`fixtures/schema/v1/schema.json`, embedded at build time via
  `include_str!`) and exits, so a consumer holding only an installed binary or
  an unpacked release archive can fetch the exact machine-readable schema its
  own version emits, entirely offline. New release archives also bundle
  `schema/schema.json` and `schema/events.jsonl` alongside the binary,
  completions, and man pages.

### Changed
- Human-readable tables now measure column widths in Unicode characters, matching
  Rust's padding unit so ordinary multibyte text no longer over-pads later columns.
- Whole-registry commands now share one `SETUP` mapping for registry open and read
  failures; by-run-id control clients retain their intentional `CONTROL` mapping.
- Aggregate `cancel --all` / `kill --all` reconfirm each snapshot target by reading
  and probing only its exact record, avoiding repeated full-registry scans while
  preserving missing, stale, unprobeable, and identity-change outcomes.
- Aggregate control mutations and `wait --all` now take their confirmed-live target
  snapshot through one registry primitive, so both commands share the same liveness
  inclusion rule while retaining their command-specific target projections.
- **Upgraded to `processkit` 3, and Windows soft-stop reporting is honest again.**
  The dependency moves from `2` to `3` (`limits` feature unchanged; the declared MSRV
  stays `1.88`, which is still `processkit`'s own floor). Two things in the major
  release touch this runner. First, `processkit::Error` is now a pointer-sized
  wrapper around a boxed `ErrorReason`, so the runner's two launch/teardown
  classifications read the failure mode off `err.reason()` instead of matching the
  error directly — the exit codes they select (`SPAWN` (101) for a not-found/spawn
  failure, `BACKEND` (102) for every other backend failure) and every operator-facing
  message are unchanged, since `Error`'s `Display` delegates to the reason's and adds
  no envelope. Second, and user-visibly: on Windows `ProcessGroup::signal` is no
  longer an unconditional refusal for a soft stop. A Job Object still has no POSIX
  signal, but ProcessKit now makes a best-effort soft *close* — a `WM_CLOSE` to every
  top-level window owned by a live member — and refuses only when the tree exposes
  nothing such a close can reach. The runner therefore stops making the blanket claim
  that Windows has no soft-terminate tier (true when `0.1.0` shipped, no longer true
  here — that historical entry is left as the record of what `0.1.0` did): a Windows
  `--timeout`/cancel whose tree owns a window now reports `soft_terminate:
  "signalled"` and says a close was asked for, never that a signal was sent; the far
  commoner windowless console child still reports `"unsupported"`, and its stderr line
  now states *why* nothing was delivered (no windowed member, no console-CTRL leader)
  instead of blaming the platform. The later `[Unreleased]` work now adopts the
  console opt-in and structured stop reporting. Verified
  rather than assumed while upgrading: the release's switch to raw pipe-byte
  accounting applies only to the fail-loud `OverflowMode::Error` ceiling and the
  `*_bytes_seen` readbacks, neither of which this runner uses, so both of its own
  ceilings (`--capture-max-bytes` and the in-flight line-assembly cap) are unaffected;
  and the output-event-stream rename is likewise irrelevant here, since `run` streams
  through `stdout_tee`/`stderr_tee` and never touches that stream.
- **`inspect --json` is now optional**, mirroring `list`/`prune`: without it,
  `inspect` prints a human-readable rendering of the snapshot (snapshot version, run
  id, mechanism, root pid, start time, and a column-aligned member table) instead of
  requiring an operator to pass `--json` and read raw JSON at the terminal. `inspect
  --json`'s output is unchanged, byte-for-byte, from before this change. The
  `inspect:--json` `probe` surface token is unaffected — the flag still exists, it is
  simply no longer required.

### Fixed
- Control clients now validate an untrusted registry endpoint against the local
  Unix-socket or Windows named-pipe shape before opening it, rejecting malformed
  endpoints with the reserved control error instead of performing arbitrary I/O.
- Human-readable output now replaces Unicode bidi, zero-width, and other formatting
  characters with spaces alongside terminal controls, preventing invisible or
  reordered text from surviving the shared terminal-safety boundary.
- On Unix, the Ctrl-C listener now preserves an inherited ignored `SIGINT`
  disposition, matching the existing SIGTERM/SIGHUP policy and direct-launch
  behavior; Windows signal handling is unchanged.
- `run_started.cwd` is now always absolute, including when `--cwd` is relative, so
  event consumers can identify the child's actual working directory without knowing
  the runner's ambient cwd. Foreground and detached runs use the same resolution path.
- Single-run `cancel`/`kill` now reject an acknowledgement whose `run_id` does not
  match the requested run, using the same shared acceptance/action/id validation as
  their `--all` forms.
- Human-readable `inspect` now collapses terminal control characters in snapshot and
  process-member strings, matching the existing safety boundary in `list` and
  `prune --dry-run`; `inspect --json` remains byte-for-byte unchanged.
- Failed registry reservations now arm lock-file cleanup immediately after
  `create_new` and close the handle before unlinking, so an early lock-probe error or
  retry does not leak an orphan `.lock` file, including on Windows.
- Human-readable `list` and `prune --dry-run` output now collapses control characters
  from untrusted registry `run_id`/`endpoint` values and orphaned lock-file names,
  preventing forged rows and terminal escape injection while preserving raw values
  in safely escaped JSON output.
- Capture metadata now includes bytes accepted by a partial file write before a
  later write error, so `output_captured.sha256` and the internal written-byte count
  continue to describe exactly the bytes present on disk.
- **`prune` now reaps the leaked control-socket directory of a run that died
  abruptly, not only its registry record and lock.** On unix a runner's control
  transport is a socket inside a per-run `0700` `pkc-<token>` directory under `/tmp`
  (or the platform temp directory), removed only by a clean teardown; a `SIGKILL`,
  crash, or outer Job Object terminate stranded that directory forever, since the
  record naming it was the only thing that pointed at it. `prune` — documented as the
  cleanup counterpart that reaps "the confirmed-stale leftovers of runners that died
  abruptly" — covered only half of them, so repeated abrupt deaths accumulated dead
  `pkc-*` directories in the temp directory. Reaping a **confirmed-stale** entry now
  removes the socket and its directory too, before the record that names them, and
  only ever after the record's `endpoint` (untrusted deserialized data, like its
  `lock_file`) passes a strict shape check: absolute, no `.`/`..`/empty segment as
  written, final component `c.sock`, parent `pkc-` plus an alphanumeric/`-` token,
  directly inside one of the temp bases the control server binds in. No symlink is
  ever followed — the directory is opened `O_NOFOLLOW | O_DIRECTORY` and the socket
  unlinked relative to that handle, only if it really is a socket — and an endpoint
  failing any of that deletes nothing at all while its record is still reaped. Live
  and unprobeable entries keep their sockets, exactly as they keep their files, and
  every deletion stays best-effort: a socket that will not go never aborts the reaping
  of other entries. `prune --json`'s tally is unchanged (a reaped socket is counted by
  its own entry's `pruned`); `prune --dry-run` reports the directory it would reap in
  a new always-present `socket_dir` field on each `entry` candidate (`null` when there
  is none), and in the human-readable listing as a trailing ` socket_dir=<path>`.
  Windows is unaffected: a named pipe lives in the kernel object namespace and
  disappears with its creator, leaving nothing on disk to reap.
- **`cleanup_started`/`cleanup_finished` no longer fabricate a confirmed `0` on a
  member-read failure.** Both emitters previously turned a `ProcessGroup::members()`
  read error into a silent `members_before: 0` / `remaining: 0, remaining_pids: []` —
  indistinguishable from a genuinely empty tree, and inconsistent with the sibling
  `emit_members_snapshot`'s honest degradation and the teardown policy that a read
  failure is not a confirmed empty tree. Both now warn on stderr on a read
  failure and set the new `read_error: true` flag instead of letting the fallback
  stand as an observation; the success path is unaffected.
- `list` (`--json` and the human-readable table) no longer prints a registry entry
  whose liveness lock could not even be *probed* (permission denied, a rejected
  symlink/reparse point, an unexpected non-regular file in its place) as `"stale"` —
  a positive, unconfirmed claim that the runner is dead. It now reports a distinct
  `"unprobed"` health value, matching the three-way vocabulary `prune --json`'s
  `unprobed` tally and `wait` already use for the identical case. Additive change to
  `list --json`'s `health` field.
- **`inspect`/`cancel`/`kill` no longer report an unprobeable registry entry as a
  runner that is gone.** All three act only on a confirmed-live entry, so *what they
  do* is unchanged — they still refuse with `CONTROL` (103) — but the refusal used to
  say "its registry entry is stale — the runner is gone (it exited without cleaning
  up)" for an entry whose liveness lock could not be probed at all, asserting a death
  nothing had established and contradicting the `unprobed` verdict `list`/`prune`/
  `wait` report for that same record. The message now distinguishes the two cases and
  names the unprobeable one `unprobed`, so cross-checking a refusal against `list`
  (as `docs/troubleshooting.md` advises) agrees instead of conflicting. Only
  free-text stderr changed; no exit code, event, or CLI surface did.
- `wait --timeout`'s give-up message now renders the deadline the same way `run`'s
  timeout/grace diagnostics do (e.g. `1500ms`), instead of `Duration`'s `{:?}` Debug
  form (`1.5s` for the same value) — the two subcommands' stderr no longer disagree
  on how to print an identical duration. Only free-text stderr changed; no exit
  code, event, or CLI surface did.

## [0.3.0] - 2026-07-25

### Added
- **New `run --detach` flag: start a run and let go.** The call re-spawns the CLI
  detached — a new session on Unix (`setsid`), `DETACHED_PROCESS` on Windows, `null`
  stdio either way — and returns as soon as that copy has *provably* started the run,
  instead of staying the runner's parent for its whole duration. "Provably" is an
  observation, not an assumption: the call waits until the detached runner's
  `run_started` event is readable in `--jsonl`, which it writes only after creating the
  container, publishing the registry record, and spawning the child — so on return the
  run is already discoverable with `list`, reachable with `inspect`/`cancel`/`kill`,
  and waitable with `wait` (and the run id is readable from the events file even when
  the runner generated it). The detached copy runs the ordinary `run` path unchanged —
  same container, same teardown, same JSONL stream — so detaching adds a spawn and a
  handshake, not a second lifecycle. A caller that *captures* the launch command's
  output sees end-of-file when the call returns rather than when the run ends — the
  detached runner is left holding none of the caller's pipes (on Windows this needed
  an explicit `HANDLE_FLAG_INHERIT` clear, since `CreateProcess`'s handle inheritance
  is all-or-nothing). **The exit code changes meaning under this flag,
  and only under it:** it reports whether the run *started* — `0` once it has, never
  the child's own code, which stays in the terminal `runner_exit` event where a
  detached caller can still observe it. A start that fails is never reported as
  success: the detached runner's own reserved-band code is passed through unchanged
  (a missing program is still `SPAWN` 101, an unusable container still `BACKEND` 102,
  an unwritable `--jsonl` still `SETUP` 111 — reported here before anything is
  spawned), so **no new exit code was minted** and `113`–`119` stay reserved. There is
  no live echo while detached — the detached runner reuses `--no-echo`'s discarding
  sinks rather than a second suppression path — while `--capture-dir`,
  `--idle-timeout`, and the JSONL stream keep observing the child exactly as in the
  foreground, and `--jsonl` stays required. It conflicts at parse time with
  `--inherit-stdio` and `--inherit-stdin` (nothing interactive survives detaching).
  On Windows, pair it with `--create-no-window` for a console child: the detached
  runner has no console to lend, so the OS gives the child a fresh one. `probe --json`
  advertises the new surface automatically (`run:--detach`). See README.md, "Detached
  runs", docs/exit-codes.md, "Detached runs", and docs/integration.md, §2.
- **New `wait --run-id <id> [--timeout <duration>]` subcommand**: block until a run
  recorded in the per-user registry is no longer live. It closes the one supervision
  gap the control plane left open — a supervisor that did *not* start the run (an
  adapter that restarted, a cleanup step, anything holding only a `run_id`) has no
  child process to wait on, and previously had to hand-roll a polling loop around
  `inspect` and read run lifetime out of `CONTROL` refusals, which conflate "I could
  not reach it" with "it finished". `wait` is registry-only: it opens the registry
  read-only (like `list`/`prune`, so waiting never creates the directory or touches
  its permissions), never connects to the run's control transport, never ends or
  disturbs the run, and needs no control endpoint — so a run whose transport never
  came up is still waitable. Because the liveness signal is an OS advisory lock with
  no event to subscribe to, it waits by honest periodic probing rather than pretending
  to be notified. Three outcomes, by exit code: the run is over (`0`), the wait's own
  `--timeout` elapsed with the run still live (the **new reserved code
  `WAIT_TIMEOUT` = 112**, see *Exit codes* below), or the `run_id` is ambiguous —
  more than one live run registered under it, so there is no single run to wait for —
  which reuses the same `CONTROL` (103) refusal `inspect`/`cancel`/`kill` give.
  Nothing is printed on success; the exit code is the whole answer. `--timeout` reuses
  `run --timeout`'s exact parser and grammar (including its rejection of a degenerate
  `0`), and omitting it blocks indefinitely. **One deliberate design choice callers
  must plan for:** a run that exits cleanly deletes its own registry entry, so an
  *unknown* `run_id` is indistinguishable from one that already finished and was
  cleaned up — both exit `0`. That keeps the ordinary "the run finished while I was
  starting up" race from becoming a hard error, at the price that a typo'd `run_id`
  also returns success immediately: `wait`'s `0` means "not running", never "existed
  and completed". `probe --json` advertises the new surface automatically (`wait`,
  `wait:--run-id`, `wait:--timeout`). See README.md, "Command interface", and
  docs/registry.md, "Waiting — `wait`".
- **New reserved exit code `WAIT_TIMEOUT` (112)**, taking the next free slot after
  `SETUP` (111) in the reserved `100`–`119` band (`113`–`119` remain reserved). It is
  minted only by `wait`, and only when *the waiter's* `--timeout` elapsed while the
  run was still live — the run itself was never touched and is still going.
  Deliberately not `TIMEOUT` (106), which means the opposite (the *runner* enforced a
  deadline and tore the child's tree down), and not `CONTROL` (103), since the run was
  resolved unambiguously and found healthy. See docs/exit-codes.md, "A waiter's
  deadline is not a run's deadline".
- **Windows: `Ctrl-Break`, console close, logoff, and system shutdown now end a run
  through the full cancel teardown** instead of the OS's default handling silently
  ending the runner. The console-control events `CTRL_BREAK_EVENT`,
  `CTRL_CLOSE_EVENT`, `CTRL_LOGOFF_EVENT`, and `CTRL_SHUTDOWN_EVENT` (caught via
  `tokio::signal::windows`, the same `SetConsoleCtrlHandler` mechanism `Ctrl-C`
  already used) join `Ctrl-C` in the same race, so they get the same soft-stop →
  `--grace` → hard-kill teardown, the same terminal JSONL events (`cancelled`,
  `cleanup_started`, `cleanup_finished`, `runner_exit`), the same registry-entry
  removal, and the same reserved `CANCELLED` (107) exit. Previously the OS's default
  handling terminated the runner outright on all four: the events were never
  written, the registry entry was left behind stale, and the container was never
  explicitly killed — the ending went unreported to any observer of the event stream
  or registry, even though the tree itself was not left orphaned: Windows already
  reaps the *whole* tree on abrupt owner death (`abrupt_cleanup: whole_tree`, closing
  the runner's last Job Object handle), unlike Linux's direct-child-only `PDEATHSIG`
  reap. Which event arrived is reported honestly rather
  than flattened onto a keyboard interrupt: the `cancelled` event's `source` gained
  the additive values **`ctrl_break`**, **`ctrl_close`**, **`ctrl_logoff`**, and
  **`ctrl_shutdown`** alongside `ctrl_c` (`schema_version` unchanged — a new value
  of an existing string field), and the stderr line names the event
  (`Ctrl-Break`/`console close`/`logoff`/`system shutdown`). All four keep the one
  `CANCELLED` (107) code, the same class of ending. `CTRL_CLOSE_EVENT` carries an
  OS-imposed termination deadline (about 5 seconds): the runner caps the
  *effective* `--grace` for that trigger alone to a budget comfortably inside that
  window (a longer request degrades to the shorter, honest wait — and the
  `cancelled` event's `grace_ms` reports this effective value, not the raw request
  — rather than risk the OS killing the runner mid-teardown, before the terminal
  events are even written); `Ctrl-Break`/logoff/shutdown carry no such matching
  deadline this runner can honestly bound, so they are left uncapped. See
  README.md, "Timeouts, cancel, and grace", and docs/schema.md / docs/exit-codes.md.
- **Unix: `SIGTERM` and `SIGHUP` now end a run through the full cancel teardown**
  instead of killing the runner where it stands. The standard external stop — a plain
  `kill <pid>`, a `systemctl stop`, a cancelled CI job, a supervisor's shutdown
  timeout — and a hung-up controlling terminal join `Ctrl-C` in the same race, so they
  get the same soft-stop → `--grace` → hard-kill teardown, the same terminal JSONL
  events (`cancelled`, `cleanup_started`, `cleanup_finished`, `runner_exit`), the same
  registry-entry removal, and the same reserved `CANCELLED` (107) exit. Previously
  their default disposition terminated the runner outright: the events were never
  written, the registry entry was left behind stale, and — the guarantee that matters —
  the container was never explicitly killed, so on Linux only the direct child was
  reaped (`PDEATHSIG`) and on macOS/BSD nothing was. Which signal arrived is reported
  honestly rather than flattened onto a keyboard interrupt: the `cancelled` event's
  `source` gained the additive values **`sigterm`** and **`sighup`** alongside `ctrl_c`
  (`schema_version` unchanged — a new value of an existing string field), and the
  stderr line names the signal. All three keep the one `CANCELLED` (107) code, the same
  class of ending. A signal the environment deliberately neutralized before launching
  the runner (`SIG_IGN`, as `nohup` does to `SIGHUP`) is left alone rather than
  un-ignored — `nohup processkit-cli run …` keeps surviving a hangup, and nothing is
  lost, because an ignored signal would not have stopped the runner either. Windows was
  left unchanged by *this* entry — its `Ctrl-Break`/console-close/logoff/shutdown
  handling is covered by the Windows entry above, which now joins `Ctrl-C` in the same
  race. See README.md, "Timeouts, cancel, and grace", and docs/schema.md /
  docs/exit-codes.md.
- `run` gained `--idle-timeout <duration>`, a deadline on child **silence** for the
  stuck-worker case (a child that is alive but has long stopped producing output).
  The deadline is re-armed on every chunk of the child's output, so a child that
  keeps talking is never reaped no matter how long it runs — only one that goes quiet
  past the window is. An idle expiry reuses the existing `TIMEOUT` (106) exit and the
  same soft-stop → grace → hard-kill teardown as `--timeout`; the two are told apart
  by a new always-present `reason` field on the `timeout` JSONL event (`overall` vs
  `idle`), so `schema_version` is unchanged (additive field). Same duration grammar
  as `--timeout`, including its parse-time rejection of `0` (see the `Changed` entry
  below); a malformed value is a `USAGE` (100) parse-time error. It
  needs the runner's output pump, so it conflicts with `--inherit-stdio` at parse
  time (like `--capture-dir`) but composes with `--capture-dir`. The new flag appears
  in the `probe` surface tokens automatically. See README.md, "Timeouts, cancel, and
  grace", and docs/schema.md / docs/exit-codes.md.
- `run` resource-limit flags `--max-memory <size>`, `--max-processes <n>`, and
  `--cpu-quota <cores>`, mapping onto ProcessKit's whole-tree `ProcessGroupOptions`
  caps (the `processkit` dependency now enables its `limits` feature). Enforcement
  needs a real container — a Windows Job Object or a Linux cgroup v2 at the real
  hierarchy root — so where a cap cannot be applied (macOS/BSD and the Linux
  process-group fallback; a cgroup v2 that is unenforceable under
  systemd/containers/typical CI) the run fails fast **before** the child is spawned:
  it now emits the previously reserved **`limit_hit`** JSONL event (naming
  `memory`/`processes`/`cpu`) and exits with `BACKEND` (102), rather than running
  silently unbounded. A nonsensical value (`--max-memory 0`, a non-positive/non-finite
  `--cpu-quota`) is a `USAGE` (100) parse-time error. The new flags appear in the
  `probe` surface tokens automatically; `schema_version` is unchanged (the
  `limit_hit` shape was already fixed in v1). See README.md, "Resource limits", and
  docs/schema.md / docs/exit-codes.md.
- Shell completions (bash/zsh/fish/PowerShell/Elvish) and man pages, generated
  from the live `clap` CLI definition by a new `build.rs` at build time and
  attached to every release archive under `completions/` and `man/man1/` (see
  README.md, "Shell completions and man pages"). Build-time generation, not a
  CLI subcommand, so the binary's own runtime surface — and the `probe`
  compatibility report a consumer's preflight checks — are unchanged.
- `docs/integration.md`: a consumer/adapter integration guide walking through
  the fail-closed `probe` preflight, the recommended `run` invocation, reading
  the JSONL event stream, control-plane supervision (`inspect`/`cancel`/
  `kill`), registry housekeeping (`list`/`prune`), and typical error modes —
  linking the existing normative documents rather than duplicating them.
- A [criterion](https://github.com/bheisler/criterion.rs)-based benchmark tier
  (`benches/`, `bench` Cargo feature) covering incremental SHA-256
  (`src/hash.rs`), bounded-capture `absorb` (`src/capture.rs`), the argv hint
  classifier (`src/events.rs`), and two through-the-binary scenarios — echo
  overhead (direct vs. under `run`, with and without `--capture-dir`) and
  startup latency (call to `run_started`) — plus a non-gating CI `perf` job
  that publishes results to the step summary (see README.md, "Benchmarks").
  Dev-only tooling: off by default, like the `e2e` tier, so it never affects a
  plain `cargo build`/`cargo test`/`cargo publish`. `StreamCapture` and
  `classify_hint` are now `pub` (still `#[doc(hidden)]`, no semver guarantee)
  so the new tier can reach them directly, matching this crate's documented
  "future benchmarks reach internal primitives directly" design
  (`docs/architecture.md`, "Target structure").

- `run` gained `--capture-max-bytes <size>`, a per-stream ceiling for
  `--capture-dir`'s bounded transcript files, replacing the previously
  hard-coded 8 MiB constant with a configurable one (same grammar as
  `--max-memory`: a byte count with an optional binary unit — `1048576`,
  `512k`, `256m`, `2g`; a malformed value is a `USAGE` (100) parse-time error).
  Omitting the flag keeps the prior 8 MiB default, so a bare `run`/`run
  --capture-dir` is byte-for-byte unchanged; the `output_captured` event's
  shape and its `truncated` flag's meaning are unaffected. The pump's
  separate in-flight line-assembly ceiling (`CAPTURE_INFLIGHT_MAX_BYTES`)
  stays an independent constant, not derived from this flag (see
  `src/capture.rs`). Appears in the `probe` surface tokens automatically. See
  README.md, "Bounded output capture".

- `run` gained `--no-echo`, an opt-in that suppresses only the runner's own live
  retransmission of the child's stdout/stderr onto its own stdout/stderr. The pipe
  and the output pump stay wired exactly as without the flag: `--capture-dir`
  still receives the child's bytes in full through the same tee, `--idle-timeout`
  still re-arms on every observed chunk, and the JSONL event stream is unaffected
  — only the live echo write is skipped. Meant for an embedding orchestrator that
  reads results from `--jsonl`/`--capture-dir` and finds the child's raw output,
  interleaved with its own, pure noise. Conflicts with `--inherit-stdio` at parse
  time (like `--capture-dir` and `--idle-timeout`), since that mode runs no pump
  to suppress in the first place. Without `--no-echo`, nothing changes: the live
  echo behaves exactly as before. Appears in the `probe` surface tokens
  automatically (`run:--no-echo`). See README.md, "Standard I/O" and "Bounded
  output capture".

- **New `prune --dry-run` flag: preview a reap without deleting anything.**
  `Registry::preview_prune` (`src/registry/mod.rs`) runs the exact same two-pass scan and
  the exact same `probe_for_prune` liveness classification a real `prune` uses, but
  never calls `fs::remove_file` — a confirmed-stale verdict releases its
  probe-acquired lock immediately (there is nothing to reclaim it for) and records
  the candidate instead of reaping it, so the aggregate tally it returns is exactly
  what a following, untouched `prune` pass over the same registry state would
  report. Without `--json` it lists each confirmed-stale candidate (a paired entry's
  `run_id`/`started_at`, or an orphaned lock's file name) followed by a "would
  prune …" summary line; with `--json` it prints the same
  `pruned`/`live`/`unprobed`/`orphaned_locks` fields `prune --json` already does,
  plus an additional `candidates` array tagged `"kind":"entry"` or
  `"kind":"orphaned_lock"`. `prune` without `--dry-run` is byte-for-byte unchanged.
  Appears in the `probe` surface tokens automatically (`prune:--dry-run`). See
  README.md, "Command interface", and docs/registry.md, "Reaping — `prune`".

### Changed
- `run --timeout 0` and `run --idle-timeout 0` are now rejected at parse time
  (`USAGE`, exit `100`) instead of arming an already-elapsed deadline that tore
  the child down immediately after spawn — almost certainly an operator typo,
  never a useful deadline in its own right. This mirrors the existing
  "degenerate cap" rejection `--max-memory 0`/`--max-processes 0`/`--cpu-quota
  0` already receive. `--grace 0` is unaffected and stays legal ("no pause"
  between the soft stop and the hard kill is a real, useful setting). See
  README.md, "Timeouts, cancel, and grace".
- The JSONL `members_snapshot` event and the control-plane `inspect` snapshot now
  populate the enriched per-member fields (`ppid`, executable `name`,
  `start_time`) from ProcessKit's `ProcessGroup::members_info()` (shipped in
  processkit 2.3.2), instead of always emitting `null`. Each field stays
  independently nullable — `members_info()` itself reports a field `null`
  wherever the platform can't read it (the "bare" BSDs report none of them) — and
  `start_time` is an opaque, platform-specific start-time token rendered as its
  decimal string, not a wall-clock timestamp (see `docs/schema.md`, "Enriched
  member fields"). Filling a field the v1 schema always declared but reserved
  `null` is a non-breaking change (`schema_version` unchanged).
- The crate is now a thin binary over an internal library target (`src/lib.rs`,
  `processkit_cli`): every module moved into the library, and `src/main.rs` only
  parses argv and dispatches into it. This is purely a build-structure change —
  the CLI flags, subcommands, exit codes, and JSONL `schema_version` are
  byte-for-byte unchanged. The library is **not** a stable public API (every
  module is `#[doc(hidden)]` and exempt from semantic versioning); it exists only
  so the crate's own test, fuzz, and benchmark tiers can reach the runner's
  internals directly. The supported compatibility surface remains the binary's.

### Fixed
- The control-plane wire protocol now reads its one request/response line under an
  explicit byte ceiling on both sides (`serve_one` on the server, `converse` on the
  client) instead of an unbounded `read_line`, so a broken or hostile owner-local
  control client sending data with no `\n` can no longer make a live run's memory
  grow without limit.
- Both interactive terminal-handoff failure paths (a failed foreground-control
  handoff, and the failed post-handoff process-group resume) now emit a
  `container_failed` event — with a new `phase: "foreground"` — before the terminal
  `runner_exit`, so the failure reason reaches the `--jsonl` stream instead of only
  stderr and the "a `container_error` exit is always preceded by `container_failed`"
  invariant holds on these paths too. `foreground` is an additive value in the v1
  `container_failed.phase` enum (no `schema_version` bump).
- `inspect`/`cancel`/`kill` now open the run registry read-only, like `list`/
  `prune` already did, instead of the mutating open `run` uses: a simple query or
  control command against a run no longer creates the registry directory or
  re-asserts its owner-only permissions as a side effect when the directory does
  not yet exist.
- An orphaned registry `.lock` file — one with no paired `.json` record, which
  `Registry::scan` never sees and so never reached `prune` — no longer accumulates
  forever. `Registry::register` now backstops the reservation it makes before
  writing the record: if the write never lands, the freshly created lock file is
  deleted on drop instead of leaked. `prune [--json]` also gained a second pass that
  reaps any orphaned `.lock` file already on disk (e.g. from a hand-edited registry,
  or a `Registration::remove` whose `.json` delete succeeded while its `.lock`
  delete did not), with the same confirm-before-delete safety as the existing
  paired-record reap (a live lock is never touched; an unprobeable one is left in
  place), plus a minimum-age floor so a lock file `Registry::register` has only just
  reserved — created, but not yet locked — is never mistaken for a long-dead orphan
  by a concurrently running `prune`. Its `--json` tally gained an additive
  `orphaned_locks` field alongside the existing `pruned`/`live`/`unprobed`. See
  README.md and [`docs/registry.md`](docs/registry.md), "Reaping — `prune`".

## [0.2.2] - 2026-07-24

### Added
- `run --inherit-stdio` for interactive commands that need the runner's stdin,
  stdout, and stderr handles directly. It preserves an existing terminal while
  retaining containment, JSONL lifecycle events, cleanup, control-plane access,
  and exit-code fidelity; the default closed-stdin plus pipe-and-echo behavior is
  unchanged. The mode is advertised through `probe` and conflicts with capture,
  no-console mode, and the two input-only modes.

### Changed
-

### Fixed
- POSIX inherited-stdio terminal handoff now keeps `SIGTTOU` ignored while the
  interactive child owns the foreground terminal, restores terminal ownership
  first, and then restores the caller's original signal disposition.

## [0.2.1] - 2026-07-23

### Added
- `run --inherit-stdin` and `run --stdin-file <file>` opt-ins. The former shares
  the runner's stdin with the child; the latter streams a readable file through
  ProcessKit and closes stdin at EOF. The modes are mutually exclusive, leave the
  default closed stdin unchanged, and are advertised through probe surface tokens.

### Changed
-

### Fixed
-

## [0.2.0] - 2026-07-23

### Changed

- ci: drop x86_64-apple-darwin from the release/CI target matrix

## [0.1.0] - 2026-07-23

### Added
- Initial project skeleton.
- Command-line surface: the `run`, `inspect`, `cancel`, and `kill` subcommands
  are parsed and validated, including `run`'s verbatim `-- <program> <args...>`
  tail.
- `run` execution: launches the program shell-free inside a ProcessKit
  `ProcessGroup` the runner owns (in `--cwd`, defaulting to the current
  directory), echoes the child's stdout/stderr live through ProcessKit's pipes
  (pipe + echo, so the child sees no TTY — colors/progress bars may degrade),
  and forwards the child's exit code exactly. Runner-own failures use the
  reserved `100..=119` band (`SPAWN`/`BACKEND`/`INTERNAL`). When `run` returns,
  the container is torn down by the group's kernel-backed kill-on-drop, so leaked
  descendants do not survive. `--create-no-window` is proxied to
  `Command::create_no_window()` (default off).
- `run` now enforces `--timeout` and `--grace` and handles `Ctrl-C`, all as
  **distinguishable** endings that share one teardown path. A `--timeout` that
  elapses exits with the reserved `TIMEOUT` code (106); a `Ctrl-C` cancel exits
  with the reserved `CANCELLED` code (107) — each distinct from the other and from
  a forwarded child code — with an explanatory line on stderr. Both first ask the
  tree to stop, wait out `--grace`, then let the owning container's kill-on-drop
  hard-tear-down the whole tree, so no descendant survives either ending.
  `--timeout`/`--grace` accept a small duration grammar (`ms`/`s`/`m`/`h`, integer,
  default `s`); a malformed value is a usage error (100). On Windows, where the
  ProcessKit kernel has no soft-terminate tier yet, no soft signal is sent — the
  grace window elapses and the Job Object is then killed atomically, and the runner
  reports this honestly rather than implying a graceful stop. (The machine-readable
  JSONL form of these outcomes lands with the event schema.)
- Documented runner exit-code contract (`docs/exit-codes.md`) that keeps the
  runner's own failures in a reserved code band, separate from the child's
  exit code, and now assigns `TIMEOUT` (106) and `CANCELLED` (107).
- Versioned JSONL event schema (v1): `run` now writes a stream of lifecycle
  events to the `--jsonl` file — one JSON object per line, each with a
  `schema_version`, and never to stdout. The stream covers `run_started` (run id,
  root PID, containment mechanism, abrupt-owner-death cleanup scope, working
  directory), `members_snapshot`,
  `root_exited`, the `cleanup_started` / `cleanup_finished` teardown pair,
  `timeout` / `cancelled`, launch and container errors, and a terminal
  `runner_exit` that preserves the child's own code even on the runner's own
  failure — so a child's code is never lost or aliased. The command line is
  redacted by default (raw argv only under `--argv-raw`; the redaction hash and
  worker-shape hint are reserved fields), and member snapshots are PID-only with
  the richer per-member fields declared but absent until ProcessKit-rs ships them.
  Normative reference in `docs/schema.md`; golden sample stream published at
  `fixtures/schema/v1/events.jsonl` and gated by a golden test. `--run-id` and
  `--argv-raw` are now consumed.
- Bounded output capture (`--capture-dir <dir>`): the child's stdout and stderr are
  teed into `<dir>/stdout.log` and `<dir>/stderr.log` alongside the unchanged live
  echo, kept separate per stream. A new `output_captured` JSONL event records, for
  each stream, the file path, a full byte counter, a SHA-256 of the captured bytes
  (the same digest primitive as the argv fingerprint), and an explicit truncation
  flag — so a consumer distinguishes "captured in full" from "clipped at the limit"
  without inferring it from the file's size. The capture is bounded by ProcessKit's
  byte-capped `OutputBufferPolicy` (the pump's in-flight memory) plus a per-stream
  file ceiling; the runner adds no draining or limiting of its own, and the
  held-descriptor teardown bound is preserved (a descendant keeping an output handle
  open past the root's exit cannot hang the runner). A run without `--capture-dir`
  is byte-for-byte unchanged (no files, no event). Additive schema v1 change,
  reflected in `docs/schema.md` and the golden fixture.
- Control-plane `cancel` and `kill` subcommands: `cancel --run-id <id>` and
  `kill --run-id <id>` reach the live runner over the same local transport and
  registry discovery as `inspect` (by `run_id`, never a PID) and end the run. `cancel`
  runs the runner's **shared** soft-stop → grace → hard-kill teardown — the same path
  a `--timeout` or a `Ctrl-C` drives, honest Windows hard-kill fallback included — and
  the run exits with the new reserved code `CONTROL_CANCELLED` (108); `kill` hard-kills
  the whole tree immediately (no soft stop, no grace) and the run exits with
  `CONTROL_KILLED` (109). Both are distinguishable from a Ctrl-C, a timeout, and each
  other by exit code *and* in the JSONL stream: `cancel` writes a `cancelled` event
  with `source` `control_cancel`, `kill` writes a new `killed` event with `source`
  `control_kill`, and each closes with a terminal `runner_exit` carrying the matching
  `source` — so an external observer reading `--jsonl` sees the external command, not
  just the control client. The kill scope is only the target run's ProcessKit
  container (discovered via the registry); nothing is ever killed by executable name.
  The wire protocol gains the two verbs without reshaping its one-request/one-JSON-line
  framing, each answered with a `{"accepted":…,"action":…,"run_id":…}` ack, and an
  unreachable/stale runner is the same bounded `CONTROL` (103) failure as `inspect`.
  Additive schema v1 change (new `source` values and the `killed` event), reflected in
  `docs/control-plane.md`, `docs/schema.md`, `docs/exit-codes.md`, and the golden
  fixture.
- Side-effect-free compatibility probe: `processkit-cli probe --json` is the
  preflight a consumer
  runs on a candidate **before** launching any payload: it prints the binary's
  compatibility surface (package name, version, JSONL `schema_version`, the reserved
  exit-code band, and the CLI surface tokens derived from the live parser) as one
  deterministic JSON line, and spawns no child, opens no registry, and creates no
  container. With `--require-schema-version` / `--require-exit-code-band` /
  `--require-surface` it *verifies* those dimensions and fails closed with the new
  reserved code `PROBE_INCOMPATIBLE` (110) — the next free slot in the `100`–`119`
  band — printing `compatible:false` with concrete `mismatches` rather than a silent
  "ok". The contract is fail-closed across three distinct, parseable outcomes — path
  missing (`NotFound` at spawn), present-but-not-executable (a non-`NotFound` spawn
  error), and present-executable-but-incompatible (exit `110`) — and forbids any
  silent fallback to an uncontained launch. The new code is recorded in
  `docs/exit-codes.md`. Additive only: no existing flag, exit code `100`–`109`, or
  `schema_version: 1` changes meaning.
- Abrupt runner-death hardening and proof: every spawned command opts into
  ProcessKit's public parent-death primitive. The versioned `run_started` event
  now reports the actual surviving guarantee as `abrupt_cleanup` (`whole_tree`
  on Windows, `direct_child_only` on Linux, `none` on macOS/other Unix), and the
  E2E tier force-kills the runner with a live child/grandchild to verify each
  platform's behavior without unsafe kill-by-PID cleanup.
- Dependencies on `processkit` (the containment backbone), `tokio` (its async
  runtime), `clap` (CLI parsing), and `serde` / `serde_json` (the JSONL event
  schema).
- Prebuilt release binaries: the manual `release.yml` workflow now fans out a
  downstream `build-artifacts` matrix that builds a `--release` binary for
  Windows, Linux, and macOS across x86_64 and aarch64 — plus a statically linked
  `x86_64-unknown-linux-musl` build for minimal/container images — and attaches
  each archive to the same GitHub Release. It runs strictly after the existing
  crates.io publish + tag, so the release ordering is unchanged and there is still
  a single release path; `cargo install processkit-cli` remains a first-class
  install. `README.md` gains an Installation section with a platform matrix that
  states the actual kernel container mechanism reported per platform (Job Object
  on Windows, cgroup v2 on Linux, POSIX process group on macOS/other Unix).
- Machine-readable JSON Schema (draft 2020-12) for the JSONL event contract v1,
  published at `fixtures/schema/v1/schema.json` alongside the golden
  `events.jsonl` fixture: one schema variant per event type plus the shared
  envelope, transcribed from the normative `docs/schema.md`. Adapters
  (`processkit-py`) can validate against it instead of reimplementing the
  shapes by hand. A new test (`tests/events.rs`) validates the golden fixture,
  and several live streams emitted by the through-the-binary tests, against
  the schema, so drift between the schema, the fixture, and the code fails the
  build. `docs/schema.md` remains the normative source of truth on any
  disagreement.
- `list [--json]`: a new subcommand that scans the per-user registry
  (`Registry::entries`) and prints every entry it finds, live and stale alike —
  `run_id`, health, `started_at`, and `endpoint` — the discovery counterpart to
  `inspect`/`cancel`/`kill` for a caller that has lost (or never had) a `run_id`.
  Read-only: it never connects to any runner's control transport, so it has none
  of their unreachable-run failure modes. Without `--json` it prints a
  human-readable table (`no runs registered` for an empty registry); with
  `--json` it prints one JSON object per entry, one per line, sorted by `run_id`
  then `started_at`. An empty registry is not an error (exits `0`), and a single
  corrupt/unreadable record never blinds the command to the healthy entries
  (the same degradation `Registry::entries` already applies). Additive only —
  the new subcommand appears in the `probe` surface tokens automatically.
- `prune [--json]`: a new subcommand that reaps detectably-dead registry entries —
  after a runner dies abruptly its `.json`/`.lock` pair lingers forever, since
  cleanup only runs on an orderly exit. It probes each entry on its own and removes
  only those confirmed stale by a successful liveness probe: a live entry is never
  touched, and an entry whose probe merely fails (its lock file could not be opened
  at all) is left in place rather than assumed dead — deliberately distinct from the
  degradation `Registry::entries` applies for display. Removal reaches files only
  through the scanned record path, never a PID, and holds the stale entry's lock
  while deleting its record and lock file. Without `--json` it reports how many
  entries were reaped, kept live, and left unprobed; with `--json` it prints that
  summary as one JSON object. An empty or already-clean registry is a no-op (exits
  `0`). Additive only — the new subcommand appears in the `probe` surface tokens
  automatically.

### Changed
- Setup/support failures no longer masquerade as an `INTERNAL` (104) runner fault.
  A new reserved code `SETUP` (111) covers a fail-closed setup failure — an async
  runtime that will not build, an unwritable `--jsonl`/`--capture-dir`, or a
  `probe`/`inspect`/control reply that will not serialize — so `INTERNAL` (104) now
  means strictly a genuine invariant violation (a runner bug) and a consumer never
  reads a bad path as one. The `--capture-dir` setup failure's terminal
  `runner_exit` event gains a matching `source: "setup"` (added to the JSONL
  schema); codes `112`–`119` remain reserved.
- The control plane's three clients — `inspect`, `cancel`, and `kill` — all reach a
  live runner over the local transport now; no subcommand returns the runner-range
  "not implemented" code any longer.
- `run` now consumes every flag it parses: `--jsonl` (the JSONL event stream) and
  `--capture-dir` (bounded output capture) are both wired up.
- Internal: the control plane's client-side scaffolding is de-duplicated. The
  `inspect`/`cancel`/`kill` wire exchange (`converse`/`converse_mutation`) is now
  one function generic over the reply type; `inspect_async`/`mutate_async` share a
  single deadline-timeout-to-`unreachable_run` helper; and the three
  current-thread tokio runtime constructions in `run`/`inspect`/`cancel`/`kill`
  now go through one shared builder. No externally visible behavior changes.
- Updated the `processkit` dependency to 2.3.2 (from 2.3.0). `events::abrupt_cleanup_str()`
  now sources the abrupt-owner-death reap scope from `processkit`'s own honest
  capability report (`Command::kill_on_parent_death_scope`, new in 2.3.2) instead of
  reimplementing the per-platform derivation locally; the emitted
  `whole_tree`/`direct_child_only`/`none` wire values are unchanged.

### Fixed
- Unix control sockets now use a short owner-only temporary directory instead of
  inheriting the registry's full path, so deeply nested macOS CI/workspace paths
  cannot exceed `sockaddr_un::sun_path` and silently disable `inspect`.

[Unreleased]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.3.4...HEAD
[0.3.4]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ZelAnton/ProcessKit-CLI/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ZelAnton/ProcessKit-CLI/releases/tag/v0.1.0
