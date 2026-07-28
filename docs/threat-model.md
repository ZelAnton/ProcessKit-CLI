# Threat model

This document states, in one place, what `processkit-cli` treats as untrusted
input, who the trusted principal is, where the project deliberately draws its
security boundary, and which concrete threats within that boundary are closed
— and by what mechanism. It does not restate the mechanisms' own normative
text (each closed threat below links to the module or document that owns it);
treat it as the map for a security reviewer or auditor, not a substitute for
reading the cited code.

## Untrusted inputs

Three input surfaces are treated as untrusted or semi-trusted and are handled
accordingly (bounded parsing, validation before use, no blind trust in shape):

- **Registry bytes.** Every record file under the per-user run registry
  (`src/registry/mod.rs`) is parsed defensively — a corrupt or malformed record is
  skipped, never trusted to abort a scan or to smuggle a path outside the
  registry directory (see "Closed threats" below for the `lock_file` case).
- **Control-plane wire strings.** The one request-verb line a client sends,
  and the one JSON reply line the server sends back, over the local
  `inspect`/`cancel`/`kill` transport (`src/control.rs`), are read as
  untrusted bytes from whichever local process holds the socket/pipe.
- **The child's argv and output.** The command line passed to `run` is
  attacker-influenceable in the sense that it ends up in a diagnostic
  artifact (the JSONL stream) an operator or automated tooling later reads;
  the child's own stdout/stderr are unbounded, potentially adversarial or
  merely pathological, byte streams (`src/events.rs`, `src/capture.rs`).

## Trusted principal and boundary

The trusted principal is **the same OS user** that invokes `processkit-cli`:
every security mechanism below defends that user's own runs against a
*different* OS user (or an unprivileged remote party with no local account),
never against that user's own other processes.

**Explicit boundary.** `processkit-cli` does **not** defend a run against a
malicious process already running as the *same* OS user. A same-user process
that can read the registry directory, connect to the control-plane transport,
or otherwise act with that user's own privileges is, by definition, already
inside the trust boundary this project draws — the owner-only restrictions
below exist to keep *other* principals out, not to isolate one same-user
process from another.

## Closed threats

Each entry names the threat, the mechanism that closes it, and the exact
code/docs it is implemented and described in.

- **A different OS user reading or connecting to the registry/transport.**
  The per-user registry directory's permissions are re-asserted on every
  mutating open, not merely checked: `0o700` re-applied via `chmod` on Unix
  (bypassing umask), a protected owner-only DACL replaced on Windows
  (`src/registry/mod.rs`, `Registry::open`/`open_in`,
  `platform::create_owner_only_dir`, `platform::restrict_to_current_user`).
  The control-plane transport is deliberately **not** derived from that
  directory: each run atomically reserves its own short-lived `0o700`
  directory under `/tmp` (falling back to the platform temp directory) and
  binds the Unix socket inside it, with the socket file itself given `0o600`
  on a best-effort basis afterward (`src/control.rs`,
  `imp::ControlServer::bind`, `create_private_socket_dir`); the path is kept
  independent of the registry directory specifically so a long registry path
  cannot push the socket path past `sockaddr_un::sun_path` on macOS (see
  [`docs/control-plane.md`](control-plane.md), "Local transport"). On
  Windows, the control-plane's named pipe is built with its own
  non-inheritable owner-only DACL, sharing only the FFI-glue module
  `src/win_security.rs` (`SecurityDescriptor`, `to_wide`) with the registry
  directory's DACL construction — that sharing is Windows-only; the Unix
  socket has no DACL and no relationship to `src/win_security.rs`.
- **The command line leaking into diagnostics.** `run_started`'s `command`
  field is redacted by default: the raw argv is not recorded, only a
  one-way SHA-256 fingerprint (`argv_sha256`) and a categorical worker-shape
  `hint` from a static classifier table, both derived from argv but unable to
  reveal it (`src/events.rs`, `argv_sha256_hex`, `classify_hint`/
  `HINT_RULES`). Recording the raw argv requires an explicit opt-in
  (`--argv-raw`); it is never the default. The per-user registry record
  publishes that same one-way pair (and only it) so `list` can tell several
  live runs apart — the raw argv is not even an input to the registry's
  `register`, which takes an `events::CommandFingerprint`, so no flag
  (`--argv-raw` included) can put a command line into a registry record. The
  values are shape-checked when read back, like every other record field (see
  [`docs/registry.md`](registry.md), "Reading a record").
- **An unbounded or malformed control-plane wire line.** Both the server's
  request-line read and every client's reply-line read are capped at
  `MAX_LINE_BYTES` (64 KiB) via a shared bounded-read helper
  (`src/control.rs`, `read_bounded_line`) — an oversized or unterminated line
  fails deterministically rather than growing an in-memory buffer without
  bound. Separately, a registry record's `lock_file` field is validated as a
  simple file name before it is ever joined onto the registry directory path
  — control characters, NUL, path separators, Windows reserved device names
  (with or without an extension, including superscript-digit aliases), and
  symlink targets (rejected at open time via `O_NOFOLLOW` on Unix / a
  reparse-point check on Windows) are all refused
  (`src/registry/mod.rs`, `is_simple_lock_file_name`,
  `is_windows_reserved_device_name`, `platform::open_lock_file`).
- **A record steering a deletion outside its own leftovers.** `prune` reaps
  the Unix control socket a **confirmed-stale** record published, which means
  a `remove` call driven by that record's `endpoint` — untrusted deserialized
  data like `lock_file` above. The value is refused unless it is exactly the
  form the control server publishes (absolute, no `.`/`..`/empty segment as
  written, final component `c.sock`, parent `pkc-` plus an alphanumeric/`-`
  token, sitting directly inside one of the temp bases the server binds in),
  and even then no symlink is followed: the directory is opened
  `O_NOFOLLOW | O_DIRECTORY` and the socket is unlinked relative to that
  handle, only if it really is a socket, with the directory itself removed by
  an empty-only `rmdir`. A value failing any of that deletes nothing at all —
  the record and its lock are still reaped (`src/registry/mod.rs`,
  `platform::control_socket_dir_to_reap`,
  `platform::reap_control_socket_dir`; rationale in
  [`docs/registry.md`](registry.md#reaping-the-control-socket)).
- **Launching an incompatible or unusable runner binary uncontained.** The
  side-effect-free `probe` subcommand is a fail-closed preflight contract: it
  spawns no child and touches no registry, reports this binary's version,
  `schema_version`, reserved exit-code band, and live CLI surface as one JSON
  line, and — given `--require-*` expectations — exits `PROBE_INCOMPATIBLE`
  (110) with the concrete mismatches on any unmet expectation, rather than
  ever letting an adapter silently proceed with an incompatible binary
  (`src/probe.rs`; consumer walkthrough in
  [`docs/integration.md`](integration.md), "Fail-closed preflight: `probe`").
- **Resource exhaustion from a pathological child output stream.** The
  `--capture-dir` tee enforces a hard per-stream byte ceiling
  (`CAPTURE_MAX_BYTES`, configurable via `--capture-max-bytes`) with an
  explicit `truncated` flag rather than growing the capture file without
  bound, and `--idle-timeout` tears the run down if the child goes silent
  past a configured window (a shared `IdleClock` re-armed by any non-empty
  write on either the default echo path or the `--capture-dir` tee), closing
  the case of a child that neither exits nor produces bounded output
  (`src/capture.rs`).
- **Supply-chain compromise of the build or release pipeline.** Every
  third-party GitHub Actions step in `.github/workflows/ci.yml` and
  `.github/workflows/release.yml` is pinned to a full commit SHA (not a
  floating tag) **except** the toolchain selector,
  `dtolnay/rust-toolchain@stable`/`@master`, which both workflows leave
  intentionally unpinned (each occurrence carries an explicit
  "intentionally unpinned" comment) so CI and releases keep tracking the
  rolling `stable`/MSRV toolchain; that one exception means trust in
  `dtolnay/rust-toolchain`'s owner is accepted, not eliminated (see "What is
  not closed" below). Everywhere else, a compromised or re-tagged action
  cannot silently change what CI or a release build runs.
  `cargo deny check advisories bans licenses sources` runs on every pull
  request and push to `main` (`deny.toml`, `.github/workflows/ci.yml`), failing
  the build on a known RustSec advisory, a yanked crate, a wildcard version
  requirement, a disallowed dependency license, or a dependency sourced from
  outside crates.io.
  Released artifacts carry a SHA-256 checksum and a signed
  `actions/attest-build-provenance` attestation
  (`.github/workflows/release.yml`) a consumer can verify against the exact
  commit and workflow that produced them. A dedicated fuzz tier
  (`fuzz/`) exercises the parsers that sit closest to the untrusted inputs
  above — the registry's byte-to-record parser, the control-plane's
  request/reply decoders, and the CLI's own value parsers — under
  `cargo-fuzz`.

## What is not closed

The boundary above is deliberate, not an oversight; the following are
explicitly **out of scope** for this project's own security mechanisms:

- **Confidentiality of data inside the child process.** Whatever the child
  program reads, writes, or holds in memory is entirely its own concern;
  `processkit-cli` observes only what the child writes to its own
  stdout/stderr (and, if requested, the process tree's membership) — it does
  not attempt to protect the child's internal state from anything.
- **Isolation from another process of the same OS user.** As stated under
  "Trusted principal and boundary" above, a same-user malicious process is
  inside the trust boundary, not outside it — this project provides no
  mechanism against it (no additional sandboxing, no cross-process
  capability restriction beyond the owner-only ACLs that already keep out
  *other* users).
- **Trust in the `dtolnay/rust-toolchain` action owner.** Both workflows
  deliberately leave that one action unpinned (a floating `@stable`/`@master`
  tag rather than a commit SHA) so CI and releases keep tracking the rolling
  stable/MSRV toolchain; a compromise of that action's owner or repository
  could change what CI or a release build runs, and the project accepts that
  residual risk rather than freezing the toolchain version.
- **Denial of service through the operating system itself.** Beyond the
  opt-in, best-effort `--max-memory`/`--max-processes`/`--cpu-quota` caps on
  the child's own process tree (platform-limited: real Windows Job Object or
  Linux cgroup v2 enforcement only, fail-fast rather than silently
  unenforced — see [`README.md`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/README.md), "Resource limits"),
  `processkit-cli` does not defend against exhaustion of system-wide
  resources (memory, file descriptors, process table slots) by other
  workloads on the same machine; that remains the operating system's and the
  operator's own concern.

## See also

- [`SECURITY.md`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/SECURITY.md) — how to report a vulnerability, and the
  automated supply-chain scanning this document's "Supply-chain compromise"
  entry summarizes.
- [`docs/architecture.md`](architecture.md) — the module map and data flow
  this document's closed-threat entries point into.
- [`docs/integration.md`](integration.md) — the consumer-facing preflight and
  redaction walkthrough (`probe`, command redaction) referenced above.
- [`docs/registry.md`](registry.md) and
  [`docs/control-plane.md`](control-plane.md) — the normative registry and
  control-plane documents the owner-only and bounded-read mechanisms above
  are drawn from.
