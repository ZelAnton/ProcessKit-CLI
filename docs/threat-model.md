# Threat model

This document states, in one place, what `processkit-cli` treats as untrusted
input, who the trusted principal is, where the project deliberately draws its
security boundary, and which concrete threats within that boundary are closed
— and by what mechanism. It does not restate the mechanisms' own normative
text (each closed threat below links to the module or document that owns it);
treat it as the map for a security reviewer or auditor, not a substitute for
reading the cited code.

## Untrusted inputs

Five input surfaces are treated as untrusted or semi-trusted and are handled
accordingly (bounded parsing, validation before use, no blind trust in shape):

- **Registry bytes.** Every record file under the per-user run registry
  (`src/registry/mod.rs`) is parsed defensively — a corrupt or malformed record is
  skipped, never trusted to abort a scan or to smuggle a path outside the
  registry directory (see "Closed threats" below for the `lock_file` case).
- **Control-plane wire strings.** The one request-verb line a client sends,
  and the one JSON reply line the server sends back, over the local
  `inspect`/`cancel`/`kill`/`attest` transport (`src/control/mod.rs`), are read as
  untrusted bytes from whichever local process holds the socket/pipe.
- **A control-plane client's claimed identity — which is why there is none.** The
  `attest` verb answers whether the connecting process is inside the run's container
  (`docs/control-plane.md`, "`attest`"), and a caller's own account of who it is would
  be the most obviously untrusted input on this list: any local process can hold any
  string and name any pid. So none is accepted. The command exposes no `--pid` and the
  verb carries no argument at all; the identity is read from the transport itself
  (unix peer credentials, `GetNamedPipeClientProcessId` on Windows) while the
  connection is open, and checked against the run's own live container membership. The
  two facts that follow are the reason this is listed here rather than under "closed
  threats": the *input* surface for this verb is empty by construction, and the answer
  is therefore a property of the connection rather than of anything parsed. A platform
  that cannot supply that identity is answered `peer_identity_unsupported` — a refusal
  — rather than being allowed to fall back to what the caller says.
- **The child's argv and output.** The command line passed to `run` is
  attacker-influenceable in the sense that it ends up in a diagnostic
  artifact (the JSONL stream) an operator or automated tooling later reads;
  the child's own stdout/stderr are unbounded, potentially adversarial or
  merely pathological, byte streams (`src/events.rs`, `src/capture.rs`).
- **The events file read back.** The JSONL lifecycle stream is this project's
  own output while a run writes it, and untrusted input the moment anything
  reads it back: it sits at an operator-chosen path (`run --jsonl`), any local
  process can write one, and a reader will read whatever it is pointed at. Two
  commands read one. `wait --report-outcome` reads a bounded head/tail window
  of the file a registry record names (`src/wait.rs`). `events` is the larger
  of the two surfaces — it reads a whole stream, and `events --file <path>`
  reads an **arbitrary caller-specified path**, including a file this registry
  never knew about, such as an adapter's own fixture; `events --run-id <id>`
  reads the locator a registry record publishes, itself untrusted deserialized
  data under "Registry bytes" above. Everything on that path is hand-rolled and
  treats the bytes as hostile:
  - an incremental line reader that hands out only *complete* lines and refuses
    to buffer one past `MAX_LINE_BYTES` (1 MiB), so a file with no newline in it
    cannot decide this process's memory use; invalid UTF-8 is replaced and
    reported, never fatal (`src/events_cmd/mod.rs`);
  - under `--validate`, an interpreter of the embedded JSON Schema document run
    over each parsed line (`src/events_cmd/schema.rs`) together with the small
    anchored matcher that document's `pattern` keywords need, which runs
    against untrusted string content (`src/events_cmd/pattern.rs`);
  - and, at the terminal boundary, every operator-facing fragment — a rendered
    field, the notice about a line that would not parse, a schema violation, and
    the stream's own locator — passed through `text::terminal_safe_bounded`
    (`src/text.rs`) before it is printed, so neither a stream's content nor a
    registry-published path naming it can forge or overwrite what an operator
    sees. `events --json` is the one deliberate exception and not a terminal
    rendering: it passes the runner's own bytes through byte for byte (a line
    that is not JSON is reported instead of emitted), relying on JSON's own
    escaping exactly as this project's other machine-readable outputs do —
    the line `src/text.rs` draws explicitly between the two. The
    `--error-format json` failure envelope (`src/error_envelope.rs`) sits on the
    same side of that line, and it is worth being explicit because it prints to
    **stderr**, where a terminal may be watching: its `message` is the very string
    the prose mode prints, but serialized by `serde_json`, so a control sequence or
    bidi override that reached a diagnostic (through an OS error text, say) is
    escaped rather than emitted, and the object is always exactly one line. The
    fragments that are sanitized at construction — the events locator above, for
    instance — stay sanitized in both modes, because both render the same message.

  See "Supply-chain compromise" below for what the fuzz tier does and does not
  currently exercise on this surface.

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
  The per-user registry directory's permissions are guaranteed on every
  mutating open, not merely assumed: `0o700` re-applied via `chmod` on Unix
  (bypassing umask); on Windows the directory is created carrying its
  protected owner-only DACL and, on a subsequent open, that DACL is compared
  ACE for ACE against the target and rewritten whenever it does not match
  (`src/registry/mod.rs`, `Registry::open`/`open_in`,
  `platform::create_owner_only_dir`). A pre-existing directory whose
  permissions were widened out of band is repaired on both platforms. The
  Windows comparison is deliberately exact and fail-closed — an unreadable
  descriptor, an extra ACE, a missing protected bit, or a non-directory all
  route to the unconditional write — so the skip can only ever elide a write
  whose result is already in place; no weaker signal an attacker could forge
  (the directory existing, a marker file, a cached flag) is accepted in its
  stead. Neither platform touches ownership, then or now.
  The control-plane transport is deliberately **not** derived from that
  directory: each run atomically reserves its own short-lived `0o700`
  directory under `/tmp` (falling back to the platform temp directory) and
  binds the Unix socket inside it, with the socket file itself given `0o600`
  on a best-effort basis afterward (`src/control/mod.rs`,
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
  (`src/control/mod.rs`, `read_bounded_line`) — an oversized or unterminated line
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
  commit and workflow that produced them.
  That workflow writes **outside this repository** in two places. The
  long-standing one is the `release` job's `cargo publish --locked` step:
  every release uploads the crate to crates.io under `CARGO_REGISTRY_TOKEN`
  (`secrets.CRATES_IO_TOKEN`), a standing credential that lives in this
  repository's own Actions secrets, publishing to a registry users are
  pointed at today ([`docs/installation.md`](installation.md), "Install from
  crates.io"). The second is `publish-package-repos`, the only job that
  pushes into another *git* repository: it pushes the generated Homebrew
  formula — and, independently, the Scoop manifest — into the tap/bucket
  repositories this project owns, which is what would make a `brew
  install`/`scoop install` work at all. Several properties bound it. It does
  nothing unless an operator has set that channel's token secret
  (`HOMEBREW_TAP_TOKEN` / `SCOOP_BUCKET_TOKEN`) — with none set it skips
  with a notice rather than failing, and each channel is enabled on its own.
  Its own `GITHUB_TOKEN` is narrowed to `contents: read` (a job-level
  `permissions:` block replaces the workflow's top-level `contents: write`
  rather than merging with it), so it cannot write to *this* repository, and
  every write it does perform is authenticated by the channel's own token
  instead. That token is required to be scoped to the tap/bucket alone (a
  fine-grained PAT holding `Contents: read and write` on it, or a GitHub App
  credential); the built-in `GITHUB_TOKEN` cannot stand in, as it reaches no
  other repository. That scoping is an obligation on whoever mints the
  secret, though, not a property the workflow can check — see "What is not
  closed" below. The operator-set target variables (`HOMEBREW_TAP_REPOSITORY`
  / `SCOOP_BUCKET_REPOSITORY`) are accepted only in an exact,
  whole-string-anchored `owner/name` shape before they can reach a clone URL
  or the step's own outputs. The job adds no third-party action, so it widens
  no pinning surface, and it runs after crates.io, the tag, the Release, and
  every asset upload under `continue-on-error: true`, so it can neither gate
  a release nor alter one that already happened (see
  [`docs/release-process.md`](release-process.md), "What the
  `publish-package-repos` job does"). Neither the tap nor the bucket is
  configured for this repository today — neither repository exists, so
  nothing has been published through them yet
  ([`docs/installation.md`](installation.md), "Publishing to a tap or
  bucket"), unlike the crates.io publication above, which happens on every
  release; what this arrangement accepts once a tap or bucket *is* configured
  is stated under "What is not closed" below. A dedicated fuzz tier
  (`fuzz/`) exercises **four** of the parsers that sit closest to the untrusted
  inputs above, under `cargo-fuzz`: the registry's byte-to-record parser, the
  control-plane's request/reply decoders, the CLI's own value parsers, and
  `wait --report-outcome`'s bounded head/tail read-back of a run's JSONL events
  file (a path any local process can write, so its content is untrusted the
  same way). It does **not** reach the `events` reader described under
  "Untrusted inputs" above — the larger of this project's two events-file
  readers, and the only one that opens an arbitrary caller-given path: neither
  its incremental line reader, nor the schema interpreter and anchored pattern
  matcher behind `--validate`, is fuzzed. That stack is covered by unit and
  through-the-binary tests, and its `--validate` verdict is held line for line
  against a real JSON Schema engine (`tests/events.rs`), but it is **not** under
  coverage-guided fuzzing: a fifth target over that reader is the way to close
  the gap, and until one exists this document claims no fuzz coverage for it.

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
- **Authentication between mutually hostile peers — including through
  `attest`.** The `attest` verb reports a *containment* fact (is the connecting
  process in this run's container?) established from kernel-supplied peer
  identity, and it is genuinely unforgeable in the sense that matters: a
  process cannot make the runner name a pid other than its own. What it is
  **not** is an authentication mechanism. It runs entirely inside the same-user
  boundary above — a process able to reach the control plane at all is already
  that user — so it neither adds a boundary nor is one. Two consequences worth
  stating outright: a `member` answer says the caller is contained, not that it
  is trustworthy (a compromised process inside the container attests
  positively, correctly); and the answer is scoped to the connection it was
  made on, so it is not a token, not transferable, and says nothing about any
  later instant (hence `checked_at`). A consumer needing a boundary between
  distrusting parties needs OS-level isolation — separate users, containers,
  sandboxes — and `attest` is a check *within* one such boundary rather than a
  substitute for it. See [`docs/control-plane.md`](control-plane.md), "The
  boundary: containment, not authentication".
- **Trust in the `dtolnay/rust-toolchain` action owner.** Both workflows
  deliberately leave that one action unpinned (a floating `@stable`/`@master`
  tag rather than a commit SHA) so CI and releases keep tracking the rolling
  stable/MSRV toolchain; a compromise of that action's owner or repository
  could change what CI or a release build runs, and the project accepts that
  residual risk rather than freezing the toolchain version.
- **A package channel published from this repository's own secrets.**
  Automatic publication to a Homebrew tap or Scoop bucket requires a standing
  credential that can push to that *other* repository to sit in **this**
  repository's Actions secrets for as long as the channel is enabled. So an
  enabled channel is only as isolated as this repository's secrets and the set
  of actors who can run its release workflow: whoever can read that secret, or
  dispatch a release carrying it, can put content into the repository users
  install from. That much is not new with the tap: crates.io is already
  published on every release from a `CRATES_IO_TOKEN` sitting in these same
  secrets. What a tap or bucket adds is a *cross-repository git write*
  capability, which is what the automation fundamentally is, and the project
  accepts that residual risk as the price of a channel that publishes itself
  instead of by hand. The alternative it rejects is not "a safer token"; it is
  publishing every release manually.

  The two bounds stated on that capability above are **not** equally strong,
  and the weaker one is the residual risk this entry accepts. That the
  publishing job holds no write access to *this* repository is enforced: its
  `GITHUB_TOKEN` is narrowed by a job-level `permissions: contents: read`
  block, and every write it performs is authenticated by the channel's own
  token instead. That the channel token reaches the tap/bucket and nothing
  else is not enforced anywhere — it is a requirement this project documents
  for the operator who mints the secret, and nothing here verifies it or could
  detect its violation: the `Resolve publication targets` step tests the token
  only for emptiness, and a classic PAT carrying `repo` scope over every
  repository its owner can reach would clone, commit, and push exactly the
  same way, on an otherwise green release. The anchored `owner/name`
  validation bounds the target the job writes *to*, not what the credential is
  permitted to do. So the blast radius of an enabled channel is whatever scope
  the operator actually granted, not the scope asked for here — enabling a
  channel accepts that gap along with the capability itself.
- **Build-provenance verification of a package installed from that channel.**
  The formula/manifest such a channel serves is **not** covered by the
  attestation described above. Homebrew and Scoop trust a formula because of
  the repository it came from; `gh attestation verify`, which a consumer can
  run against a release archive downloaded directly, has no place in the
  `brew install` path, and neither the formula nor the manifest bundle is
  itself attested. The only thing tying an install through that channel to the
  attested release artifacts is the per-archive `sha256` the generated
  formula/manifest pins, taken from the Release's own `.sha256` sidecars
  (`scripts/generate_package_manifests.py`) — a formula pushed with some other
  URL/digest pair would install whatever it names. A consumer who needs
  provenance should run that verification against a downloaded archive itself
  (see [`README.md`](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/README.md), "Prebuilt binaries"), rather than
  infer it from the channel. Neither the tap nor the bucket is configured
  today, so this particular gap is a property of that mechanism accepted in
  advance rather than a live exposure through those two channels — which says
  nothing about crates.io, a live channel whose published source crate this
  attestation does not cover either.
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
