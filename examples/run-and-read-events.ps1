$ErrorActionPreference = 'Stop'
$bin = if ($env:PROCESSKIT_CLI_BIN) { $env:PROCESSKIT_CLI_BIN } else { 'processkit-cli' }
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("processkit-cli-example-" + [guid]::NewGuid())
$events = Join-Path $scratch 'events.jsonl'
New-Item -ItemType Directory -Path $scratch | Out-Null

try {
    & $bin run --run-id "example-foreground-$PID" --timeout 10s --grace 1s `
        --create-no-window --jsonl $events -- `
        pwsh -NoProfile -NonInteractive -Command "Write-Output 'example child output'"
    if ($LASTEXITCODE -ne 0) { throw "Contained child failed with exit code $LASTEXITCODE" }

    $terminal = Get-Content -LiteralPath $events | Select-Object -Last 1 | ConvertFrom-Json
    if ($terminal.event -ne 'runner_exit' -or $terminal.code -ne 0) {
        throw "Unexpected terminal event: $($terminal | ConvertTo-Json -Compress)"
    }
    "terminal event: source=$($terminal.source) code=$($terminal.code)"
}
finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
