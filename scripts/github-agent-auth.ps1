# 인증 자료는 이 전송 경계 내부에서만 처리하며 호출자에게 반환하지 않는다.
function Assert-MagiGitHubTarget {
    param([string]$Method, [string]$Path)
    $repo = '/repos/znzlspt17/MAGI-Desktop'
    $reads = @('/app', "$repo/installation", '/installation/repositories', $repo, "$repo/pulls")
    if ($Method -ceq 'GET' -and ($Path -cin $reads -or $Path -cmatch '^/repos/znzlspt17/MAGI-Desktop/pulls/[1-9][0-9]*$' -or $Path -cmatch '^/repos/znzlspt17/MAGI-Desktop/pulls\?state=(open|closed|all)&per_page=100$')) { return $true }
    if ($Method -ceq 'GET' -and ($Path -ceq "$repo/git/ref/heads/dev" -or $Path -cmatch '^/repos/znzlspt17/MAGI-Desktop/git/commits/[a-f0-9]{40}$')) { return $true }
    if ($Method -ceq 'POST' -and $Path -cin @("$repo/git/trees", "$repo/git/commits", "$repo/git/refs")) { return $true }
    if ($Method -ceq 'POST' -and ($Path -ceq "$repo/pulls" -or $Path -cmatch '^/repos/znzlspt17/MAGI-Desktop/pulls/[1-9][0-9]*/reviews$')) { return $true }
    throw 'API operation or repository is not allowed.'
}

function ConvertTo-MagiBase64Url([byte[]]$Bytes) {
    [Convert]::ToBase64String($Bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

function New-MagiAppJwt {
    param([System.Security.Cryptography.RSA]$Rsa, [string]$AppId)
    if ($AppId -cnotmatch '^[1-9][0-9]*$') { throw 'Invalid app identifier.' }
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $header = ConvertTo-MagiBase64Url ([Text.Encoding]::UTF8.GetBytes('{"alg":"RS256","typ":"JWT"}'))
    $payload = ConvertTo-MagiBase64Url ([Text.Encoding]::UTF8.GetBytes((@{ iat=$now-60; exp=$now+540; iss=$AppId } | ConvertTo-Json -Compress)))
    $signature = $Rsa.SignData([Text.Encoding]::UTF8.GetBytes("$header.$payload"), [Security.Cryptography.HashAlgorithmName]::SHA256, [Security.Cryptography.RSASignaturePadding]::Pkcs1)
    "$header.$payload.$(ConvertTo-MagiBase64Url $signature)"
}

function Assert-MagiGitWrite {
    param([string]$Role, [string]$Path, [object]$Body)
    if ($Role -cnotin @('dev','designer','devops','rust_desktop','rust_inference')) { throw 'This role cannot write Git data.' }
    if ($Path.EndsWith('/git/refs') -and ($Body.ref -cnotmatch ('^refs/heads/codex/' + [regex]::Escape($Role) + '/[^\s]+$'))) { throw 'Only the selected role branch can be created.' }
    if ($Path.EndsWith('/git/commits') -and (@($Body.parents).Count -ne 1 -or $Body.parents[0] -cnotmatch '^[a-f0-9]{40}$')) { throw 'A single existing commit parent is required.' }
    if ($Path.EndsWith('/git/trees') -and $Body.base_tree -cnotmatch '^[a-f0-9]{40}$') { throw 'An existing base tree is required.' }
    return $true
}

function Invoke-MagiHttpJson {
    param([string]$Method, [string]$Path, [string]$Bearer, [object]$Body)
    $handler = [Net.Http.HttpClientHandler]::new()
    # 리디렉션으로 인증을 전송하거나 오류 본문으로 토큰을 노출하지 않는다.
    $handler.AllowAutoRedirect = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::new($Method), 'https://api.github.com' + $Path)
    $response = $null
    try {
        $client.Timeout = [TimeSpan]::FromSeconds(30)
        $request.Headers.UserAgent.ParseAdd('MAGI-Desktop-Agent/1.0')
        $request.Headers.Accept.ParseAdd('application/vnd.github+json')
        $request.Headers.Add('X-GitHub-Api-Version', '2026-03-10')
        $request.Headers.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $Bearer)
        if ($null -ne $Body) { $request.Content = [Net.Http.StringContent]::new(($Body | ConvertTo-Json -Depth 20 -Compress), [Text.Encoding]::UTF8, 'application/json') }
        try { $response = $client.SendAsync($request).GetAwaiter().GetResult() } catch { throw 'GitHub transport failed; request details withheld.' }
        if (!$response.IsSuccessStatusCode) { throw ('GitHub API returned HTTP ' + [int]$response.StatusCode + '; response details withheld.') }
        try { $response.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json -AsHashtable -Depth 50 } catch { throw 'GitHub response could not be decoded.' }
    } finally {
        if ($response) { $response.Dispose() }
        $request.Dispose()
        $client.Dispose()
        $handler.Dispose()
    }
}

function Invoke-MagiAuthenticatedRequest {
    param(
        [ValidateSet('pm','dev','designer','devops','intern','rust_desktop','rust_inference','cto')][string]$Role,
        [ValidateSet('GET','POST')][string]$Method,
        [string]$Path, [object]$Body, [string]$ConfigPath
    )
    $null = Assert-MagiGitHubTarget -Method $Method -Path $Path
    if ($Role -ceq 'intern' -and $Method -cne 'GET') { throw 'This role cannot write.' }
    $isGitWrite = $Method -ceq 'POST' -and $Path.Contains('/git/')
    if ($isGitWrite) { $null = Assert-MagiGitWrite -Role $Role -Path $Path -Body $Body }
    try { $config = Get-Content -LiteralPath $ConfigPath -Raw -ErrorAction Stop | ConvertFrom-Json -AsHashtable -ErrorAction Stop } catch { throw 'Invalid or missing app configuration.' }
    if ($config.repository -cne 'znzlspt17/MAGI-Desktop') { throw 'Repository is not allowed.' }
    $app = $config.roles[$Role]
    $expectedSlugs = @{pm='magi-ieum';dev='magi-daon';designer='magi-garam';devops='magi-jian';intern='magi-nayoon';rust_desktop='magi-seoyun';rust_inference='magi-byeori';cto='magi-seojin'}
    if (!$app -or $app.app_slug -cne $expectedSlugs[$Role] -or "$($app.app_id)" -cnotmatch '^[1-9][0-9]*$') { throw 'Role app configuration is invalid.' }
    $keyPath = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) ('MAGI/github-agents/keys/' + $app.app_slug + '.pem.dpapi')
    $rsa = [Security.Cryptography.RSA]::Create()
    $plainBytes = $null
    $jwt = $null
    $installationToken = $null
    try {
        try {
            $encryptedBytes = [IO.File]::ReadAllBytes($keyPath)
            $plainBytes = [Security.Cryptography.ProtectedData]::Unprotect($encryptedBytes, $null, [Security.Cryptography.DataProtectionScope]::CurrentUser)
            $pem = [Text.Encoding]::UTF8.GetString($plainBytes)
            $rsa.ImportFromPem($pem)
        } catch { throw 'App key is unavailable to the current Windows user.' }
        finally {
            $pem = $null
            if ($plainBytes) { [Array]::Clear($plainBytes, 0, $plainBytes.Length) }
        }
        $jwt = New-MagiAppJwt -Rsa $rsa -AppId "$($app.app_id)"
        $identity = Invoke-MagiHttpJson -Method GET -Path '/app' -Bearer $jwt -Body $null
        if ($identity.slug -cne $app.app_slug -or "$($identity.id)" -cne "$($app.app_id)") { throw 'Authenticated app does not match the selected role.' }
        if ($Path -ceq '/app') { return @{ id=$identity.id; slug=$identity.slug } }
        if ($Path -ceq '/repos/znzlspt17/MAGI-Desktop/installation') {
            $installation = Invoke-MagiHttpJson -Method GET -Path $Path -Bearer $jwt -Body $null
            if ($installation.account.login -cne 'znzlspt17' -or "$($installation.app_id)" -cne "$($app.app_id)") { throw 'Installation owner or app mismatch.' }
            return $installation
        }
        if ("$($app.installation_id)" -cnotmatch '^[1-9][0-9]*$') { throw 'App installation is not configured.' }
        $permissions = @{ contents='read'; pull_requests='read' }
        if ($Role -cne 'intern') { $permissions.pull_requests = 'write' }
        if ($isGitWrite) { $permissions.contents = 'write'; $permissions.pull_requests = 'read' }
        # API 호출마다 대상 저장소로 한정한 단기 토큰을 발급한다.
        $tokenResponse = Invoke-MagiHttpJson -Method POST -Path ('/app/installations/' + $app.installation_id + '/access_tokens') -Bearer $jwt -Body @{ repositories=@('MAGI-Desktop'); permissions=$permissions }
        $installationToken = $tokenResponse.token
        if ([string]::IsNullOrWhiteSpace($installationToken)) { throw 'Installation authentication was not issued.' }
        $tokenResponse = $null
        Invoke-MagiHttpJson -Method $Method -Path $Path -Bearer $installationToken -Body $Body
    } finally {
        $installationToken = $null
        $jwt = $null
        $rsa.Dispose()
    }
}
