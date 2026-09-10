# Google Play listing — fortis

Draft copy and assets for the Play Console listing. Fill the `TODO` items.

## Identity

| Field | Value |
|---|---|
| App name | **Fortis Wallet** |
| Developer/publisher | **Fortis Tech Labs** |
| Package | `com.fortistechlabs.wallet` |
| Category | Finance |
| Contact email | info@fortistechlabs.com |
| Website | https://fortistechlabs.com |
| Privacy policy | https://fortistechlabs.com/privacy |

## Short description (≤ 80 chars)

```
Non-custodial wallet for Bitcoin XBT and Bitcoin BTC.
```

## Full description (≤ 4000 chars)

```
fortis is a non-custodial wallet for Bitcoin XBT and Bitcoin BTC.

Your keys are yours. The recovery phrase is generated on your device and
encrypted under a password you choose. It is never uploaded, and neither fortis
nor anyone else can move your funds or recover your phrase for you.

WHAT YOU CAN DO
• Create a wallet with a 12- or 24-word recovery phrase, with an optional BIP-39
  passphrase (a "25th word") for extra protection.
• Restore an existing wallet — word-by-word autocomplete helps you type the
  phrase without mistakes.
• Receive: show a QR code for your address, copy it, or share it.
• Send: scan a QR code with the camera or paste an address, choose a fee
  (fast / normal / slow, or a custom sat/vB rate), review, and sign.
• Tap any transaction to open it in a public block explorer.
• Switch between the Bitcoin and XBT chains.

BUILT FOR SELF-CUSTODY
• Keys are encrypted at rest with a key derived from your password.
• The screens that show your recovery phrase or a password are excluded from
  screenshots, screen recording, and the app switcher.
• When you create a wallet, fortis stirs in extra randomness from your device's
  motion sensors and timing jitter, on top of the platform's secure generator.
• XBT spends use the fork's opt-in replay-protected signature type so a
  transaction is bound to one chain.

PRIVACY
fortis collects no analytics and contains no advertising or third-party
trackers. To show balances and broadcast transactions the app sends your public
wallet addresses to its backend; there is no sign-up, email, or account. Full
detail: https://fortistechlabs.com/privacy

FEES
The Bitcoin network fee is set by you (fast / normal / slow, or a custom
sat/vB rate). Payments made through the hosted backend also carry a 1% service
fee (minimum 400 sat) that funds it, added as a transaction output and shown
before you sign.

fortis is early software. Back up your recovery phrase offline and start small.
```

## Data safety form

| Question | Answer |
|---|---|
| Does your app collect or share user data? | Yes |
| **Data types collected** | *Financial info* → "Other financial info": public wallet addresses and the on-chain transactions for them, submitted (signed) transactions. *App info and performance* → "Crash logs": app/OS/device version + stack trace. *App activity* → none. *Location, Personal info, Contacts, Messages, Photos, Audio, Files, Calendar, Device IDs* → none. |
| **Data shared with third parties** | Wallet addresses and IP address are processed by Cloudflare (infrastructure) and, only during a backend outage, by mempool.space / mempool.guide (fallback block-data sources). Crash logs are sent to a fortis-operated endpoint. |
| Is data encrypted in transit? | Yes (HTTPS) |
| Can users request data deletion? | Yes — support@fortistechlabs.com; on-device data is removed on uninstall / "Forget this wallet" |
| Is any collected data required? | Wallet addresses are required to show balances; the crash log and the per-install token are not linked to the user's identity |
| Data collection purposes | App functionality; (crash logs) diagnostics |

## Permissions

| Permission | Why | Declaration |
|---|---|---|
| `INTERNET` | Talk to the backend | standard |
| `CAMERA` | Scan a payment address QR code, on-device only | in-app + Data safety: not collected |
| `USE_BIOMETRIC` | (reserved — biometric unlock) | standard |

`ACCESS_LOCAL_NETWORK` and cleartext traffic are **debug builds only** — the
release build is HTTPS-only and never touches the local network.

## Content rating

Finance / crypto app, no objectionable content. Answer "no" to all violence /
sexual / drug / gambling questions; note it references cryptocurrency.

## Crypto declaration

Play Console → App content → "Crypto Exchanges and Software Wallets":
declare a **non-custodial software wallet** — keys are generated and stored on
the user's device and never leave it; fortis cannot access or move user funds.
The operating entity is **Fortis Tech Labs** (address as in the privacy policy).

## Assets

| Asset | File | Spec |
|---|---|---|
| App icon | `play-icon-512.png` (from `web/icon.svg`) | 512×512 PNG |
| Feature graphic | `feature-graphic.png` | 1024×500 PNG |
| Phone screenshots | `screenshots/0{1..4}-*.png` | 1344×2688 (2:1) |

Screenshots are from the **release** build (`app-release.apk`, R8-minified,
upload-key-signed), `Pixel_8_Pro_AVD`, 1344×2688 with the status bar + gesture
pill cropped off. Password-locked wallet "Savings" on XBT against the live
`api.fortistechlabs.com` ("connected"). Last grabbed 2026-09-09 **without** the
service fee — re-grab `03-send.png` (and the confirm screen) before submitting,
now that the 1% fee is back on and the send screen shows the fee line again.
Consider adding framed/captioned marketing versions later.

## TODO before submitting

- [ ] Fill `[DATE]` and the registered address in `site/privacy.html`, deploy `site/`
- [ ] Generate the upload keystore, fill `android/keystore.properties`
- [ ] `:app:bundleRelease` → upload the `.aab`
- [ ] Confirm the app name on Play (trademark)
- [ ] Play Console: $25 + identity verification; if a new personal account,
      12 testers × 14 days closed testing before production
