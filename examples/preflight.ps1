$ErrorActionPreference = 'Stop'
$bin = if ($env:PROCESSKIT_CLI_BIN) { $env:PROCESSKIT_CLI_BIN } else { 'processkit-cli' }

& $bin probe --json `
    --require-schema-version 1 `
    --require-exit-code-band 100-119 `
    --require-surface 'run:--jsonl' `
    --require-surface 'run:--timeout' `
    --require-surface 'inspect:--json' `
    --require-surface 'wait:--all'
if ($LASTEXITCODE -ne 0) { throw "ProcessKit CLI preflight failed with exit code $LASTEXITCODE" }
