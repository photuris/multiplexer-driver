# Smoke test for multiplexer-driver on Windows. Run it in PowerShell
# inside Herdr, with multiplexer-driver installed. It creates one
# workspace, md-winsmoke, and closes it at the end.
$ErrorActionPreference = 'Stop'

$ws = $null

# Runs multiplexer-driver on Herdr and returns the stdout lines.
function Invoke-Md {
    $out = & multiplexer-driver --harness herdr @args
    if ($LASTEXITCODE -ne 0) {
        throw "multiplexer-driver $args exited $LASTEXITCODE"
    }
    return $out
}

try {
    $ws = (Invoke-Md workspace create --label md-winsmoke |
        ConvertFrom-Json).workspace_id

    $handle = (Invoke-Md pane spawn --name md-winsmoke-1 --workspace $ws `
        -- powershell -NoProfile -Command "Write-Output ('win-' + (6*7))" |
        ConvertFrom-Json).handle

    $found = $false
    for ($i = 0; $i -lt 20 -and -not $found; $i++) {
        $text = (Invoke-Md pane read $handle | ConvertFrom-Json).output
        $found = @($text -split "\r?\n" | ForEach-Object { $_.Trim() }) `
            -contains 'win-42'
        if (-not $found) { Start-Sleep -Milliseconds 500 }
    }
    if (-not $found) { throw "pane $handle never printed win-42" }

    $tags = (Invoke-Md workspace tag $ws project=smoke | ConvertFrom-Json).tags
    if ($tags.project -ne 'smoke') {
        throw "tags.project is '$($tags.project)', expected 'smoke'"
    }

    $pane = Invoke-Md pane list --workspace $ws | ConvertFrom-Json |
        Where-Object { $_.handle -eq $handle }
    if ($pane.label -ne 'md-winsmoke-1') {
        throw "pane label is '$($pane.label)', expected 'md-winsmoke-1'"
    }

    & multiplexer-driver --harness herdr pane read "${ws}:p999" 2>$null |
        Out-Null
    if ($LASTEXITCODE -ne 4) {
        throw "pane read of a missing pane exited $LASTEXITCODE, expected 4"
    }

    Write-Output 'PASS'
}
catch {
    Write-Output "FAIL: $($_.Exception.Message)"
    $failed = $true
}
finally {
    if ($ws) {
        & multiplexer-driver --harness herdr workspace close $ws 2>$null |
            Out-Null
    }
}
if ($failed) { exit 1 }
