# Android: use your own Esplora server — design

**Status:** awaiting review · 2026-10-22
**Scope:** Android app only. Web wallet, `fortisd`, Electrum and Tor are out of scope.

## Why

Users who run their own node want the wallet to read the chain from it, so
their addresses, balances and history never reach fortis servers or a public
explorer. Most node setups (Umbrel, Start9, RaspiBlitz, a plain `mempool`
container) already expose an Esplora/mempool REST API, and the app already
speaks Esplora — its public-explorer fallback is an `EsploraBackend` with no
token. So this is a setting plus wiring, not a new backend.

## Decisions

| question | decision |
|---|---|
| What "own node" means | **An Esplora/mempool REST URL.** Not `fortisd`, not Electrum. |
| Scope of the setting | **App-wide, one per chain** (BTC, XBT). Every mainnet wallet on that chain uses it. |
| Switching | **Per-chain choice: *Fortis servers* / *Your server*.** The saved URL is kept when switching back to fortis; *Remove* deletes it. |
| Server unreachable | **Fail visibly, no fallback.** "Can't reach your server (host)" + *Retry*; last known balance stays. Nothing goes to the edge or the public explorer. |
| Transport | **HTTPS only.** `network_security_config.xml` is unchanged. |
| Onion / Tor | **Not now.** (Later option: route `.onion` URLs via Orbot's SOCKS proxy; allow cleartext for the `onion` domain only.) |
| Service fee | **None on ordinary sends** through an own server (no pricing URL, as `Backend.kt` already documents for self-hosted backends). Swap commission is unchanged. |
| Networks | **Mainnet only.** The setting is ignored for other networks. |

## Settings and storage

`Store` gains four app-wide preferences:

- `own_server_btc`, `own_server_xbt` — the saved, normalized URL, or absent.
- `use_own_server_btc`, `use_own_server_xbt` — boolean, default off.

API: `ownServer(chain): String?` (URL only when saved **and** on), plus
`setOwnServer(chain, url: String?)` and `setUseOwnServer(chain, on)`.
`setOwnServer(chain, null)` (*Remove*) clears both keys for that chain.

The Settings tab gets a **"Your server"** `GlassCard` with one row per chain:

- A two-option choice: **Fortis servers** / **Your server (host)**.
- Choosing *Your server* with no saved URL opens the **Test & save** dialog
  (paste URL → validate → save and switch on).
- Choosing *Your server* with a saved URL re-runs the quick check (tip height
  + fork-block hash) before switching; on failure it stays on fortis and shows
  the error.
- *Change* (opens the dialog prefilled) and *Remove* beside a saved URL.

## Validation

`data/OwnServer.kt`:

```kotlin
sealed interface OwnServerCheck {
    data class Ok(val url: String) : OwnServerCheck   // normalized, ends in the API root
    object NotHttps : OwnServerCheck
    object Unreachable : OwnServerCheck
    object NotEsplora : OwnServerCheck
    object WrongChain : OwnServerCheck
    data class NotSynced(val tip: Long) : OwnServerCheck
}
suspend fun validateOwnServer(http: OkHttpClient, chain: String, url: String): OwnServerCheck
```

Steps, stopping at the first failure:

1. Trim, drop trailing `/`. Scheme must be `https` → else `NotHttps`, no request made.
2. Candidates: the URL as given, then `{url}/api` if it doesn't already end in `/api`.
   For each: `GET {c}/blocks/tip/height` must return a bare integer. First
   candidate that does is the base. None → `NotEsplora` if any answered,
   `Unreachable` if none connected.
3. Tip < `FORK_CHECK_HEIGHT` → `NotSynced(tip)`.
4. `GET {base}/block-height/961640` must equal the chain's constant → else `WrongChain`.

Constants (read from the local Core and Knots nodes, 2026-10-22):

```kotlin
const val FORK_CHECK_HEIGHT = 961_640L
val FORK_CHECK_HASH = mapOf(
    "btc" to "00000000000000000001d82da6ecccf08e07afa383f9212b0e1b95cc72430c00",
    "xbt" to "0000000000000050c1e5f69672f459293be14f46e5a494e7a8c8541396f18eeb",
)
```

Error strings: "This server must use HTTPS." · "Can't reach this server." ·
"Not an Esplora server." · "This server is on the other chain." ·
"This server isn't synced yet (block N)."

## Backend selection

A pure function, unit-testable without a ViewModel:

```kotlin
data class BackendPlan(val primary: Target, val fallback: Target?)
sealed interface Target {
    object Edge : Target                    // HOSTED_EDGE/{chain}, token, pricing, batch scan
    data class Esplora(val url: String) : Target   // no token, no pricing, no batch scan
}
fun chooseBackend(chain: String, network: String, ownServer: String?): BackendPlan
```

- `network == "mainnet"` and `ownServer != null` → `BackendPlan(Esplora(ownServer), fallback = null)`.
- Otherwise → today's `BackendPlan(Edge, Esplora(PUBLIC_*_ESPLORA))`.

`WalletViewModel` uses it in:

- **`ensureBackend()`** — builds `backend` / `fallback` from the plan. With an
  own server, `fallback` stays `null`.
- **`overviewBackend()`** — balance scan, swap funder, swap fee reserve follow
  the same plan (cached per wallet id as today).
- **`reconnect()`** — with an own server, rebuilds that backend and skips
  `edgeRegister`.
- **On any change** to a chain's server or switch: drop `backend`, `fallback`
  and that chain's `overviewBackends` entries, then `refresh()`.

The status line shows the server's host (the `EsploraBackend` label) instead
of "fortis". With an own server, a refresh error surfaces as
"Can't reach your server (host)" with *Retry*.

## Swaps

Swaps keep talking to `HOSTED_EDGE` (coordinator, chain watch, token) — no
change there. One change: **`swapFunder` fetches the commission address
directly from `$HOSTED_EDGE/pricing`** instead of `b.status().pricing`, since
an own-server backend has no pricing URL. That request carries no wallet data.
Funding coins, feerate and broadcast go through the wallet's backend (the own
server when set).

## What still reaches fortis with an own server on

Crash reports, `/pricing` (swap funding only) and all swap traffic.
Wallet addresses, balances, history and ordinary broadcasts do not. Prices and
fee rates come from the own server (mempool's `/v1/prices` and
`/v1/fees/recommended`; a plain electrs server has no prices, and fee rates
fall back to its `/fee-estimates`).

## Files

- New: `data/OwnServer.kt` (validation + constants + `chooseBackend`).
- `data/Store.kt` — the four preferences.
- `WalletViewModel.kt` — selection, cache invalidation, `swapFunder` pricing.
- `ui/screens/Screens.kt` — "Your server" card and Test & save dialog.
- `res/values*/strings.xml` — new strings (untranslated locales fall back to English).

## Testing

JUnit, fake `OkHttpClient` interceptor serving Esplora routes (the
`ExplorerChainCheckTest` pattern), in `test/.../data/OwnServerTest.kt`:

- Valid BTC server → `Ok`; valid XBT server → `Ok`.
- `http://` → `NotHttps`, and no request is made.
- Other chain's hash at 961 640 → `WrongChain`; tip below 961 640 → `NotSynced`.
- HTML / non-numeric tip → `NotEsplora`; connection failure → `Unreachable`.
- Bare host passes only via `/api`, and `Ok.url` ends in `/api`.
- `chooseBackend`: own server + mainnet → Esplora, no fallback; own server +
  non-mainnet → edge + public fallback; no own server → edge + public fallback.

Store behaviour (URL kept on switch-off; *Remove* clears both keys) is checked
on device, since `Store` wraps DataStore and has no unit tests today.

**Manual on device:** set BTC to a real public Esplora (e.g.
`https://mempool.emzy.de/api`) → balance and history load, status shows its
host; switch to fortis → edge again, URL still shown; switch back → re-checked,
works; enter an XBT server in the BTC row → rejected as wrong chain; break the
URL's host while on → visible error, no traffic to the edge (check with the
edge's request counter).
