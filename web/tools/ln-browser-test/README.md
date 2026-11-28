# Lightning tab browser tests

Two checks that run the web wallet in headless Chromium against `mock.mjs` (a stand-in for the edge's
`/ln/account`, `/register`, and the `web/` files). They exist because the Lightning tab moves money and the
node tests cannot see the DOM, the re-ask-password gate or the double-click guard.

```sh
node mock.mjs &                                   # http://localhost:8199 (restart it between runs: it keeps a fake clock)
chromium --headless=new --no-sandbox --disable-gpu --virtual-time-budget=60000 \
  --dump-dom http://localhost:8199/__harness.html | grep -A40 'id="results"'   # the tab on its own, 37 checks
node app-e2e.mjs /tmp/ln-e2e                      # the real app: restore a wallet, pay with the password re-ask
```

The mock does not verify the node signature; `crates/fortis-lightning/tests/web_account_identity.rs` does.
Needs `web/pkg` built (see `web/README.md`).
