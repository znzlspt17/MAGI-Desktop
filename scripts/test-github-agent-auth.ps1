$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'github-agent-auth.ps1')

function Assert-That([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw $Message }
}

# 다른 저장소나 외부 호스트에는 인증 헤더를 보낼 수 없어야 한다.
foreach ($unsafePath in @('https://example.com/steal', '//example.com', '/repos/other/project/pulls', '/repos/znzlspt17/MAGI-Desktop/../other/pulls', '/repos/znzlspt17/MAGI-Desktop/pulls?redirect=https://example.com', '/repos/znzlspt17/MAGI-Desktop/pulls%2f..')) {
    $rejected = $false
    try { $null = Assert-MagiGitHubTarget -Method POST -Path $unsafePath } catch { $rejected = $true }
    Assert-That $rejected 'Unsafe API target was accepted'
}
foreach ($safePath in @('/app', '/repos/znzlspt17/MAGI-Desktop/installation', '/repos/znzlspt17/MAGI-Desktop', '/repos/znzlspt17/MAGI-Desktop/pulls?state=open&per_page=100', '/installation/repositories')) {
    Assert-That (Assert-MagiGitHubTarget -Method GET -Path $safePath) 'Valid read operation was rejected'
}
foreach ($gitPath in @('/repos/znzlspt17/MAGI-Desktop/git/trees', '/repos/znzlspt17/MAGI-Desktop/git/commits', '/repos/znzlspt17/MAGI-Desktop/git/refs')) {
    Assert-That (Assert-MagiGitHubTarget -Method POST -Path $gitPath) 'Scoped Git data write must be accepted for the publisher'
}
$readonlyRejected=$false
try { $null=Assert-MagiGitWrite -Role pm -Path '/repos/znzlspt17/MAGI-Desktop/git/refs' -Body @{ref='refs/heads/codex/pm/work'} } catch { $readonlyRejected=$true }
Assert-That $readonlyRejected 'Read-only role must not write Git data'
$wrongBranchRejected=$false
try { $null=Assert-MagiGitWrite -Role devops -Path '/repos/znzlspt17/MAGI-Desktop/git/refs' -Body @{ref='refs/heads/main'} } catch { $wrongBranchRejected=$true }
Assert-That $wrongBranchRejected 'Publisher must only create its own role branch'
$rsa = [System.Security.Cryptography.RSA]::Create(2048)
try {
    $jwt = New-MagiAppJwt -Rsa $rsa -AppId '12345'
    $parts = $jwt.Split('.')
    Assert-That ($parts.Count -eq 3) 'JWT must contain three signed segments'
    function Decode-Base64Url([string]$Value) {
        $s = $Value.Replace('-', '+').Replace('_', '/')
        [Convert]::FromBase64String($s.PadRight($s.Length + ((4 - $s.Length % 4) % 4), '='))
    }
    $header = [Text.Encoding]::UTF8.GetString((Decode-Base64Url $parts[0])) | ConvertFrom-Json
    $payload = [Text.Encoding]::UTF8.GetString((Decode-Base64Url $parts[1])) | ConvertFrom-Json
    $signature = Decode-Base64Url $parts[2]
    Assert-That ($header.alg -eq 'RS256') 'JWT algorithm must be RS256'
    Assert-That ($payload.iss -eq '12345') 'JWT issuer must match the selected app'
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    Assert-That ($payload.iat -le $now -and $payload.iat -ge ($now - 120)) 'JWT issued time must allow bounded clock skew'
    Assert-That ($payload.exp -gt $now -and $payload.exp -le ($now + 600)) 'JWT lifetime must be at most ten minutes'
    Assert-That ($rsa.VerifyData([Text.Encoding]::UTF8.GetBytes($parts[0] + '.' + $parts[1]), $signature, [Security.Cryptography.HashAlgorithmName]::SHA256, [Security.Cryptography.RSASignaturePadding]::Pkcs1)) 'JWT signature is invalid'
} finally { $rsa.Dispose() }
Write-Output 'PASS: request scope and ephemeral RSA JWT signature/lifetime checks'
