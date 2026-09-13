# Publishing the fortis web wallet

The browser/PWA wallet (`web/`) — signing happens entirely in wasm, the edge
sees only chain lookups. Same Cloudflare Pages mechanism as the marketing
site ([`publish-site.md`](publish-site.md)), separate project so the two
deploy independently.

```
web/  →  https://app.fortistechlabs.com/
```

## Before the first deploy

1. **Build the wasm module** — `web/pkg/` is git-ignored build output, not
   checked in:
   ```sh
   rustup target add wasm32-unknown-unknown
   cargo install wasm-pack        # or: npm i -g wasm-pack
   # Windows: winget install LLVM.LLVM   (set CC=clang / AR=llvm-ar if cc can't find them)
   wasm-pack build crates/wallet-wasm --target web --out-dir ../../web/pkg
   ```
   `deploy/publish-wallet.ps1` refuses to run without it.

2. **Same `CLOUDFLARE_API_TOKEN`** as the site deploy (`Account │ Cloudflare
   Pages │ Edit`). If you've already deployed `site/`, this is already set —
   nothing new to do.

## Deploy

```powershell
deploy\publish-wallet.bat              # double-click, or run from any shell
# or:  powershell -File deploy\publish-wallet.ps1
deploy\publish-wallet.ps1 -Preview     # throwaway preview build with its own URL
```

Uploads the current contents of `web/` (including `web/pkg/`) as a production
deployment. First run also creates the `fortis-wallet` Pages project.

## One-time: custom domain

After the first deploy, in the dashboard → **Workers & Pages → fortis-wallet
→ Custom domains** → add `app.fortistechlabs.com`. If a DNS record for it
already exists, accept the prompt to repoint it — otherwise, in **DNS →
Records**, add a proxied `CNAME app → fortis-wallet.pages.dev`.

## Recommended once this is live: lock down the edge's CORS

`fortis-edge`'s `--allow-origin` defaults to `*` (any origin) — fine while the
wallet had no fixed home, but now that it does, restrict it:

```
--allow-origin https://app.fortistechlabs.com
```

Edit that argument wherever the edge service's launch args live (however
you're running it in production — see [`README.md`](README.md#3-make-it-reachable)
for the tunnel/systemd/nssm options), then restart the service. This is a
hardening step, not a functional requirement — registration and requests work
today regardless, since the default is wide open.

## Keeping the two sites in sync

`site/index.html` doesn't currently link to the wallet — it only advertises
the Android APK / Zapstore. Adding a "Launch web wallet" link there is a
separate, deliberate decision (public marketing copy) — ask before doing it,
don't bundle it into a wallet deploy.
