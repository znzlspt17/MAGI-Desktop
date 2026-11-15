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
    & cargo build --release --locked --workspace --target-dir $output
    if ($LASTEXITCODE -ne 0) { throw "Windows 빌드 실패. 출력 경로: $output" }
    Write-Output "개발 빌드 출력: $output"
    Write-Warning '개발용 빌드입니다. 배포용 패키지와 제3자 라이선스 고지는 생성하지 않았습니다.'
} finally {
    Pop-Location
}
