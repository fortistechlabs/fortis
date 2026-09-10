# Changelog

Fortis Wallet (Android). Versions are `versionName (versionCode)`.

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
