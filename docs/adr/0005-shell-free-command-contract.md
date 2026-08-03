# 0005: Keep command execution shell-free

- Status: Accepted
- Date: 2026-07-29 (retrospective)

## Context

Implicit shell parsing changes quoting across platforms, expands variables and
globs, and introduces an injection boundary between an automation adapter's argv
and the program that receives it.

## Decision

Interpret every token after `--` as `<program> <args...>` and pass it directly to
ProcessKit. Do not provide a shell mode. A caller that needs shell language must
name the shell explicitly (`sh -c`, `cmd /c`, or equivalent) and therefore owns the
shell's quoting and security boundary.

See [Running commands](../running-commands.md) for platform examples.
The boundary is declared by the
[CLI parser](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/cli/run.rs) and passed
to the child without a shell by the
[launch path](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/src/run/launch.rs).

## Alternatives considered

- Add `--shell`. Rejected because its quoting semantics cannot be portable and it
  makes unsafe interpolation deceptively convenient.
- Join argv into one command string. Rejected because round-tripping arbitrary
  arguments through a shell is lossy and injection-prone.
- Auto-detect shell metacharacters. Rejected because implicit behavior is harder to
  reason about than an explicit shell executable.

## Consequences

Adapters can construct argv without another parser rewriting it, and Windows/Unix
behavior stays aligned. Shell pipelines require a deliberate extra program token;
examples must make that boundary visible rather than implying expansion by the CLI.
