# fortis — visual design language

**Graphite signal.** Flat graphite, high-contrast text, one green signal
accent, monospace figures. Quiet and precise: readability comes from contrast,
not size; colour is reserved for meaning. Dark only.

Implemented on Android in [`ui/theme/Theme.kt`](android/app/src/main/kotlin/com/fortis/wallet/ui/theme/Theme.kt)
(`Fx` tokens, `FxType` type scale) and [`ui/Glass.kt`](android/app/src/main/kotlin/com/fortis/wallet/ui/Glass.kt)
(components). Names match across platforms.

> **Not yet adopted by the web wallet or the site.** [`web/style.css`](web/style.css)
> and [`site/`](site) still implement the previous "Glassy & layered" language
> and castle-shield logo until their own restyles.

## Principles

1. **Contrast over size.** Every text colour is ≥ 4.5:1 on both the background
   and a panel; interactive borders ≥ 3:1. Android's `ContrastTest` enforces it.
2. **Values are never faint.** Amounts and fiat are always monospace, ≥ 13 sp,
   in `text` or `textDim` — never `textFaint`.
3. **One signal.** Green marks state, focus and the main action: active tab, the
   hero's top line, the coin unit, incoming amounts, focus rings, the sync dot,
   and the primary button.
4. **Flat and precise — except the things you press.** No glows, blur or
   ambient motion; 1 px lines; panels and fields 4–8 dp corners; no pills.
   Buttons and tabs are the tactile exceptions: buttons are gently shaded and
   press down, tabs are real folder tabs.

## Tokens

| token | value | use | vs bg0 / panel |
|---|---|---|---|
| `bg0` | `#0B0C0D` | screen | — |
| `panel` | `#111315` | cards, fields, sheets, dialogs | — |
| `panelRaised` | `#16191C` | selected rows, address/phrase wells, chips | — |
| `line` | `#222528` | decorative dividers, panel edges | 1.27 / 1.21 (decorative) |
| `control` | `#5C636A` | borders of fields and secondary buttons | 3.21 / 3.06 |
| `text` | `#E9ECEF` | primary text, values | 16.51 / 15.70 |
| `textDim` | `#9AA1A8` | secondary text, fiat, meta, labels | 7.49 / 7.13 |
| `textFaint` | `#7D848B` | instructional hints only | 5.17 / 4.92 |
| `signal` | `#00FF41` | the one accent, Matrix green (see principle 3) | 14.34 / 13.64 |
| `warn` | `#E6C15A` | pending, caution notes | 11.32 / 10.76 |
| `bad` | `#F2545B` | errors, destructive actions | 5.78 / 5.49 |
| `primary` / `onPrimary` | `#E9ECEF` / `#0B0C0D` | primary button fill / label | 16.51 |

Buttons: primary — signal green, `primaryTop` `#4DFF7A` → `primaryBottom`
`#00D936`, pressed `#00C832`, on `primaryLip` `#006B1B`, label `onPrimary`
`#03140A`. Secondary — solid slate, `ghostTop` `#3A424A` → `ghostBottom`
`#2B3238`, pressed `#272D33`, on `ghostLip` `#15191C`; no outline. On slate the
dim and error tints are lifted (`dimOnSlate` `#B4BBC2`, `badOnSlate` `#FF8A8F`,
via `ghostLabel`) to keep 4.5:1. Shading stays under 1.5:1 top to bottom.

Corners `rSm` 4 · `r` 6 · `rLg` 8 · `rBtn` 10 (buttons). Lip `lip` 3 dp. Spacing `s1…s6` = 4 8 12 16 24 32.

## Type

**JetBrains Mono** (400, 500, 600) for everything — words, numbers, addresses,
txids, the recovery phrase — with tabular figures. One typeface, terminal-style;
scripts it lacks fall back to the system font. SIL OFL 1.1 — the licence text
ships in `android/app/src/main/assets/licenses/`.

| style | font | size | colour | use |
|---|---|---|---|---|
| `balance` | Mono 500, −0.01 em | 36 | text | hero balance |
| `fiat` | Mono | 17 | textDim | ≈ US$ under the hero |
| `amount` | Mono | 15 | text | amounts in lists, history, confirm; addresses |
| `fiatSmall` | Mono | 13 | textDim | ≈ US$ in lists, pending, chips |
| `title` | Mono 600 | 17 | text | screen and card titles |
| `body` | Mono | 15 | — | running text |
| `meta` | Mono | 13 | textDim | hints, block height, timestamps, notes |
| `label` | Mono 500, +0.08 em, CAPS | 11 | textDim | small labels (BTC · MAIN) |
| `nav` / `button` | Mono 600, +0.06 em, CAPS | 12 | — | tabs, buttons |
| `brand` | Mono 500 | 32 | text | the "fortis" wordmark |

## Components

- **Backdrop** — flat `bg0`. No blobs, no animation.
- **Panel** (`GlassCard`) — `panel`, 1 px `line`, `r`. The hero (balance) panel
  replaces its top edge with a 1 px `signal` line.
- **Buttons** — filled, never just outlined: a face shaded lighter at the top,
  with a faint 1 px highlight along its top edge, standing 3 dp proud of a darker
  lip. Pressed, the face
  sinks onto the lip (50 ms), goes flat and slightly darker, and loses the
  highlight. `rBtn` corners; minimum `btnH` 48 dp face (`btnHDense` 40), growing when a long
  label wraps; labels centred with 12 dp side padding.
- **Primary button** — the screen's main action, in signal green, `onPrimary`
  CAPS label. Disabled: 40 % opacity, no press.
- **Secondary button** (`GhostButton`) — solid slate, label via `ghostLabel`.
- **Pressable tile** (`PressableTile`) — a whole row you can press, e.g. each
  wallet on Home: the secondary-button key with 16 × 12 dp padding, a `signal`
  chevron, and a 1.5 dp `signal` outline when selected. Secondary text in
  `dimOnSlate`.
- **Field** — `panel` fill, 1 px `control`, `signal` when focused, `rSm`; label
  in `label` style above it; monospace fields in JetBrains Mono.
- **Tabs** (nav bar, wallet tabs, segmented pickers) — folder tabs with `r`
  top corners, 4 dp apart, on a 1 px `control` baseline. Selected: `panelRaised`
  fill, `control` outline, 2 dp `signal` edge on top, `text` label, and open at
  the bottom (no baseline under it). Others: `panel` fill, `line` outline,
  `textDim` label.
  Labels stay on one line, shrinking (down to 9 sp) when a translation is long.
- **Sheets and dialogs** — `panel` container.
- **Receive QR** — always on white, for scanners.
- **Status dot** — 7 dp; `signal` when synced, `warn` otherwise.

## Logo

The **keyhole shield** — see [`brand/README.md`](brand/README.md) for the files,
sizes and colours. App-icon tile `panel` `#111315`.
