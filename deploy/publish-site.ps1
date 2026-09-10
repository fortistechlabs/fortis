<#
  Publish the fortistechlabs.com static site  (repo:  site\ )  to Cloudflare Pages.

  ---------------------------------------------------------------------------
  ONE-TIME SETUP
  ---------------------------------------------------------------------------
  1. Create an API token: Cloudflare dashboard -> My Profile -> API Tokens ->
     Create Token -> Custom token, permission  Account | Cloudflare Pages | Edit.
     Then:
        setx CLOUDFLARE_API_TOKEN "<token>"
     (open a new shell afterwards - or don't; this script also reads it straight
      from the User/Machine environment so a stale shell still works.)

  2. First run creates the Pages project (default name "fortis-rest").
     Afterwards, in the dashboard:
        Workers & Pages -> fortis-rest -> Custom domains
        -> add  fortistechlabs.com  and  www.fortistechlabs.com
     (Leave api.fortistechlabs.com alone - that's the cloudflared tunnel.)

     Already made the project under a different name? Pass  -Project <name>.

  ---------------------------------------------------------------------------
  EVERY TIME
  ---------------------------------------------------------------------------
        .\deploy\publish-site.ps1
     Uploads the current contents of site\ as a production deployment.
     -Preview  deploys a throwaway preview build instead (its own URL).
#>
[CmdletBinding()]
param(
    [string]$Project = 'fortis-rest',
    [string]$Branch  = 'main',
    [switch]$Preview
)

$ErrorActionPreference = 'Stop'
# npx / git non-zero exits are checked explicitly below via $LASTEXITCODE - don't
# let PowerShell 7 turn a benign one (e.g. "project already exists") into a fatal.
$PSNativeCommandUseErrorActionPreference = $false
$repo = Split-Path -Parent $PSScriptRoot
$dir  = Join-Path $repo 'site'

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    throw 'Node.js is required - https://nodejs.org'
}

# --- auth: require CLOUDFLARE_API_TOKEN -----------------------------------------
# Prefer the current process env; fall back to the persisted User / Machine env
# so a shell opened before `setx` still works. Fail loud rather than silently
# dropping to a wrangler OAuth login.
if (-not $env:CLOUDFLARE_API_TOKEN) {
    foreach ($scope in 'User', 'Machine') {
        $t = [Environment]::GetEnvironmentVariable('CLOUDFLARE_API_TOKEN', $scope)
        if ($t) { $env:CLOUDFLARE_API_TOKEN = $t; break }
    }
}
if (-not $env:CLOUDFLARE_API_TOKEN) {
    throw @'
CLOUDFLARE_API_TOKEN is not set.
  Cloudflare dashboard -> My Profile -> API Tokens -> Create Token
  -> Custom token, permission: Account | Cloudflare Pages | Edit
  then:  setx CLOUDFLARE_API_TOKEN "<token>"
'@
}
$env:CLOUDFLARE_API_KEY   = $null   # make sure a stale global-key auth can't win
$env:CLOUDFLARE_EMAIL     = $null
if (-not (Test-Path (Join-Path $dir 'index.html'))) {
    throw "Can't find $dir\index.html - run this from a checkout of the fortis repo."
}

# --- guard: unfilled placeholders in the site HTML -------------------------
$holes = Select-String -Path (Join-Path $dir '*.html') `
    -Pattern '\[(DATE|registered address|operating entity)\]' -ErrorAction SilentlyContinue
if ($holes) {
    Write-Warning 'site HTML still has unfilled placeholders:'
    $holes | ForEach-Object { Write-Host ("  {0}:{1}  {2}" -f $_.Filename, $_.LineNumber, $_.Line.Trim()) }
    if ((Read-Host 'Deploy anyway? (y/N)') -ne 'y') { return }
}

if ($Preview -and $Branch -eq 'main') { $Branch = 'preview' }
$isProd = $Branch -eq 'main'

# --- commit metadata -> shown on the deployment in the dashboard ----------
$sha   = (& git -C $repo rev-parse --short HEAD    2>$null)
$msg   = (& git -C $repo log -1 --pretty=format:%s 2>$null)
$dirty = if (& git -C $repo status --porcelain 2>$null) { 'true' } else { 'false' }

Write-Host ''
Write-Host "  project : $Project"
Write-Host "  folder  : $dir"
Write-Host ("  auth    : CLOUDFLARE_API_TOKEN (...{0})" -f $env:CLOUDFLARE_API_TOKEN.Substring([Math]::Max(0, $env:CLOUDFLARE_API_TOKEN.Length - 4)))
Write-Host ("  target  : {0}" -f $(if ($isProd) { 'production (fortistechlabs.com)' } else { "preview  (branch '$Branch')" }))
Write-Host ("  commit  : {0} {1}{2}" -f $sha, $msg, $(if ($dirty -eq 'true') { '   [+ uncommitted changes]' }))
Write-Host ''

$wr = @('--yes', 'wrangler@4')

$deploy = @(
    'pages', 'deploy', $dir,
    '--project-name', $Project,
    '--branch',       $Branch,
    '--commit-dirty', $dirty
)
if ($sha) { $deploy += @('--commit-hash',    $sha) }
if ($msg) { $deploy += @('--commit-message', $msg) }

# Run from the repo root so wrangler's .wrangler\ working dir always lands in one
# predictable, git-ignored place regardless of where this script was invoked from.
Push-Location $repo
try {
    # Create the Pages project only if it doesn't exist yet. (Calling
    # `pages project create` on an existing project errors out.)
    $projects = (& npx @wr pages project list 2>$null | Out-String)
    if ($LASTEXITCODE -eq 0 -and $projects -notmatch [regex]::Escape($Project)) {
        Write-Host "  creating Pages project '$Project' ..."
        & npx @wr pages project create $Project --production-branch main | Out-Null
    }

    & npx @wr @deploy
    $rc = $LASTEXITCODE
}
finally { Pop-Location }

if ($rc -ne 0) {
    Write-Host ''
    Write-Warning "Deploy failed (exit $rc)."
    Write-Host '  - auth?     the token needs  Account | Cloudflare Pages | Edit'
    Write-Host '              check it:  npx wrangler whoami'
    Write-Host '  - project?  list yours:  npx wrangler pages project list'
    Write-Host '              then re-run with  -Project <name>'
    exit $rc
}

Write-Host ''
Write-Host '  Deployed.' -ForegroundColor Green
if ($isProd) {
    Write-Host '  Live at https://fortistechlabs.com/  and  https://www.fortistechlabs.com/'
    Write-Host '  (edge cache may hold the old page for a minute; the immutable'
    Write-Host '   *.fortis-rest.pages.dev URL above is instant).'
} else {
    Write-Host '  Preview build - see the *.pages.dev URL above.'
}
