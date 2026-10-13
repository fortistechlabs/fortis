# Changelog

Fortis Wallet (Android). Versions are `versionName (versionCode)`.

## Unreleased

- A swap the other side never locks its coins into no longer ties yours up for
  two days: once the match is cancelled, their app signs your coins straight
  back and they return within minutes — automatically, on both sides. (New
  swaps only; needs the matching server update. If their phone is gone for
  good, the refund at the 48-hour mark still happens on its own.)
- Clearer cancelled swaps: you see when your coins come back, or that nothing
  left your wallet; a running swap's status pulses green.
- Swaps survive losing the phone: restore your recovery phrase (with both its
  XBT and BTC wallets) and the app finds the swaps that still have your coins
  locked, then refunds or completes them — also from Settings → Find my swaps.
  Needs the matching server update.
- Swap BTC ⇄ XBT with other fortis users — atomic, non-custodial, 0.5% per side;
  available once enabled on the server. Post an offer or take a private one by
  code; each side funds its own escrow and either side can always get its coins
  back after the timelock. Swaps keep going in the background (and after a
  reboot), with notifications as they progress. Offers carry your own limit
  price; NeoxEX is shown as a reference. Commission is 0.5% per side with a
  400 sat minimum; swaps start at 2 000 sat.
- Pick a swap from the list: the Swaps page shows the offers waiting for a
  match, says for each whether you have enough to take it (all of it, or up to
  how much), and lets you take all or part of one at its price.
- Safer when you put the phone down: the app locks itself after a minute in
  the background, asks for your fingerprint or password again before showing a
  recovery phrase or sending, and keeps every screen out of screenshots and the
  recent-apps preview. Swaps carry on while it's locked.
- Swaps are tougher: a swap whose coins were locked in time is no longer lost
  because the server didn't hear about it in time (it carries on, and nobody is
  penalised); a stuck step is abandoned and retried instead of blocking the
  swap; a cancelled match explains itself on screen; and the swap screen tells
  you to keep your phone on and connected until it's done.
- Every swap transaction is replay-protected: BTC fundings, claims and refunds
  carry an OP_RETURN the XBT chain can't accept, and XBT ones are signed with
  `SIGHASH_UNIFIED`, which Bitcoin rejects.
- Swaps no longer rely on the fortis server alone for what is on chain: before
  you fund or reveal the swap secret, the app checks the other side's coins with
  an independent block explorer, and sends its claim through one as well.
- The app and the web wallet are now fully translated into German, French,
  Spanish, Portuguese (Brazil and Portugal) and Chinese (Simplified and
  Traditional), swaps included.
- A wallet someone floods with tiny "dust" payments still shows its whole
  balance (it used to fail to load).
- Swaps lock XBT for at most 2 days, not 6: if the other side never funds,
  your XBT comes back after 48 hours.
- Safer swaps and sends:
  - a swap only counts as refunded or finished once an independent explorer
    confirms it;
  - a refund waiting to confirm follows the live network fee;
  - the app refuses a service fee above 1% (or a minimum above 2,000 sat), and
    swap terms for any network other than Bitcoin mainnet.
- Your data stays on your phone: Android's device-to-device transfer no longer
  copies it, copied addresses and keys are marked sensitive, and release builds
  write nothing about your swaps to the system log.
- Backend hardening: the swap server checks on chain that a side's escrow
  really holds the agreed coins before it moves a swap on, relays each message
  once and only in its checked form, limits each install to 10 open offers,
  and keeps its crash log and caches to a fixed size on disk.
- New look: a darker graphite design with much higher-contrast text, a
  terminal-style typeface throughout, Matrix-green highlights, real tabs, and
  buttons — including each wallet on the home screen — that look pressable and
  press down when tapped. Larger amounts and dollar values, and a new app icon.
- Connect a hardware wallet: scan the device's Bitcoin account key as a QR code
  (plain or animated — Keystone, Passport, SeedSigner, Jade and others) or
  paste it, and the app adds it as a view-only BTC wallet. It then offers to
  open the same addresses on XBT. Only Native SegWit (BIP-84) accounts are
  accepted. Your device can't sign XBT transactions, so the XBT wallet is
  labelled view-only.
- Backend build: minimum supported Rust version raised from 1.82 to 1.88
  (required by rocksdb 0.25).
- Backend: the address index (`fortis-index`) is rewritten. A full sync from
  SegWit activation now takes hours instead of ~11 days, the API serves many
  wallets at once, and the index survives crashes, reorgs and node restarts
  without manual repair. It also serves BTC over a local Bitcoin Core node.
  Existing indexes must be rebuilt into a new directory (the old format is
  refused).
- Backend: pending transactions now report their fee when every input is a
  known P2WPKH output (previously 0 until confirmed).

## 0.4.3 (17) — 2026-09-28

- Fixed a BTC watch-only wallet briefly showing 0 BTC before showing its
  real balance, whenever the backend's batch data source had a momentary
  hiccup. It now keeps showing the last known balance through a hiccup like
  that instead of flashing to zero.
- Fixed the "view on block explorer" link for BTC transactions, which had
  stopped working (the explorer it pointed at became unreachable from some
  networks).

## 0.4.2 (16) — 2026-09-28

- Fixed a case where a BTC wallet could get stuck showing "Connecting…"
  indefinitely, or briefly show an incorrect balance, when the backend's
  batch data source was temporarily unavailable. It now shows a safe,
  clearly-labeled balance in that situation instead of a wrong or stuck one.
- Reopening a wallet now shows its last-known balance immediately instead of
  a blank screen while it checks for updates.
- Tapping "Reconnect" (Settings) now visibly does something — it shows
  "Refreshing…" and either updates or reports that it couldn't, instead of
  appearing to do nothing.

## 0.4.1 (15) — 2026-09-28

- A Bitcoin (BTC) wallet with a lot of history could still take several
  requests to fully load, even after 0.4.0's single-request scan: the app
  would send a small guess first and grow it in several follow-up round
  trips whenever a wallet's real usage went deeper than the guess. It now
  sends one properly-sized request every time, so opening or refreshing a
  BTC wallet is a single round trip regardless of how much history it has.
- The backend no longer re-fetches an address's full transaction history on
  every single refresh — it now remembers what it already knows and only
  asks upstream about addresses that could actually have something new,
  which is most of why repeat loads of the same wallet are now fast.
- Fixed a bug where a BTC wallet could show a "pending" transaction that
  wasn't real — a stale entry from an upstream data source that had never
  actually reached the Bitcoin network. Pending transactions are now
  double-checked against the real network before being shown.

## 0.4.0 (14) — 2026-09-19

- Wallets with a lot of history now load in about a second instead of tens of
  seconds. The app used to ask the backend about each address one at a time
  (hundreds of requests per refresh, which the backend then rate-limited); it
  now sends the wallet's addresses in a single request and gets the balance,
  coins and recent history back together. Older backends and public explorers
  still work — the app falls back to the old address-by-address scan.
- A scan that couldn't check every address now reports an error instead of
  quietly showing a smaller balance. Previously an address the backend rate-
  limited mid-scan was skipped without any sign, which could understate a
  wallet's balance.

## 0.3.8 (13) — 2026-09-18

- A deep watch-only wallet (100+ used addresses) could feel slow to open,
  even after the balance had already loaded once. History now shows as
  soon as it's ready instead of waiting on the balance check that follows
  it, and network requests during a scan now actually run at the
  concurrency the app intends instead of being silently throttled.

## 0.3.7 (12) — 2026-09-18

- Fixed a watch-only wallet (BTC and XBT) undercounting its balance and
  missing transactions: a backend caching bug could remember an address as
  having no history when it actually did, and the on-device cache had no
  way to know that answer was wrong. The backend bug is already fixed;
  this release also clears any previously-cached incorrect data on-device
  so it can't keep showing stale results after updating.

## 0.3.6 (11) — 2026-09-16

- Fixed a bug where switching wallets while a slow scan was still finishing
  could briefly show the *previous* wallet's balance and history under the
  new one, before correcting itself a few seconds later. Switching wallets
  now always shows the new wallet's own state.

## 0.3.5 (10) — 2026-09-15

- Reopening a wallet (or switching between them) now shows your last-known
  balance and history immediately instead of waiting on a fresh scan every
  time — confirmed transactions are remembered on-device, since they never
  change. The app still quietly checks for anything new right after.

## 0.3.4 (9) — 2026-09-15

- Fixed a watch-only wallet showing as fully connected (green dot, current
  block height) while its balance and history were still loading in the
  background — most noticeable on a deep BTC wallet, which can take a while
  to scan. Now stays on "connecting…" until the balance actually arrives.

## 0.3.3 (8) — 2026-09-15

- Fixed a crash: a watch-only wallet with hundreds of used addresses could
  run out of memory partway through scanning and close the app.
- Fixed a bug where a wallet with a lot of history could get stuck showing
  "connecting…" indefinitely, or show a wrong (too low) balance, especially
  after leaving the wallet screen open for a while. Refreshing no longer
  piles up overlapping scans on top of each other, and a single address
  that's slow to answer no longer cuts the rest of the scan short.
- Address scans are noticeably faster: requests within a scan now go out
  several at a time instead of strictly one after another.
- Backend reliability: the hosted service now tries more than one data
  provider and keeps the useful parts of its cache across restarts, so a
  hiccup with one provider is less likely to be visible in the app.

## 0.3.2 (7) — 2026-09-15

- Fixed a bug where a watch-only wallet with a lot of history could show
  **no** balance or history at all, instead of just an incomplete one. The
  deeper address scan added in 0.3.1 sends a lot more requests to check a
  wallet's history, and a single rate-limited or slow reply partway through
  used to abort the whole scan and leave the screen blank. It now keeps
  whatever it already found and retries a couple of times before giving up
  on any one address, so a transient hiccup no longer wipes out an
  otherwise-good result.

## 0.3.1 (6) — 2026-09-15

- Fixed a watch-only wallet bug: importing an xpub with a lot of prior history
  could show the wrong balance and be missing transactions. The address scan
  now keeps looking as far as real activity goes, instead of stopping after a
  fixed 20 addresses per branch. If you imported a watch-only wallet before
  this update, reopen it (or pull to refresh) to pick up anything that was
  missed.
- Accepts `zpub`/`ypub` (and testnet `vpub`/`upub`) when importing a watch-only
  xpub, not just the plain `xpub` form — what most wallets and hardware
  devices actually show for a native SegWit account.

## 0.3.0 (5) — 2026-09-14

- Watch-only wallets: import an account-level extended public key (xpub) to
  watch a wallet's addresses, balance, and history — with no seed and no
  ability to ever spend from it. Useful for watching a hardware wallet or
  another device's wallet from your phone.

## 0.2.0 (4) — 2026-09-10

- Changes for fortistechlabs.com domain. Android treats this as a new app: it
  installs alongside the old one and does **not** carry your wallets over.
  **Install 0.2.0, restore each wallet from its recovery phrase, then uninstall
  the old app.** Same signing key as before, so the certificate fingerprint is
  unchanged.
- No feature changes.

## Backend — 2026-09-10

- Payments through the hosted backend carry a 1% service fee (minimum 400 sat)
  that funds it. The app adds it as a transaction output and shows the exact
  amount on the confirmation screen before you sign. No app update needed — the
  app reads this from the backend.

## 0.1.2 (3) — 2026-09-09

- Backend moved to `api.fortistechlabs.com` (was `api.fortis.rest`). The old
  host keeps working through the transition. If your device can't reach the
  backend it still falls back to public block explorers.
- Website is now [fortistechlabs.com](https://fortistechlabs.com/).

## 0.1.1 (2) — 2026-09-09

First public release. Direct download + [Obtainium](https://obtainium.imranr.dev/).

- Non-custodial wallet for Bitcoin XBT and Bitcoin BTC. Keys and the recovery
  phrase are generated on-device and never leave it.
- Create or restore a wallet (12/24 words + optional BIP-39 passphrase).
- Up to 10 named wallets on either chain.
- One app lock — fingerprint / face / device PIN — opens every wallet. Key held
  in the device's secure hardware (StrongBox where available); Settings warns if
  only software-backed.
- Receive (QR + share), Send (QR scan or paste, fee tiers or custom sat/vB,
  "take the fee from the amount"), History, approximate USD value.
- Opt-in XBT replay protection on BTC sends.
- "Receive" auto-advances past addresses already used.
- Balance count-up animation.
- 75 languages.
- Recovery-phrase and password screens blocked from screenshots / screen
  recording / the app switcher.
- No analytics, no ads, no third-party trackers. The backend only ever sees
  public addresses and finished signed transactions — no account, no sign-up.
- You set the Bitcoin network fee (fast / normal / slow, or a custom sat/vB rate).
