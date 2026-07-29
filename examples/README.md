# Runnable examples

These scripts exercise the installed `processkit-cli` binary through its public
command-line contract. Set `PROCESSKIT_CLI_BIN` to test a specific build; otherwise
the scripts resolve `processkit-cli` from `PATH`.

| Scenario | Use it when |
| --- | --- |
| [`preflight`](preflight.sh) ([PowerShell](preflight.ps1)) | An adapter must fail closed when the installed schema, exit band, or required flags drift. |
| [`run-and-read-events`](run-and-read-events.sh) ([PowerShell](run-and-read-events.ps1)) | A foreground supervisor needs a finite deadline and must parse the terminal JSONL event. |
| [`detach-inspect-wait`](detach-inspect-wait.sh) ([PowerShell](detach-inspect-wait.ps1)) | A launcher hands work to the live runner, then supervises it from separate client invocations. |
| [`fleet-cancel`](fleet-cancel.sh) ([PowerShell](fleet-cancel.ps1)) | An operator labels several runs, discovers the matching fleet, and stops only that snapshot. |

The POSIX scripts require `python3` for JSON parsing. The PowerShell scripts require
PowerShell 7 (`pwsh`). Both families create unique run ids and scratch directories,
and clean up through the control plane. They never search for or terminate processes
by pid or executable name.

Every shell appears explicitly after `--`: ProcessKit CLI deliberately has no shell
mode. The examples use small platform-native commands only to provide deterministic
children for the runner.
