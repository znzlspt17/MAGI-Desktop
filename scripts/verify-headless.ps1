$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $crates = @('core', 'engine', 'provider', 'integrations', 'transport', 'inference')
    foreach ($crate in $crates) {
        & cargo fmt -p $crate -- --check
        if ($LASTEXITCODE -ne 0) { throw "cargo fmt 검사 실패: $crate" }
    }
    & cargo test --locked --workspace --exclude desktop
    if ($LASTEXITCODE -ne 0) { throw 'cargo test 실패' }
    & cargo clippy --locked --workspace --exclude desktop --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'cargo clippy 검사 실패' }
} finally { Pop-Location }
