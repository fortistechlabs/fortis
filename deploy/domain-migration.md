# Moving to fortistechlabs.com

One-time migration from `fortis.rest` → `fortistechlabs.com`. The repo side is
done (commit `a76b4f0`); this is the Cloudflare + Namecheap + release checklist.

**Plan:** site → `fortistechlabs.com` / `www`, API → `api.fortistechlabs.com`.
`fortis.rest` gets a 301 and is kept alive on the tunnel until the v0.1.2 app is
adopted, then it's allowed to lapse at renewal.

The tunnel config on the host (`C:\cloudflared\config.yml`) is **already**
updated to serve both `api.fortistechlabs.com` and `api.fortis.rest`.

---

## 1. Add the zone

1. **Cloudflare** → *Add a site* → `fortistechlabs.com` → Free plan. It scans the
   existing records and shows two nameservers (`x.ns.cloudflare.com`).
2. **Namecheap** → Domain List → `fortistechlabs.com` → *Manage* → *Nameservers*
   → **Custom DNS** → paste the two Cloudflare nameservers → save (the green
   check). Propagation is usually minutes.
3. Wait for the Cloudflare zone to read **Active**.
4. In the new zone's **DNS → Records**, delete any parking A/AAAA records
   Cloudflare imported from Namecheap (the "domain for sale" page). Leave it empty.

## 2. Point the site

5. **Workers & Pages → `fortis-rest` → Custom domains → Set up a custom domain**
   → `fortistechlabs.com`. Accept the record it creates. Repeat for
   `www.fortistechlabs.com`.
6. Wait for both to go **Active** (cert issuance, a few minutes).
7. Check: `https://fortistechlabs.com/` and `https://fortistechlabs.com/privacy`
   serve the site.

## 3. Point the API

8. In the **new zone → DNS → Records**, add:
   - Type `CNAME`, Name `api`, Target `<TUNNEL_ID>.cfargotunnel.com`, **Proxied**
     (orange). Get `<TUNNEL_ID>` from `cloudflared tunnel list` (or the tunnel's
     line in `C:\cloudflared\config.yml`).

   *(CLI alternative, from the host:
   `& "C:\Program Files (x86)\cloudflared\cloudflared.exe" tunnel route dns fortis api.fortistechlabs.com`
   — if it errors about the zone, re-run `cloudflared tunnel login` and pick
   `fortistechlabs.com`, or just use the manual record above.)*
9. On the host, pick up the config change:
   ```powershell
   Restart-Service fortis-tunnel
   ```
10. Check: `curl https://api.fortistechlabs.com/` returns the same health JSON as
    `curl https://api.fortis.rest/`.

## 4. Email routing

11. **New zone → Email → Email Routing → Get started.** It adds the MX + SPF
    records itself.
12. *Destination addresses* → add your personal inbox → click the link in the
    verification email Cloudflare sends.
13. *Routing rules* → add:
    - `info@fortistechlabs.com` → your inbox
    - `support@fortistechlabs.com` → your inbox
14. Check: email `info@fortistechlabs.com`, confirm it lands in Gmail. (Sending
    *as* that address from Gmail is optional and separate — "Send mail as" +
    an SMTP relay.)

## 5. Redirect fortis.rest

15. In the **`fortis.rest` zone → Rules → Redirect Rules → Create rule**:
    - If: `Hostname equals fortis.rest` **or** `Hostname equals www.fortis.rest`
    - Then: *Dynamic* redirect, expression
      `concat("https://fortistechlabs.com", http.request.uri.path)`, status
      **301**, *Preserve query string* on.
16. Leave the **`api.fortis.rest`** DNS record alone — the redirect rule doesn't
    match it, and old app installs still need it.
17. Once the redirect works, remove `fortis.rest` + `www.fortis.rest` from the
    `fortis-rest` Pages project's custom domains (tidy; not required).
18. Check: `https://fortis.rest/privacy` → 301 → `https://fortistechlabs.com/privacy`.

## 6. Ship v0.1.2

Only after `api.fortistechlabs.com` is confirmed serving. Full steps in
[`../RELEASING.md`](../RELEASING.md); short form:

```powershell
$env:JAVA_HOME = 'C:\Program Files\Android\Android Studio\jbr'
cd C:\Repos\fortis\android
.\gradlew.bat :app:assembleRelease
Copy-Item app\build\outputs\apk\release\app-release.apk ..\fortis-wallet.apk
(Get-FileHash ..\fortis-wallet.apk -Algorithm SHA256).Hash.ToLower()   # -> site/version.json
cd C:\Repos\fortis
gh release create v0.1.2 .\fortis-wallet.apk --title "Fortis Wallet 0.1.2" --notes-file CHANGELOG.md
deploy\publish-site.ps1
```

Update `site/version.json`: `versionCode` 3, `versionName` 0.1.2, new `sha256`.
The signing cert is unchanged, so `certSha256` and the fingerprint in
`site/index.html` stay.

## 7. Later — retire fortis.rest

Before the Namecheap renewal date, once v0.1.2 has taken over:
- let `fortis.rest` lapse (the redirect rules and the `api.fortis.rest` DNS
  record go with it);
- remove the `api.fortis.rest` block from `C:\cloudflared\config.yml` and
  `Restart-Service fortis-tunnel`;
- drop `deploy/cloudflared/config.example.yml`'s second hostname if it was added.

---

## Rollback

Nothing here is destructive until step 7. If the new domain misbehaves, the
`fortis.rest` zone, Pages custom domains, and tunnel hostname are all still live
and unchanged — just don't cut v0.1.2, and pause the redirect rule.
