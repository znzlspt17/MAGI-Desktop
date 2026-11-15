$ErrorActionPreference = 'Stop'
$scriptPath = Join-Path $PSScriptRoot 'github-agent.ps1'
if (!(Test-Path -LiteralPath $scriptPath)) { throw 'FAIL: agent routing script is missing' }
. $scriptPath
$config = Join-Path ([IO.Path]::GetTempPath()) ('magi-test-' + [guid]::NewGuid() + '.json')
$inputFile = Join-Path ([IO.Path]::GetTempPath()) ('magi-test-' + [guid]::NewGuid() + '.json')
try {
 '{"repository":"znzlspt17/MAGI-Desktop","roles":{"devops":{"app_slug":"magi-devops","app_id":123,"installation_id":456},"intern":{"app_slug":"magi-intern","app_id":123,"installation_id":456}}}' | Set-Content -LiteralPath $config -Encoding utf8
 '{"title":"Title","head":"codex/devops/task","base":"dev","body":"Evidence"}' | Set-Content -LiteralPath $inputFile -Encoding utf8
 $script:calls = @(); $script:author = 'magi-devops[bot]'; $script:prAuthor = 'other[bot]'
 function Invoke-MagiAuthenticatedRequest { param($Role,$Method,$Path,$Body,$ConfigPath)
  $script:calls += "$Method $Path"
  if ($script:failTransport) { throw 'FAKE_SENSITIVE_MARKER' }
  if ($Path -eq '/app') { return @{ slug = "magi-$Role"; id = 123 } }
  if ($Path -match '/pulls/\d+$') { return @{ user = @{ login = $script:prAuthor } } }
  if ($Method -eq 'POST') { return @{ number = 1; user = @{ login = $script:author; type = 'Bot' }; html_url = 'https://github.com/znzlspt17/MAGI-Desktop/pull/1' } }
  return @(@{ number=1; user=@{ login='other[bot]' } })
 }
 function Assert-Fails([scriptblock]$action) { $failed=$false; try { & $action | Out-Null } catch { $failed=$true }; if (!$failed) { throw 'FAIL: expected rejection' } }
 $r = Invoke-MagiAgent -Role devops -Operation CreatePullRequest -ConfigPath $config -InputPath $inputFile
 if ($r.author -ne 'magi-devops[bot]' -or $script:calls[-1] -ne 'POST /repos/znzlspt17/MAGI-Desktop/pulls') { throw 'FAIL: create routing/author' }
 '{"title":"Title","head":"codex/devops/task","base":"main"}' | Set-Content -LiteralPath $inputFile -Encoding utf8
 $before=$script:calls.Count
 Assert-Fails { Invoke-MagiAgent -Role devops -Operation CreatePullRequest -ConfigPath $config -InputPath $inputFile }
 if (@($script:calls | Select-Object -Skip $before | Where-Object { $_ -like 'POST *' }).Count) { throw 'FAIL: invalid base posted' }
 '{"title":"Title","head":"codex/dev/task","base":"dev"}' | Set-Content -LiteralPath $inputFile -Encoding utf8
 $before=$script:calls.Count
 Assert-Fails { Invoke-MagiAgent -Role devops -Operation CreatePullRequest -ConfigPath $config -InputPath $inputFile }
 if (@($script:calls | Select-Object -Skip $before | Where-Object { $_ -like 'POST *' }).Count) { throw 'FAIL: invalid role branch posted' }
 '{"title":"Title","head":"codex/devops/","base":"dev"}' | Set-Content -LiteralPath $inputFile -Encoding utf8
 Assert-Fails { Invoke-MagiAgent -Role devops -Operation CreatePullRequest -ConfigPath $config -InputPath $inputFile }
 '{"title":"Title","head":"codex/devops/task","base":"dev","body":"Evidence"}' | Set-Content -LiteralPath $inputFile -Encoding utf8
 Assert-Fails { Invoke-MagiAgent -Role invalid -Operation GetPullRequests -ConfigPath $config }
 Assert-Fails { Invoke-MagiAgent -Role intern -Operation CreatePullRequest -ConfigPath $config -InputPath $inputFile }
 $who = Invoke-MagiAgent -Role devops -Operation WhoAmI -ConfigPath $config
 if ($who.actor -ne 'magi-devops[bot]') { throw 'FAIL: identity' }
 $null = Invoke-MagiAgent -Role devops -Operation GetRepository -ConfigPath $config
 $null = Invoke-MagiAgent -Role devops -Operation GetPullRequests -ConfigPath $config
 $script:failTransport=$true
 try { Invoke-MagiAgent -Role devops -Operation GetRepository -ConfigPath $config; throw 'FAIL: expected transport failure' } catch { if ($_.Exception.Message -match 'FAKE_SENSITIVE_MARKER') { throw 'FAIL: unsafe error' } }
 $script:failTransport=$false
 $script:author='wrong[bot]'; Assert-Fails { Invoke-MagiAgent -Role devops -Operation CreatePullRequest -ConfigPath $config -InputPath $inputFile }; $script:author='magi-devops[bot]'
 '{"event":"APPROVE","body":"Evidence"}' | Set-Content -LiteralPath $inputFile -Encoding utf8
 $script:prAuthor='magi-devops[bot]'; $before=$script:calls.Count
 Assert-Fails { Invoke-MagiAgent -Role devops -Operation CreateReview -PullNumber 1 -ConfigPath $config -InputPath $inputFile }
 if ($script:calls.Count -ne ($before+2)) { throw 'FAIL: own approval posted' }
 $script:prAuthor='other[bot]'
 $review = Invoke-MagiAgent -Role devops -Operation CreateReview -PullNumber 1 -ConfigPath $config -InputPath $inputFile
 if ($review.author -ne 'magi-devops[bot]' -or $script:calls[-1] -ne 'POST /repos/znzlspt17/MAGI-Desktop/pulls/1/reviews') { throw 'FAIL: review routing' }
 '{"repository":"elsewhere/project","roles":{"devops":{"app_slug":"magi-devops","app_id":123,"installation_id":456}}}' | Set-Content -LiteralPath $config -Encoding utf8
 Assert-Fails { Invoke-MagiAgent -Role devops -Operation GetPullRequests -ConfigPath $config }
 Write-Output 'PASS routing, roles, repository, writes, authors and own approval'
} finally { Remove-Item -LiteralPath $config,$inputFile -Force -ErrorAction SilentlyContinue }
