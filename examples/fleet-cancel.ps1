$ErrorActionPreference = 'Stop'
$bin = if ($env:PROCESSKIT_CLI_BIN) { $env:PROCESSKIT_CLI_BIN } else { 'processkit-cli' }
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("processkit-cli-example-" + [guid]::NewGuid())
$label = "example=fleet-$PID"
New-Item -ItemType Directory -Path $scratch | Out-Null

try {
    foreach ($member in 'one', 'two') {
        & $bin run --detach --run-id "example-fleet-$member-$PID" --label $label `
            --timeout 30s --create-no-window --jsonl (Join-Path $scratch "$member.jsonl") -- `
            pwsh -NoProfile -NonInteractive -Command 'Start-Sleep -Seconds 20'
        if ($LASTEXITCODE -ne 0) { throw "Detached launch failed with exit code $LASTEXITCODE" }
    }

    $runs = @(& $bin list --json --label $label | ForEach-Object { $_ | ConvertFrom-Json })
    if ($LASTEXITCODE -ne 0 -or $runs.Count -ne 2) { throw "Expected two labeled runs, got $($runs.Count)" }
    & $bin cancel --all --label $label
    if ($LASTEXITCODE -ne 0) { throw "Fleet cancel failed with exit code $LASTEXITCODE" }
    & $bin wait --all --label $label --timeout 10s
    if ($LASTEXITCODE -ne 0) { throw "Fleet wait failed with exit code $LASTEXITCODE" }

    foreach ($member in 'one', 'two') {
        $terminal = Get-Content -LiteralPath (Join-Path $scratch "$member.jsonl") |
            Select-Object -Last 1 | ConvertFrom-Json
        if ($terminal.event -ne 'runner_exit' -or $terminal.source -ne 'control_cancel') {
            throw "Unexpected terminal event: $($terminal | ConvertTo-Json -Compress)"
        }
    }
}
finally {
    & $bin cancel --all --label $label *> $null
    & $bin wait --all --label $label --timeout 5s *> $null
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
