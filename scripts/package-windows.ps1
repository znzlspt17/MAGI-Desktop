[CmdletBinding()]
param([Parameter(Mandatory)][string]$BuildDirectory)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-SafeExistingPath([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        $item = Get-Item -LiteralPath $current -Force -ErrorAction Stop
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "재분석 지점은 사용할 수 없습니다: $current"
        }
        $current = [IO.Path]::GetDirectoryName($current)
    }
}

$root = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$build = [IO.Path]::GetFullPath($BuildDirectory)
Assert-SafeExistingPath $root
Assert-SafeExistingPath $build
if (-not (Test-Path -LiteralPath $build -PathType Container)) {
    throw '빌드 폴더가 필요합니다.'
}
$names = @('desktop.exe', 'engine.exe', 'inference.exe')
foreach ($name in $names) {
    $source = Join-Path $build $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "실행 파일이 없습니다: $name"
    }
    Assert-SafeExistingPath $source
}

# 현재 workspace의 잠금 의존성을 기준으로 공개 라이선스 원문만 수집한다.
Push-Location $root
try {
    $rustInfo = @(& rustc -vV)
    if ($LASTEXITCODE -ne 0) { throw 'Rust host 대상을 확인할 수 없습니다.' }
    $hostLine = $rustInfo | Where-Object { $_ -like 'host: *' } | Select-Object -First 1
    if (-not $hostLine) { throw 'Rust host 대상을 확인할 수 없습니다.' }
    $hostTarget = $hostLine.Substring(6)
    $packages = @(& cargo tree --locked --offline --edges normal,build --target $hostTarget -p desktop -p engine -p inference --prefix none --format '{p}|{l}')
    if ($LASTEXITCODE -ne 0 -or $packages.Count -eq 0) { throw '잠금 의존성 목록을 확인할 수 없습니다.' }
} finally { Pop-Location }

$cargoHome = if ([string]::IsNullOrEmpty($env:CARGO_HOME)) { Join-Path $env:USERPROFILE '.cargo' } else { $env:CARGO_HOME }
$sourceRoot = Join-Path $cargoHome 'registry/src'
if (Test-Path -LiteralPath $sourceRoot -PathType Container) { Assert-SafeExistingPath $sourceRoot }
$registries = if (Test-Path -LiteralPath $sourceRoot -PathType Container) { @(Get-ChildItem -LiteralPath $sourceRoot -Directory) } else { @() }
$notices = [Collections.Generic.List[string]]::new()
$missing = [Collections.Generic.List[string]]::new()
$seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$notices.Add('현재 workspace의 Cargo.lock과 host 대상 의존성을 기준으로 수집한 공개 고지입니다.')
$notices.Add('입력 실행 파일이 현재 workspace에서 빌드되었는지는 확인하지 않았습니다. 배포 전 별도 검토가 필요합니다.')
foreach ($line in @($packages | Sort-Object -Unique)) {
    if ([string]::IsNullOrWhiteSpace($line)) { continue }
    if ($line -notmatch '^(?<crate>[^ ]+) v(?<version>[^ |]+)(?: \((?<location>[^)]*)\))?\|(?<license>.*?)(?: \(\*\))?$') { throw 'Cargo 의존성 목록 형식이 예상과 다릅니다.' }
    $crate = $Matches.crate; $version = $Matches.version; $license = $Matches.license
    if ($Matches['location'] -and $Matches['location'].StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { continue }
    if (-not $seen.Add("$crate|$version|$license")) { continue }
    $notices.Add("`n===== $crate $version | $license =====")
    $dirs = @($registries | ForEach-Object { Join-Path $_.FullName "$crate-$version" } | Where-Object { Test-Path -LiteralPath $_ -PathType Container })
    if ($dirs.Count -ne 1) { $missing.Add("$crate $version : 공개 소스가 없거나 중복됨"); continue }
    try { Assert-SafeExistingPath $dirs[0] } catch { $missing.Add("$crate $version : 공개 소스 경로 확인 실패"); continue }
    $licenseDirs = @($dirs[0])
    foreach ($subdir in @('licenses','LICENSES')) {
        $path = Join-Path $dirs[0] $subdir
        if (Test-Path -LiteralPath $path -PathType Container) {
            try { Assert-SafeExistingPath $path; $licenseDirs += $path } catch { $missing.Add("$crate $version : $subdir 경로 확인 실패") }
        }
    }
    $files = @($licenseDirs | ForEach-Object { Get-ChildItem -LiteralPath $_ -File } | Where-Object { $_.Name -match '^(?i:LICENSE|LICENCE|COPYING|NOTICE)(?:[._-].*)?$' } | Sort-Object FullName)
    if ($files.Count -eq 0) { $missing.Add("$crate $version : 라이선스 원문 없음") }
    foreach ($file in $files) {
        if ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) { $missing.Add("$crate $version : 재분석 지점 고지 제외"); continue }
        $notices.Add("`n--- $($file.Name) ---`n")
        $notices.Add([IO.File]::ReadAllText($file.FullName))
    }
    if ($crate -eq 'epaint_default_fonts' -and $version -eq '0.31.1') {
        foreach ($font in @('emoji-icon-font-mit-license.txt','Hack-Regular.txt','OFL.txt','UFL.txt')) {
            $fontPath = Join-Path $dirs[0] "fonts/$font"
            if (-not (Test-Path -LiteralPath $fontPath -PathType Leaf)) { $missing.Add("$crate $version : fonts/$font 없음"); continue }
            try { Assert-SafeExistingPath $fontPath } catch { $missing.Add("$crate $version : fonts/$font 경로 확인 실패"); continue }
            $fontFile = Get-Item -LiteralPath $fontPath -Force
            if ($fontFile.Attributes -band [IO.FileAttributes]::ReparsePoint) { $missing.Add("$crate $version : fonts/$font 재분석 지점 제외"); continue }
            $notices.Add("`n--- fonts/$font ---`n")
            $notices.Add([IO.File]::ReadAllText($fontPath))
        }
    }
}
$notices.Add("`n===== 누락된 고지 =====")
if ($missing.Count -eq 0) { $notices.Add('확인된 누락 없음. 별도의 배포 검토가 필요합니다.') } else { foreach ($entry in $missing) { $notices.Add($entry) } }

$dist = Join-Path $root 'dist'
if (Test-Path -LiteralPath $dist) {
    Assert-SafeExistingPath $dist
    if (-not (Test-Path -LiteralPath $dist -PathType Container)) {
        throw 'dist는 폴더여야 합니다.'
    }
} else {
    New-Item -ItemType Directory -Path $dist -ErrorAction Stop | Out-Null
}
$name = 'windows-' + [guid]::NewGuid().ToString('N')
$folder = Join-Path $dist $name
$archive = Join-Path $dist ($name + '.zip')
New-Item -ItemType Directory -Path $folder -ErrorAction Stop | Out-Null
foreach ($binary in $names) {
    [IO.File]::Copy((Join-Path $build $binary), (Join-Path $folder $binary), $false)
}
$readme = @"
MAGI Windows 개발용 패키지

desktop, engine, inference는 각각 별도 프로세스입니다.
실제 모델 추론은 아직 지원하지 않습니다. 모의 응답을 실제 추론으로 간주하지 마세요.
모델, 데이터베이스, 로그 및 인증 정보는 포함하지 않습니다.
제3자 고지는 THIRD-PARTY-NOTICES.txt를 확인하세요. 누락된 고지와 입력 빌드의 출처를 검토하기 전에는 배포할 수 없습니다.
"@
[IO.File]::WriteAllText((Join-Path $folder 'README.txt'), $readme, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllLines((Join-Path $folder 'THIRD-PARTY-NOTICES.txt'), $notices, [Text.UTF8Encoding]::new($false))
Add-Type -AssemblyName System.IO.Compression
$stream = [IO.File]::Open($archive, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
    try {
        foreach ($entry in @($names + @('README.txt', 'THIRD-PARTY-NOTICES.txt'))) {
            [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, (Join-Path $folder $entry), $entry) | Out-Null
        }
    } finally { $zip.Dispose() }
} finally { $stream.Dispose() }
[pscustomobject]@{ PackageDirectory = $folder; ArchivePath = $archive }
