$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Split-Path -Parent $PSScriptRoot)).Path
$targetRoot = Join-Path $root 'target'
if (Test-Path -LiteralPath $targetRoot) {
    if ((Get-Item -LiteralPath $targetRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'target은 재분석 지점일 수 없습니다.'
    }
} else {
    New-Item -ItemType Directory -Path $targetRoot | Out-Null
}
$output = Join-Path $targetRoot ('windows-build-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $output -ErrorAction Stop | Out-Null
Push-Location $root
try {
    $revision = @(& git rev-parse --verify HEAD)
    if ($LASTEXITCODE -ne 0 -or $revision.Count -ne 1 -or $revision[0] -cnotmatch '^(?:[0-9a-f]{40}|[0-9a-f]{64})$') { throw '소스 revision을 확인할 수 없습니다.' }
    $changes = @(& git status --porcelain --untracked-files=normal)
    if ($LASTEXITCODE -ne 0) { throw '소스 변경 상태를 확인할 수 없습니다.' }
    $rustInfo = @(& rustc -vV)
    if ($LASTEXITCODE -ne 0) { throw 'Rust 버전을 확인할 수 없습니다.' }
    $versionLines = @($rustInfo | Where-Object { $_ -like 'release: *' })
    $hostLines = @($rustInfo | Where-Object { $_ -like 'host: *' })
    if ($versionLines.Count -ne 1 -or $hostLines.Count -ne 1) { throw 'Rust 버전 또는 대상이 없습니다.' }
    $version = $versionLines[0].Substring(9); $target = $hostLines[0].Substring(6)
    if ($version -cnotmatch '^[0-9][0-9A-Za-z.+-]{0,63}$' -or $target -cnotmatch '^[A-Za-z0-9_.-]{1,100}$') { throw 'Rust 버전 또는 대상 형식이 잘못되었습니다.' }
    & cargo build --release --locked --workspace --target $target --target-dir $output
    if ($LASTEXITCODE -ne 0) { throw "Windows 빌드 실패. 출력 경로: $output" }
    $binaries = @('desktop.exe', 'engine.exe', 'inference.exe')
    $release = Join-Path $output 'release'
    $targetRelease = Join-Path (Join-Path $output $target) 'release'
    foreach ($binary in $binaries) {
        if (-not (Test-Path -LiteralPath (Join-Path $targetRelease $binary) -PathType Leaf)) { throw "빌드 실행 파일이 없습니다: $binary" }
    }
    if (Test-Path -LiteralPath $release) {
        $releaseItem = Get-Item -LiteralPath $release -Force
        if (-not $releaseItem.PSIsContainer -or ($releaseItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'release 출력 폴더가 잘못되었습니다.' }
    } else {
        New-Item -ItemType Directory -Path $release -ErrorAction Stop | Out-Null
    }
    foreach ($binary in $binaries) {
        [IO.File]::Copy((Join-Path $targetRelease $binary), (Join-Path $release $binary), $false)
    }
    $record = [ordered]@{ schemaVersion = 1; sourceRevision = $revision[0]; sourceDirty = [bool]$changes.Count; rustVersion = $version; target = $target; binaries = $binaries }
    $recordPath = Join-Path $release 'build-info.json'
    $stream = [IO.File]::Open($recordPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $writer = [IO.StreamWriter]::new($stream, [Text.UTF8Encoding]::new($false))
        try { $writer.Write(($record | ConvertTo-Json -Depth 4)) } finally { $writer.Dispose() }
    } finally { $stream.Dispose() }
    Write-Output "개발 빌드 출력: $output"
    Write-Warning '개발용 빌드입니다. 배포용 패키지와 제3자 라이선스 고지는 생성하지 않았습니다.'
} finally {
    Pop-Location
}
