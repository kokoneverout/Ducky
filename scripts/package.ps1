$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $projectRoot
try {
    cargo build --locked --release
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
    $version = (cargo metadata --no-deps --format-version 1 | ConvertFrom-Json).packages[0].version
    $outputPath = Join-Path $projectRoot "dist\ASRInput-$version"
    New-Item -ItemType Directory -Path $outputPath -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $projectRoot 'target\release\asr-input.exe') -Destination $outputPath
    Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination $outputPath
    Copy-Item -LiteralPath (Join-Path $projectRoot 'THIRD_PARTY_NOTICES.md') -Destination $outputPath
    Copy-Item -LiteralPath (Join-Path $projectRoot 'licenses') -Destination $outputPath -Recurse -Force
    Write-Output "Package ready: $outputPath"
} finally {
    Pop-Location
}
