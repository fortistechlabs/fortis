<#
  Publish the fortis web wallet  (repo:  web\ )  to Cloudflare Pages.

  ---------------------------------------------------------------------------
  ONE-TIME SETUP
  ---------------------------------------------------------------------------
  1. Same CLOUDFLARE_API_TOKEN as deploy\publish-site.ps1 (Account | Cloudflare
     Pages | Edit) - if you've already deployed the marketing site, you're set.

  2. Build the wasm module at least once (git-ignored, not committed):
        rustup target add wasm32-unknown-unknown
        cargo install wasm-pack
        wasm-pack build crates/wallet-wasm --target web --out-dir ../../web/pkg
     This script refuses to deploy without web\pkg\wallet_wasm_bg.wasm present.

  3. First run creates the Pages project (default name "fortis-wallet").
     Afterwards, in the dashboard:
        Workers & Pages -> fortis-wallet -> Custom domains
        -> add  app.fortistechlabs.com
     (DNS: a proxied CNAME app -> fortis-wallet.pages.dev, or accept the
      dashboard's own prompt to create/repoint it.)

     Already made the project under a different name? Pass  -Project <name>.

  4. Recommended once the wallet has a real home: lock down the edge's CORS
     from the default `--allow-origin *` to just this origin -
     `--allow-origin https://app.fortistechlabs.com` - then restart the edge
     service. See deploy\publish-wallet.md.

  ---------------------------------------------------------------------------
  EVERY TIME
  ---------------------------------------------------------------------------
        .\deploy\publish-wallet.ps1
     Uploads the current contents of web\ (including web\pkg\) as a
     production deployment.
     -Preview  deploys a throwaway preview build instead (its own URL).
#>
[CmdletBinding()]
param(
    [string]$Project = 'fortis-wallet',
    [string]$Branch  = 'main',
    [switch]$Preview
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$repo = Split-Path -Parent $PSScriptRoot
$dir  = Join-Path $repo 'web'

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    throw 'Node.js is required - https://nodejs.org'
}

# --- auth: require CLOUDFLARE_API_TOKEN -----------------------------------------
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
$env:CLOUDFLARE_API_KEY = $null
$env:CLOUDFLARE_EMAIL   = $null
if (-not (Test-Path (Join-Path $dir 'index.html'))) {
    throw "Can't find $dir\index.html - run this from a checkout of the fortis repo."
}

# --- guard: the wasm module must actually be built --------------------------
$wasm = Join-Path $dir 'pkg\wallet_wasm_bg.wasm'
if (-not (Test-Path $wasm)) {
    throw @"
$wasm is missing - web\pkg\ is git-ignored build output, not source.
Build it first:
  rustup target add wasm32-unknown-unknown
  cargo install wasm-pack        # or: npm i -g wasm-pack
  wasm-pack build crates/wallet-wasm --target web --out-dir ../../web/pkg
"@
}

if ($Preview -and $Branch -eq 'main') { $Branch = 'preview' }
$isProd = $Branch -eq 'main'

$sha   = (& git -C $repo rev-parse --short HEAD    2>$null)
$msg   = (& git -C $repo log -1 --pretty=format:%s 2>$null)
$dirty = if (& git -C $repo status --porcelain 2>$null) { 'true' } else { 'false' }

Write-Host ''
Write-Host "  project : $Project"
Write-Host "  folder  : $dir"
Write-Host ("  auth    : CLOUDFLARE_API_TOKEN (...{0})" -f $env:CLOUDFLARE_API_TOKEN.Substring([Math]::Max(0, $env:CLOUDFLARE_API_TOKEN.Length - 4)))
Write-Host ("  target  : {0}" -f $(if ($isProd) { 'production (app.fortistechlabs.com, once the custom domain is added)' } else { "preview  (branch '$Branch')" }))
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

Push-Location $repo
try {
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
    Write-Host '  Once the custom domain is added (one-time, see the top of this'
    Write-Host '  script), live at https://app.fortistechlabs.com/'
    Write-Host '  (the *.pages.dev URL above works immediately either way).'
} else {
    Write-Host '  Preview build - see the *.pages.dev URL above.'
}
