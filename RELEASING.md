# Releasing Fortis Wallet

Google Play needs an **organization** account for crypto wallets (policy, Aug 2024)
and this is a personal account, so distribution is:

- **GitHub Releases** — the source of truth. APK attached to a tagged release.
- **fortistechlabs.com** — "Download APK" button → the release's `fortis-wallet.apk`,
  plus an "Add to Obtainium" deep link.
- **[Obtainium](https://obtainium.imranr.dev/)** — users paste
  `github.com/fortistechlabs/fortis`; it polls the releases and auto-updates.

The store-listing groundwork (`store/STORE.md`, `store/RELEASE.md`, the Play
Console declarations) is kept for whenever an org account exists.

---

## Cut a release

### 1. Bump the version

`android/app/build.gradle.kts` — `versionCode` **must increase every release**;
bump `versionName` too. Add a `CHANGELOG.md` entry.

### 2. Build the signed APK

```powershell
$env:JAVA_HOME = 'C:\Program Files\Android\Android Studio\jbr'
cd C:\Repos\fortis\android
.\gradlew.bat :app:assembleRelease
Copy-Item app\build\outputs\apk\release\app-release.apk `
  ..\fortis-wallet.apk           # stable asset name — the site links to /latest/download/fortis-wallet.apk
```

Universal APK (arm64-v8a + x86_64), signed with `fortis-upload.jks`. **That key
is the app's permanent identity for direct installs** — if Play ever happens,
hand Play *this* key as the app signing key so website users can cross-update.

### 3. Record the hashes

```powershell
(Get-FileHash ..\fortis-wallet.apk -Algorithm SHA256).Hash.ToLower()
$apksigner = (Get-ChildItem "$env:LOCALAPPDATA\Android\Sdk\build-tools\*\apksigner.bat" | Sort-Object Name)[-1]
& $apksigner verify --print-certs ..\fortis-wallet.apk    # needs $env:JAVA_HOME set
```

Update `site/version.json` (`versionCode`, `versionName`, `sha256`) and, if the
signing cert ever changes, the fingerprint in `site/index.html`.

### 4. Tag and publish

```powershell
cd C:\Repos\fortis          # not android\ — the APK and CHANGELOG.md are at the repo root
gh release create v0.1.1 .\fortis-wallet.apk `
  --title "Fortis Wallet 0.1.1" `
  --notes-file CHANGELOG.md
```

`gh release create` makes the `v0.1.1` tag at the current commit (make sure
`main` is pushed first), uploads the APK, and publishes in one step — no
separate `git tag` / `git push` needed. The repo must be **public** or the
`releases/latest/download/` link needs auth.

…(needs `gh` — `winget install GitHub.cli`) or on github.com: **Releases → Draft
a new release**, choose tag `v0.1.1`, paste the changelog, attach
`fortis-wallet.apk`, **Publish**.

- Tag format `v<versionName>` — Obtainium strips the `v` and compares to the
  installed `versionName`.
- Keep the asset named **`fortis-wallet.apk`** (no version in the filename) so
  `releases/latest/download/fortis-wallet.apk` always resolves.

### 5. Redeploy the site

```powershell
deploy\publish-site.ps1
```

Pushes the updated `version.json` and any listing/screenshot changes.

---

## Obtainium notes

- Source URL: `https://github.com/fortistechlabs/fortis`
- The one-tap link on the site is an HTTPS redirect wrapper around the
  `obtainium://add/<url-encoded source URL>` deep link (the payload is a plain
  URL-encoded source URL — **not** JSON; passing JSON throws "Invalid URL"):
  `https://apps.obtainium.imranr.dev/redirect?r=obtainium%3A%2F%2Fadd%2Fhttps%253A%252F%252Fgithub.com%252Ffortistechlabs%252Ffortis`
  The redirect wrapper degrades gracefully on desktop; the bare `obtainium://`
  scheme only resolves on Android with Obtainium installed.
- Obtainium needs the repo **public** and each release to carry exactly one
  `.apk` asset (or an `apkFilterRegEx`).
