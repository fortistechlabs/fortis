# Publishing the fortistechlabs.com site

The **Fortis Tech Labs** company site + the **Fortis Wallet** privacy policy that
Google Play requires. Lives in [`site/`](../site). **No build step** — plain HTML,
one SVG, a few small JPEGs.

```
site/
  index.html      →  https://fortistechlabs.com/          (Fortis Tech Labs)
  privacy.html    →  https://fortistechlabs.com/privacy   (Pages serves /privacy → /privacy.html)
  icon.svg        →  https://fortistechlabs.com/icon.svg  (copy of web/icon.svg — keep in sync)
  img/*.jpg       →  downscaled app screenshots
```

Deployed to **Cloudflare Pages** (project `fortis-rest`) — separate origin from
the `api.fortistechlabs.com` tunnel, free, global, auto-TLS.

## Deploy

```powershell
deploy\publish-site.bat              # double-click, or run from any shell
# or:  powershell -File deploy\publish-site.ps1
deploy\publish-site.ps1 -Preview     # throwaway preview build with its own URL
```

Uploads the current contents of `site/` as a production deployment, tagged with
the git commit. First run also creates the `fortis-rest` Pages project (pass
`-Project <name>` if yours is named differently).

### Auth

The script needs a **`CLOUDFLARE_API_TOKEN`** with the *Account → Cloudflare
Pages → Edit* permission:

```powershell
setx CLOUDFLARE_API_TOKEN "<token>"    # then open a NEW shell (or the script
                                       # will also read it from the User env)
```

`npx wrangler login` (browser OAuth) works too, but the token is preferred and is
what the script looks for first.

## One-time: custom domains

After the first deploy, in the dashboard → **Workers & Pages → fortis-rest →
Custom domains** → add `fortistechlabs.com` and `www.fortistechlabs.com`. If a DNS record for
either already exists (e.g. from an earlier Pages project), the dashboard prompts
to repoint it — accept that, or in **DNS → Records** set each to a proxied
`CNAME → fortis-rest.pages.dev`.

**Leave `api.fortistechlabs.com` alone** — that's the `cloudflared` tunnel, not Pages.

## Keeping the icon in sync

```sh
cp web/icon.svg site/icon.svg
```
