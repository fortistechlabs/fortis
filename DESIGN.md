# fortis — visual design language

**Glassy & layered.** Dark by default. Frosted translucent panels over a deep,
softly glowing gradient; a few depth planes that move at different rates. Content
is quiet, money is loud.

One set of tokens, two implementations: CSS custom properties in
[`web/style.css`](web/style.css), and a Compose `Theme` later. Names match.

## Planes (z-order, back to front)

| plane | what | motion |
|---|---|---|
| **ambient** | full-viewport gradient + 2 blurred colour blobs | slow infinite drift; ~6% parallax on scroll |
| **hero** | the balance panel — strongest glass, accent glow | still; balance value counts up on change |
| **surface** | cards (tabs, receive, send, history) | fade + 6px rise on mount, spring easing |
| **overlay** | confirm sheet, toast | rise from bottom |

## Tokens

### Colour (dark)

```
--bg-0            #070810     deepest
--bg-1            #0b0d18     gradient top
--bg-2            #0e1222     gradient bottom
--blob-a         rgba(96,142,255,.28)     cool
--blob-b         rgba(178,120,255,.22)    violet
--text           #eef1f8
--text-dim       #9aa3ba
--text-faint     #5c6580
--accent         #6ea8fe
--accent-2       #b98cff
--good           #49e0a6
--bad            #ff6b7d
--warn           #f6c445
```

Light mode swaps `--bg-*` for `#eef1f8 → #dfe4f2`, glass to `rgba(255,255,255,.62)`,
text to near-black; blobs and accent unchanged.

### Glass

```
--glass-1        rgba(255,255,255,.045)      surface cards
--glass-2        rgba(255,255,255,.07)       hover / inputs
--glass-hero     rgba(255,255,255,.06)       balance panel
--hair           rgba(255,255,255,.09)       1px borders
--sheen          inset 0 1px 0 rgba(255,255,255,.07)   top inner highlight
--blur           18px       backdrop-filter blur (+ saturate(1.5))
```

### Elevation  (shadow + glow)

```
--e-card   0 10px 34px -14px rgba(0,0,0,.55), var(--sheen)
--e-hero   0 20px 60px -22px rgba(0,0,0,.6), 0 0 70px -26px var(--accent), var(--sheen)
--e-pop    0 24px 70px -20px rgba(0,0,0,.7)
--glow-accent   0 0 0 1px rgba(110,168,254,.35), 0 0 24px -6px rgba(110,168,254,.55)
```

### Shape & space

```
--r-sm 12px   --r 18px   --r-lg 26px   --r-pill 999px
space: 4 8 12 16 24 32 48   (--s1 … --s7)
--app-w 460px
```

### Type

```
font: system-ui stack; addresses & amounts use ui-monospace.
--fs-display 2.15rem / 650 / -0.02em   tabular-nums   (balance)
--fs-title   1.05rem / 620
--fs-body    0.95rem / 430
--fs-label   0.78rem / 500 / 0.02em / --text-dim   (uppercase-ish labels)
--fs-mono    0.92rem
```

### Motion

```
--ease-spring  cubic-bezier(.22, 1, .36, 1)
--ease-out     cubic-bezier(.2, .6, .2, 1)
--t-fast 160ms   --t 280ms   --t-count 650ms
```

All motion is gated on `@media (prefers-reduced-motion: reduce)` → transitions
collapse to none, blobs stop, balance snaps.

## Component recipes

- **glass card** — `background: var(--glass-1); backdrop-filter: blur(var(--blur)) saturate(1.5); border: 1px solid var(--hair); border-radius: var(--r); box-shadow: var(--e-card)`
- **hero panel** — glass card with `--glass-hero` + `--e-hero`; balance in `--fs-display` with a faint text-glow (`text-shadow: 0 0 22px rgba(110,168,254,.25)`)
- **primary button** — gradient `linear-gradient(135deg, var(--accent), var(--accent-2))`, white ink, `--r-pill`, `box-shadow: var(--glow-accent)`; press → `scale(.97)`
- **ghost button** — `--glass-2`, hairline, `--text`; hover lifts `--glass` one step
- **input** — `--bg-0` at 60%, hairline, `--r-sm`; focus → 2px accent ring + faint glow
- **tab bar** — a glass pill; active tab is a filled inner pill sliding under the label
- **status dot** — 7px, `--good` glowing when synced, `--warn` pulsing when syncing
- **pane transition** — `@keyframes paneIn { from { opacity:0; transform: translateY(6px) scale(.995) } }`, `--t` `--ease-spring`
- **confirm sheet** — overlay plane, rises from bottom, `--e-pop`, dim scrim behind

## Ambient background

Two absolutely-positioned radial-gradient blobs (`--blob-a`, `--blob-b`), each
~70vw, `filter: blur(60px)`, `opacity: .9`, on a `--bg-1 → --bg-2` linear base.
`@keyframes drift` translates/rotates each blob over 40–60s, opposite directions.
Scroll handler nudges them `translateY(scrollY * -0.06)`.
