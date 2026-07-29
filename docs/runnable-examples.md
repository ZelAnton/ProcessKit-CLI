# Runnable examples

The repository ships four paired POSIX and PowerShell examples. They are executed
against the built binary on Linux and Windows in CI, so their public command shapes
cannot drift unnoticed.

| Scenario | Use it when | Scripts |
| --- | --- | --- |
| Compatibility preflight | An adapter must fail closed when the installed schema, exit band, or required flags drift. | [POSIX](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/preflight.sh) · [PowerShell](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/preflight.ps1) |
| Foreground JSONL | A supervisor needs a finite deadline and must parse the terminal lifecycle event. | [POSIX](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/run-and-read-events.sh) · [PowerShell](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/run-and-read-events.ps1) |
| Detached supervision | A launcher hands work to the runner, then uses separate `inspect` and `wait` clients. | [POSIX](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/detach-inspect-wait.sh) · [PowerShell](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/detach-inspect-wait.ps1) |
| Label-scoped fleet stop | An operator discovers and cancels only a uniquely labeled fleet snapshot. | [POSIX](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/fleet-cancel.sh) · [PowerShell](https://github.com/ZelAnton/ProcessKit-CLI/blob/main/examples/fleet-cancel.ps1) |

See the repository's
[examples README](https://github.com/ZelAnton/ProcessKit-CLI/tree/main/examples)
for prerequisites, `PROCESSKIT_CLI_BIN` selection, and cleanup guarantees.
