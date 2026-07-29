$ErrorActionPreference = 'Stop'
$bin = if ($env:PROCESSKIT_CLI_BIN) { $env:PROCESSKIT_CLI_BIN } else { 'processkit-cli' }
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("processkit-cli-example-" + [guid]::NewGuid())
$runId = "example-detached-$PID"
$events = Join-Path $scratch 'events.jsonl'
$release = Join-Path $scratch 'release-child'
New-Item -ItemType Directory -Path $scratch | Out-Null

try {
    & $bin run --detach --run-id $runId --timeout 30s --create-no-window `
        --env "PROCESSKIT_EXAMPLE_RELEASE=$release" `
        --jsonl $events -- pwsh -NoProfile -NonInteractive -Command `
        'while (-not (Test-Path -LiteralPath $env:PROCESSKIT_EXAMPLE_RELEASE)) { Start-Sleep -Milliseconds 50 }'
    if ($LASTEXITCODE -ne 0) { throw "Detached launch failed with exit code $LASTEXITCODE" }

    $snapshot = (& $bin inspect --run-id $runId --json | ConvertFrom-Json)
    if ($LASTEXITCODE -ne 0 -or $snapshot.run_id -ne $runId) { throw 'Inspect returned the wrong run' }
    New-Item -ItemType File -Path $release | Out-Null
    & $bin wait --run-id $runId --timeout 10s
    if ($LASTEXITCODE -ne 0) { throw "Wait failed with exit code $LASTEXITCODE" }

    $terminal = Get-Content -LiteralPath $events | Select-Object -Last 1 | ConvertFrom-Json
    if ($terminal.event -ne 'runner_exit' -or $terminal.code -ne 0) {
        throw "Unexpected terminal event: $($terminal | ConvertTo-Json -Compress)"
    }
}
finally {
    & $bin cancel --run-id $runId *> $null
    & $bin wait --run-id $runId --timeout 5s *> $null
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
