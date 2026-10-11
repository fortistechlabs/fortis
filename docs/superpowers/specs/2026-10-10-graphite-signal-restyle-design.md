# Graphite signal — Android restyle — design

**Status:** implemented · 2026-10-10 — revised during the device check at the
owner's direction: JetBrains Mono replaces Inter everywhere; signal is Matrix
green `#00FF41`; buttons are filled, shaded and press down (green primary,
slate secondary); tabs are folder tabs; Home wallets are pressable tiles.
`DESIGN.md` is the current reference where it and this spec differ.
**Scope:** the Android app's visual language (`android/`), the new fortis logo
(master files + Android launcher icon), and `DESIGN.md` rewritten to describe
both. The web wallet and the site adopt them next, in their own specs.

## Why

Text is hard to read. The palette is pale text on translucent blue/violet
glass over drifting colour blobs. The worst offenders aren't the headline
numbers but the small text: every ≈ US$ value is 11–12 sp in the faintest grey
(`Fx.textFaint` `#8A93A8`), and ~40 hints/labels are 12 sp faint. The owner
wants it readable, darker, and "cypherpunk" — but sophisticated, not loud:
two rounds of mockups rejected big text, big buttons, neon and CRT looks in
favour of **Graphite signal** (`.superpowers/brainstorm/…/visual-direction-v2.html`,
option C).

## Goals

1. Every text colour ≥ 4.5:1 against both the background and the panel; control
   borders ≥ 3:1. Enforced by a unit test.
2. Fiat values and amounts readable at a glance: no value is shown in the faint
   colour or below 13 sp.
3. A quiet, precise look: flat graphite, 1 px lines, tight corners, one green
   signal accent, monospace figures.
4. No change to behaviour, layout structure or strings.
5. A new logo — the keyhole shield (option C1) — replaces the gradient castle
   shield on Android, with master files the web wallet and site reuse.

## Non-goals

The web wallet (`web/`) and the site (`site/`) — follow next, each in its own
spec, reusing `brand/` and `DESIGN.md`.
Light mode. New screens or navigation. Translating anything.

## Approach

Re-theme through the existing seams rather than renaming everything:
`Fx` (`ui/theme/Theme.kt`) keeps its token names where the meaning holds and gets
new values; the components in `ui/Glass.kt` keep their names and signatures and
get new rendering; the ~50 hard-coded `fontSize`s and ~10 hard-coded colours in
`Screens.kt` / `HwConnectScreen.kt` are replaced with named styles and tokens.
Rejected: renaming all ~270 token call sites (same result, much noisier diff);
a Material 3 theming rebuild (too large for a re-skin).

## Tokens

| token | today | new | ratio vs bg / panel |
|---|---|---|---|
| `bg0` (screen) | gradient `#0A0C1A→#0C1024` + blobs | `#0B0C0D` flat | — |
| `panel` (was `glass1`) | `#FFFFFF` @ 4.5% | `#111315` | — |
| `panelRaised` (was `glass2`) | `#FFFFFF` @ 8% | `#16191C` | — |
| `line` (was `hair`) — decorative dividers, panel edges | `#FFFFFF` @ 9% | `#222528` | 1.27 / 1.21 (decorative, exempt) |
| `control` *(new)* — input and secondary-button borders | `hair` | `#5C636A` | 3.21 / 3.06 |
| `text` | `#EEF1F8` | `#E9ECEF` | 16.51 / 15.70 |
| `textDim` — secondary text, fiat, meta | `#B7BFD2` | `#9AA1A8` | 7.49 / 7.13 |
| `textFaint` — hints only, never values | `#8A93A8` | `#7D848B` | 5.17 / 4.92 |
| `signal` (was `accent`) — the one accent | `#6EA8FE` | `#3DDC97` | 11.08 / 10.54 |
| `accent2` | `#B98CFF` | removed — call sites move to `signal` or `textDim` | — |
| `good` | `#49E0A6` | `= signal` | — |
| `warn` | `#F6C445` | `#E6C15A` | 11.32 / 10.76 |
| `bad` | `#FF6B7D` | `#F2545B` | 5.78 / 5.49 |
| `primary` *(new)* — primary button fill | blue→violet gradient | `#E9ECEF` | — |
| `onPrimary` *(new)* | `#0A0C16` hard-coded | `#0B0C0D` | 16.51 on `primary` |
| `blobA/B/C`, `bg1`, `bg2`, `glassHero` | — | removed | — |
| corners `rSm / r / rLg / pill` | 12 / 18 / 26 / 999 dp | **4 / 6 / 8** dp; `pill` removed (buttons use `rSm`) | — |

Spacing tokens `s1…s6` are unchanged.

**Signal** is used for: active tab underline, the hero panel's top line, the
BTC/XBT unit after the balance, incoming amounts, focus rings, the sync dot.
Nothing else — it stays a signal.

## Type

Fonts bundled in `res/font/` (SIL Open Font License 1.1; licence texts in
`app/src/main/assets/licenses/`):

- **JetBrains Mono** Regular, Medium — every number, address, txid, the
  recovery-phrase grid, monospace fields. Tabular figures (`tnum`).
- **Inter** Regular, Medium, SemiBold — everything else.

Named styles in `Theme.kt` (an `FxType` object alongside `FortisType`); every
hard-coded `fontSize` maps to one:

| style | use | today | new |
|---|---|---|---|
| `balance` | hero balance | 32 sp mono SemiBold | 36 sp mono Medium, −0.01 em |
| `fiat` | ≈ US$ under the hero | 12 sp `textDim` | 17 sp mono, `textDim` |
| `amount` | amounts in lists, history, confirm sheet | 12–14 sp mixed | 15 sp mono, `text` |
| `fiatSmall` | ≈ US$ in lists | 11 sp `textFaint` | 13 sp mono, `textDim` |
| `title` | screen/card titles | 17 sp | 17 sp Inter SemiBold |
| `body` | running text | 15 sp | 15 sp Inter |
| `meta` | hints, block height, "address #0", timestamps | 11–12 sp `textFaint` | 13 sp Inter, `textDim` (`textFaint` only for genuine hints) |
| `label` | small-caps labels (BTC · MAIN, RECENT) | 12 sp | 11 sp Inter Medium, uppercase, +0.14 em, `textDim` |
| `nav` / `button` | tabs, buttons | 13–14 sp | 12 sp Inter SemiBold, uppercase, +0.12 em |
| `brand` | "fortis" wordmark | 38 sp Bold | 32 sp mono Medium |

## Components (`ui/Glass.kt` — names and signatures unchanged)

- **`AmbientBackground`** — flat `bg0`. The blobs and their infinite animation
  go (also a battery saving).
- **`GlassCard`** — `panel` fill, 1 dp `line`, `r` corners. New optional
  parameter `hero: Boolean = false` draws a 1 dp `signal` line along the top
  edge instead of the full border's top segment. The Wallet hero passes it.
- **`PrimaryButton`** — 40 dp tall, `primary` fill, `onPrimary` text, `rSm`
  corners, `button` style; `enabled = false` renders at 40% alpha (today it looks
  identical when disabled).
- **`GhostButton`** — 40 dp, transparent, 1 dp `control` border, `text` (or
  `tint`) label, `button` style.
- **`Field`** — `panel` fill, 1 dp `control` border, `signal` when focused,
  `rSm` corners; mono fields use JetBrains Mono; label in `label` style.
- **`Screen`**, **`ErrorText`** — colours only.
- **`BrandMark`** — `brand` style; logo unchanged.

In `Screens.kt`:

- **`NavBar`** and the Wallet tab's Receive/Send/History switch become text
  tabs: `nav` style, active = `text` + 1 dp `signal` underline, inactive =
  `textDim`; no filled pills. The hard-coded `Color(0xFF0A0C16)` /
  `Color.White.copy(alpha = 0.75f)` go.
- **`Segmented`** (chain picker etc.) — same treatment as the tabs.
- **Bottom sheets** (`ConfirmSheet`, `XbtOfferSheet`) — `panel` container, 1 dp
  `line` top edge.
- Every fiat value uses `fiat` / `fiatSmall`; every amount uses `amount`.

## Logo — keyhole shield (C1)

Chosen from `.superpowers/brainstorm/…/logo-c-refine.html`. Geometry on a
64 × 64 grid:

- **Shield:** `M32 7 53 14.5V30c0 13.5-8.8 21.6-21 26.5C19.8 51.6 11 43.5 11 30V14.5Z`,
  stroked, round joins, no fill.
- **Keyhole** (filled, `signal`): circle `cx 32 cy 27 r 5` + trapezoid
  `M29.2 30.5h5.6L36.5 42h-9Z`.

Weights by rendered size — one drawing doesn't survive every size:

| variant | for | shield stroke | keyhole |
|---|---|---|---|
| `fortis-mark` | ≥ 48 px: app icon, splash, onboarding | 2.2 | circle + trapezoid |
| `fortis-mark-lockup` | beside the wordmark (~30 px) | 3.5 | circle r 5.5 + trapezoid |
| `fortis-mark-small` | ≤ 32 px: favicons, notifications | 4.5 (32 px), 7 (16 px) | 32 px: circle r 6.5 + trapezoid; 16 px: a single dot r 9 |

Colours: on dark — shield `text` `#E9ECEF`, keyhole `signal` `#3DDC97`. On
light — shield `#0B0C0D`, keyhole `#188A5A` (green darkened for ≥ 3:1 on
`#E9ECEF`). App-icon tile: `panel` `#111315`.

**Master files** (new `brand/` directory, the single source the web wallet and
site copy from): `fortis-mark.svg`, `fortis-mark-light.svg`,
`fortis-mark-lockup.svg`, `fortis-mark-small-32.svg`, `fortis-mark-small-16.svg`,
each with a 64 × 64 `viewBox` and no embedded fonts; and
`fortis-wordmark.svg` (lockup mark + "fortis" in JetBrains Mono Medium, text
converted to paths so it needs no font).

**Android:**
- Adaptive launcher icon (`res/drawable/ic_launcher*.xml`): background flat
  `#111315` (gradient removed); foreground = `fortis-mark` scaled into the 66 dp
  safe zone of the 108 dp canvas; `monochrome` = the same paths in a single
  colour (themed icons, Android 13+).
- `BrandMark` shows a new `res/drawable/ic_mark.xml` (the mark on transparent),
  96 dp, above the wordmark.

## Testing

- **Contrast test (written first).** `ui/theme/Contrast.kt` gains
  `contrastRatio(fg: Color, bg: Color): Double` (WCAG 2.x relative luminance).
  `app/src/test/…/ContrastTest.kt` asserts, against both `bg0` and `panel`:
  `text`, `textDim`, `textFaint`, `signal`, `warn`, `bad` ≥ 4.5; `control` ≥ 3.0;
  and `onPrimary` on `primary` ≥ 4.5. Plus a known-value check of the helper
  (black on white = 21.0).
- Existing suites stay green: the 7 Android unit tests, `cargo test --workspace`,
  `:app:assembleDebug`.
- **On device** (the `.dev` debug build beside the store app): screenshot and
  review Onboard, Gen/Create, Restore, Watch, Hardware connect, Home, Wallet
  (Receive, History), Settings, the XBT offer sheet, the app-lock screen; then
  switch the in-app locale to German and check for overflow/truncation at the
  new sizes. Screenshots go to the owner before sign-off.

## Rollout

Order, each step leaving the app building: fonts → tokens + type (`Theme.kt`)
→ components (`Glass.kt`) → screen sweep (`Screens.kt`, `HwConnectScreen.kt`)
→ `DESIGN.md` rewrite + CHANGELOG. Nothing is committed by the implementer.

## Risks

- **Longer locales at larger fiat/meta sizes** may wrap — the German pass
  covers the worst case; fix by allowing wrap, never by shrinking below the
  table.
- **APK size** grows by the five font files (~600 KB).
- **Store listings and Zapstore** still show the old icon until their
  graphics (`store/`, `zapstore.yaml` icon) are regenerated — part of the site
  spec, not this one.
- **Web and Android diverge** until the web wallet is restyled; `DESIGN.md`
  describes the new language, with a note that `web/style.css` still uses the
  old one.
