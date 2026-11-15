param(
 [string]$Role,
 [ValidateSet('WhoAmI','GetRepository','GetPullRequests','CreatePullRequest','CreateReview')][string]$Operation,
 [string]$ConfigPath = (Join-Path $PSScriptRoot '../.codex/github-apps.json'),
 [string]$InputPath,
 [int]$PullNumber
)
function Invoke-MagiAgent {
 param([string]$Role,[string]$Operation,[string]$ConfigPath,[string]$InputPath,[int]$PullNumber)
 $roles = @('pm','dev','designer','devops','intern','rust_desktop','rust_inference','cto')
 if ($Role -cnotin $roles) { throw 'Unknown role.' }
 if ($Operation -cnotin @('WhoAmI','GetRepository','GetPullRequests','CreatePullRequest','CreateReview')) { throw 'Unknown operation.' }
 if ($Role -ceq 'intern' -and $Operation -in @('CreatePullRequest','CreateReview')) { throw 'This role cannot write.' }
 try { $config = Get-Content -LiteralPath $ConfigPath -Raw -ErrorAction Stop | ConvertFrom-Json -AsHashtable -ErrorAction Stop } catch { throw 'Invalid or missing configuration.' }
 if ($config.repository -cne 'znzlspt17/MAGI-Desktop') { throw 'Repository is not allowed.' }
 $app = $config.roles[$Role]
 if (!$app -or $app.app_slug -cnotmatch '^[a-z0-9]+(?:-[a-z0-9]+)*$' -or "$($app.app_id)" -notmatch '^[1-9][0-9]*$' -or "$($app.installation_id)" -notmatch '^[1-9][0-9]*$') { throw 'Role app configuration is incomplete.' }
 if (!(Get-Command Invoke-MagiAuthenticatedRequest -ErrorAction SilentlyContinue)) { throw 'Authenticated transport unavailable.' }
 $repo='/repos/znzlspt17/MAGI-Desktop'
 try { $identity = Invoke-MagiAuthenticatedRequest -Role $Role -Method GET -Path '/app' -Body $null -ConfigPath $ConfigPath } catch { throw 'App identity request failed.' }
 if ($identity.slug -cne $app.app_slug -or "$($identity.id)" -cne "$($app.app_id)") { throw 'App identity mismatch.' }
 $actor = "$($app.app_slug)[bot]"
 if ($Operation -eq 'WhoAmI') { return @{ role=$Role; app_slug=$app.app_slug; actor=$actor } }
 if ($Operation -eq 'GetRepository') { $path=$repo; $method='GET'; $body=$null }
 elseif ($Operation -eq 'GetPullRequests') { $path="$repo/pulls"; $method='GET'; $body=$null }
 else {
  if (!$InputPath) { throw 'JSON input file required.' }
  try { $body=Get-Content -LiteralPath $InputPath -Raw -ErrorAction Stop | ConvertFrom-Json -AsHashtable -ErrorAction Stop } catch { throw 'Invalid input JSON.' }
  if ($body -isnot [System.Collections.IDictionary]) { throw 'Input must be a JSON object.' }
  $method='POST'
  if ($Operation -eq 'CreatePullRequest') {
   if (@($body.Keys | Where-Object { $_ -cnotin @('title','head','base','body','draft','maintainer_can_modify') }).Count -gt 0 -or !$body.title -or !$body.head -or !$body.base) { throw 'Invalid pull request input.' }
   if ($body.base -cne 'dev' -or $body.head -cnotmatch ('^codex/' + [regex]::Escape($Role) + '/\S+$')) { throw 'Pull request must target dev from the role branch.' }
   $path="$repo/pulls"
  } else {
   if ($PullNumber -lt 1 -or @($body.Keys | Where-Object { $_ -cnotin @('body','event','commit_id') }).Count -gt 0 -or $body.event -cnotin @('APPROVE','REQUEST_CHANGES','COMMENT')) { throw 'Invalid review input.' }
   try { $pull = Invoke-MagiAuthenticatedRequest -Role $Role -Method GET -Path "$repo/pulls/$PullNumber" -Body $null -ConfigPath $ConfigPath } catch { throw 'Pull request lookup failed.' }
   if (!$pull.user.login) { throw 'Pull request author unavailable.' }
   if ($body.event -ceq 'APPROVE' -and $pull.user.login -ieq $actor) { throw 'Cannot approve own pull request.' }
   $path="$repo/pulls/$PullNumber/reviews"
  }
 }
 try { $result = Invoke-MagiAuthenticatedRequest -Role $Role -Method $method -Path $path -Body $body -ConfigPath $ConfigPath } catch { throw 'GitHub request failed.' }
 if ($method -eq 'POST') {
  if ($result.user.login -cne $actor -or $result.user.type -cne 'Bot') { throw 'Published author does not match the configured bot; check GitHub before retrying.' }
  return @{ author=$result.user.login; number=$result.number; url=$result.html_url }
 }
 return $result
}
if ($MyInvocation.InvocationName -ne '.') {
 $auth = Join-Path $PSScriptRoot 'github-agent-auth.ps1'
 if (!(Test-Path -LiteralPath $auth)) { throw 'Authenticated transport unavailable.' }
 . $auth
 Invoke-MagiAgent -Role $Role -Operation $Operation -ConfigPath $ConfigPath -InputPath $InputPath -PullNumber $PullNumber
}
