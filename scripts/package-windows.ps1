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
제3자 라이선스 고지는 이 패키지에 포함되지 않았습니다. 배포 전에 라이선스를 검토하고 필요한 고지를 제공해야 합니다.
"@
[IO.File]::WriteAllText((Join-Path $folder 'README.txt'), $readme, [Text.UTF8Encoding]::new($false))
Add-Type -AssemblyName System.IO.Compression
$stream = [IO.File]::Open($archive, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
    try {
        foreach ($entry in @($names + 'README.txt')) {
            [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, (Join-Path $folder $entry), $entry) | Out-Null
        }
    } finally { $zip.Dispose() }
} finally { $stream.Dispose() }
[pscustomobject]@{ PackageDirectory = $folder; ArchivePath = $archive }