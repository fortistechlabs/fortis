# fortis brand

The **keyhole shield**: a shield outline with a keyhole in the green signal
colour. One drawing on a 64 × 64 grid; the line gets heavier as the mark gets
smaller, because a hairline disappears at favicon size.

| file | use at | shield line | keyhole |
|---|---|---|---|
| `fortis-mark.svg` | ≥ 48 px on dark — app icon, splash, onboarding | 2.2 | circle + slot |
| `fortis-mark-light.svg` | ≥ 48 px on light | 2.2 | circle + slot |
| `fortis-mark-lockup.svg` | beside the wordmark (~30 px) | 3.5 | larger circle + slot |
| `fortis-mark-small-32.svg` | ~32 px — favicons, tabs, notifications | 4.5 | larger circle + slot |
| `fortis-mark-small-16.svg` | ~16 px | 7 | a single dot |
| `fortis-wordmark.svg` | headers, banners | lockup mark + "fortis" | — |

All marks are transparent — put them on a tile yourself (app icons use
`#111315`).

## Colours

| | on dark | on light |
|---|---|---|
| shield | `#E9ECEF` | `#0B0C0D` |
| keyhole | `#00FF41` (signal, Matrix green) | `#008F25` (≥ 3:1 on `#E9ECEF`) |
| tile | `#111315` | `#E9ECEF` |

## Wordmark

"fortis" is JetBrains Mono Medium converted to outlines, so the SVG needs no
font. To regenerate it (e.g. after a font change):

```sh
python3 -m venv /tmp/venv && /tmp/venv/bin/pip install fonttools
/tmp/venv/bin/python -I brand/tools/wordmark.py \
  android/app/src/main/res/font/jetbrains_mono_medium.ttf brand/fortis-wordmark.svg
```

## Where it's used

- Android: `res/drawable/ic_launcher_foreground.xml` (scaled 1.05 into the
  adaptive icon's 66 dp safe zone), `ic_launcher_background.xml` (`#111315`),
  `ic_mark.xml` (onboarding).
- Web wallet: `web/icon.svg` (= `site/icon.svg`), `icon-192.png` / `icon-512.png`
  (the mark on a rounded `#111315` tile), `apple-touch-icon.png` (square tile),
  and the onboarding mark inline in `web/src/app.js`.
- Site: `site/icon.svg`.
