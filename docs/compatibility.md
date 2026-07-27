# Compatibility and upgrades

ProcessKit CLI has three public compatibility surfaces:

1. command names and flags;
2. the reserved runner exit-code band;
3. JSONL `schema_version`.

Breaking any of them requires a major release. An adapter should verify the
exact pieces it uses before launching a payload rather than discovering an
incompatible binary after work has started.

## Fail-closed preflight

```sh
processkit-cli probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run \
  --require-surface run:--jsonl \
  --require-surface run:--capture-dir \
  --require-surface inspect:--json \
  --require-surface cancel
```

`probe` is side-effect-free. It launches no child, creates no container, and
does not touch the registry. A missing requirement exits
`PROBE_INCOMPATIBLE` (`110`) and reports all mismatches.

## Surface tokens

A surface token is either a subcommand or `subcommand:--long-flag`:

```text
run
run:--jsonl
run:--idle-timeout
inspect
inspect:--json
cancel:--all
```

Require only the features the adapter will actually use. This permits additive
CLI releases while preventing an invocation from reaching a binary that lacks
a needed flag.

## Schema pinning

Every event carries `schema_version`. An adapter should reject an unknown value
before interpreting event-specific fields. The current schema is documented in
[JSONL event schema](schema.md) and available from the installed binary:

```sh
processkit-cli probe --json --print-schema > schema.json
```

`--print-schema` is mutually exclusive with `--require-*`: printing the
document and checking compatibility are separate operations, so neither can be
silently skipped.

Within one schema version, readers must tolerate additive optional fields and
unknown event fields. Removing or changing the meaning/type of an existing
field requires a new schema version.

## Exit-code compatibility

Normal foreground runs forward the child code. Runner-owned outcomes occupy
`100`–`119`. Because a child may itself return a number inside that band, the
terminal `runner_exit.child_code` / runner reason remains the authoritative
machine distinction.

Verify the whole reserved band, not only the individual codes your current
version knows. This protects space for additive runner outcomes without making
an old adapter classify them as child exits.

Detached launch is the documented exception: launcher `0` means the run
started, and the child's eventual code is in terminal JSONL.

## Upgrade procedure

1. Download and verify the new archive without replacing the active binary.
2. Run the new binary by absolute path with the adapter's full `probe`
   requirements.
3. Compare `probe --print-schema` with the adapter's pinned schema version.
4. Exercise one harmless run through the same I/O, capture, and control-plane
   flags used in production.
5. Confirm `run_started.mechanism` in the actual deployment environment.
6. Atomically replace the installed binary.
7. Keep the previous binary until terminal events from the smoke run have been
   consumed successfully.

## Reading release notes

The [project changelog](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/CHANGELOG.md)
uses Keep a Changelog categories. Look for:

- `Added`: optional flags/events/features an adapter may adopt;
- `Changed`: behavior that remains inside the current compatibility contract;
- `Fixed`: corrections that may alter previously buggy observations;
- `Removed`: major-version migration work.

The manifest version, `v<version>` tag, crates.io artifact, and GitHub Release
are produced by one release workflow and should identify the same release.

## Rolling upgrades with live runs

The control plane lives inside each running process. Replacing the executable
on disk does not upgrade a runner already in memory. A new client must therefore
remain compatible with the live run's registry/control/schema contract until
that run finishes.

For a conservative rolling upgrade:

1. stop launching new work through the old binary;
2. let or cancel old live runs;
3. wait for the registry to become empty;
4. preview and prune stale entries;
5. replace the binary and launch new work.

Do not use recorded PIDs to bridge versions.

## Downgrades

Run the target older binary's `probe` by absolute path before replacing the
newer one. An older binary may lack an additive flag even when the JSONL schema
version is unchanged. Surface-token verification catches that case.

Preserve event files across a downgrade; they are durable observations from the
binary that produced them and must be decoded according to their own
`schema_version`, not the currently installed executable.

## Adapter acceptance checklist

- exact supported `schema_version`;
- exact reserved exit-code band;
- every command and long flag the adapter invokes;
- required containment `mechanism`;
- required `abrupt_cleanup` strength;
- I/O mode/capture assumptions;
- resource-limit applicability in the real environment.

## See also

- [Integration guide](integration.md) — complete adapter lifecycle.
- [Exit-code contract](exit-codes.md).
- [JSONL event schema](schema.md).
- [Platform support](platform-support.md).
