# Graphite Signal Restyle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restyle the Android app to the "Graphite signal" language — flat graphite, high-contrast text, one green signal accent, bundled JetBrains Mono + Inter, a named type scale — and replace the logo with the keyhole shield, with no behaviour or layout change.

**Architecture:** Re-theme through the existing seams: `Fx` tokens in `ui/theme/Theme.kt` get new values (a few renamed/removed, so the compiler finds every stale call site), a new `FxType` object holds the named text styles, `ui/Glass.kt` components keep their names/signatures and get new rendering, then a sweep replaces every hard-coded `fontSize` / colour in the screens.

**Tech Stack:** Kotlin, Jetpack Compose (Material 3), JUnit 4 JVM unit tests, Gradle (AGP 9.4).

**Spec:** `docs/superpowers/specs/2026-10-10-graphite-signal-restyle-design.md`

## Global Constraints

- **No commits.** The owner commits. Every "Commit" step below is replaced by: leave changes in the working tree and list the touched files in your report.
- No change to behaviour, layout structure, or strings (`res/values*/strings.xml`).
- Logo geometry, stroke weights and colours exactly as the spec's "Logo — keyhole shield (C1)" section.
- Fonts: JetBrains Mono **v2.304** (`https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip`) and Inter **v4.1** (`https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip`), SIL OFL 1.1. Download into the scratchpad, never execute anything from the archives.
- Colours exactly as the spec's token table; corners `rSm`/`r`/`rLg` = 4/6/8 dp; spacing `s1…s6` unchanged.
- Thresholds: text tokens ≥ 4.5:1 and `control` ≥ 3.0:1 against both `bg0` and `panel`; `onPrimary` on `primary` ≥ 4.5:1.
- No value (amount or fiat) rendered below 13 sp or in `textFaint`.
- The receive QR keeps its white background (`Screens.kt` ~line 893) — scanners need the light quiet zone.
- Build/test commands (this machine's `gradlew` script is broken; use the wrapper jar):
  `GW=/tmp/claude-1000/-home-donal-Work/936feca6-53d6-4fa8-bf65-de6808dbff19/scratchpad/gw.sh` then `$GW :app:testDebugUnitTest` / `$GW :app:assembleDebug`. Unit-test results: `android/app/build/test-results/testDebugUnitTest/*.xml`.

## Review Focus

1. **A large balance** (e.g. `20999999.99999999` BTC) at 36 sp mono is wider than the ~348 dp hero — the figure must stay fully visible: the unit wraps under the number rather than clipping or shrinking. (Task 4 Step 1 `FlowRow`; no real wallet holds such a balance, so the reviewer checks the layout code.)
2. **German (longest locale)** at the new fiat/meta sizes — labels wrap, nothing truncates mid-word or overlaps the Lock button. (Task 6 device pass.)
3. **Disabled primary button** must look disabled (40% alpha) — today `HwConnectScreen`'s Add looks live with no key. (Task 3.)
4. **The receive QR** must still scan after the restyle (white background kept, not `panel`). (Task 6: scan it with the phone's camera app / Google Lens.)
5. **Focus visibility** in `Field` and `MnemonicField` — the focused border is `signal`, the unfocused one `control` (≥ 3:1), so a user can see where they're typing. (Task 3.)

---

### Task 1: Contrast helper and graphite tokens

**Files:**
- Create: `android/app/src/main/kotlin/com/fortis/wallet/ui/theme/Contrast.kt`
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/theme/Theme.kt` (the `Fx` object and `DarkColors`)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/theme/ContrastTest.kt`

**Interfaces:**
- Produces: `fun contrastRatio(fg: Color, bg: Color): Double` (WCAG 2.x relative luminance, alpha ignored).
- Produces `Fx` tokens (all `Color` unless noted): `bg0`, `panel`, `panelRaised`, `line`, `control`, `text`, `textDim`, `textFaint`, `signal`, `good` (= `signal`), `warn`, `bad`, `primary`, `onPrimary`; `rSm`/`r`/`rLg` (`Dp`); `s1…s6` unchanged.
- Removes: `bg1`, `bg2`, `blobA/B/C`, `glass1`, `glass2`, `glassHero`, `hair`, `accent`, `accent2`, `pill`. (Compile errors at old call sites are expected until Tasks 3–5; keep this task's verification to the unit test.)

- [ ] **Step 1: Write the failing test** — `ContrastTest`:
  - `blackOnWhiteIs21`: `assertEquals(21.0, contrastRatio(Color.Black, Color.White), 0.01)`
  - `sameColourIs1`: `contrastRatio(Fx.text, Fx.text) == 1.0` (±0.001)
  - `textTokensMeetAA`: for each of `Fx.text, Fx.textDim, Fx.textFaint, Fx.signal, Fx.warn, Fx.bad` and each bg of `Fx.bg0, Fx.panel`: ratio ≥ 4.5, failure message naming the pair.
  - `controlBorderMeets3`: `Fx.control` vs `Fx.bg0` and `Fx.panel` ≥ 3.0.
  - `primaryButtonTextMeetsAA`: `contrastRatio(Fx.onPrimary, Fx.primary)` ≥ 4.5.
  - `pinnedValues`: `Fx.text == Color(0xFFE9ECEF)`, `Fx.bg0 == Color(0xFF0B0C0D)`, `Fx.panel == Color(0xFF111315)`, `Fx.signal == Color(0xFF3DDC97)` (guards against "fixing" a failing ratio by drifting from the spec).

- [ ] **Step 2: Run it — expect FAIL** (unresolved `contrastRatio`, `Fx.panel`, …). Because Task 1 breaks main-source compilation elsewhere, run the test step **after** Step 3 on a build where the old tokens are temporarily kept as `@Deprecated` aliases (`val glass1 get() = panel`, `hair → line`, `accent → signal`, `accent2 → signal`, `glass2 → panelRaised`, `glassHero → panel`, `pill = rSm`, `bg1/bg2 → bg0`, `blobA/B/C = Color.Transparent`) so the app still compiles; Task 5 deletes the aliases.
  Run: `$GW :app:testDebugUnitTest` → `ContrastTest` fails before Step 3's values are in.

- [ ] **Step 3: Implement** `contrastRatio` in `Contrast.kt` and the new `Fx` values from the spec's token table in `Theme.kt`; update `DarkColors` (`primary = Fx.signal`, `secondary = Fx.signal`, `background = Fx.bg0`, `surface = Fx.panel`, `outline = Fx.line`, `onPrimary = Fx.onPrimary`). Update `Theme.kt`'s doc comment to "Graphite signal — see /DESIGN.md".

- [ ] **Step 4: Run** `$GW :app:testDebugUnitTest` → all tests PASS (`ContrastTest` 6, plus the existing 7). `$GW :app:assembleDebug` → BUILD SUCCESSFUL (deprecation warnings expected).

- [ ] **Step 5: No commit** — report touched files.

### Task 2: Bundled fonts and the type scale

**Files:**
- Create: `android/app/src/main/res/font/jetbrains_mono_regular.ttf`, `jetbrains_mono_medium.ttf`, `inter_regular.ttf`, `inter_medium.ttf`, `inter_semibold.ttf`
- Create: `android/app/src/main/assets/licenses/JetBrainsMono-OFL.txt`, `android/app/src/main/assets/licenses/Inter-OFL.txt`
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/theme/Theme.kt` (`FortisType`, new `FxType`)
- Test: `android/app/src/test/kotlin/com/fortis/wallet/ui/theme/TypeScaleTest.kt`

**Interfaces:**
- Produces: `val Mono: FontFamily` (JetBrains Mono 400/500), `val Sans: FontFamily` (Inter 400/500/600).
- Produces `object FxType` with `TextStyle`s: `balance`, `fiat`, `amount`, `fiatSmall`, `title`, `body`, `meta`, `label`, `nav`, `button`, `brand` — sizes, weights, families, tracking and colours exactly as the spec's Type table. Mono styles set `fontFeatureSettings = "tnum"`. `label`, `nav`, `button` carry no text transform (Compose has none) — callers pass `.uppercase()`; document that on each.
- `FortisType` (Material `Typography`) maps `titleMedium → FxType.title`, `bodyMedium → FxType.body`, `bodySmall → FxType.meta`, `labelMedium → FxType.label`, `displayLarge → FxType.balance`, with `Sans` as the default family.

- [ ] **Step 1: Write the failing test** — `TypeScaleTest`:
  - `valueStylesAreAtLeast13sp`: `balance, fiat, amount, fiatSmall` each `fontSize.value >= 13f`.
  - `valueStylesAreMonoAndTabular`: those four have `fontFamily == Mono` and `fontFeatureSettings == "tnum"`.
  - `valueStylesNeverFaint`: those four have `color != Fx.textFaint`.
  - `pinnedSizes`: `balance` 36, `fiat` 17, `amount` 15, `fiatSmall` 13, `meta` 13, `label` 11, `button` 12 (sp).
- [ ] **Step 2: Run** `$GW :app:testDebugUnitTest` → FAIL (unresolved `FxType`).
- [ ] **Step 3: Fetch fonts** into the scratchpad (own empty dir per archive), copy `fonts/ttf/JetBrainsMono-{Regular,Medium}.ttf` and Inter's `extras/ttf/Inter-{Regular,Medium,SemiBold}.ttf` to the `res/font/` names above, and each archive's `OFL.txt` / `LICENSE.txt` to `assets/licenses/`. Then implement `Mono`, `Sans`, `FxType`, `FortisType`.
- [ ] **Step 4: Run** tests → PASS; `$GW :app:assembleDebug` → BUILD SUCCESSFUL; APK grows by ≤ 1 MB (`ls -l app/build/outputs/apk/debug/app-debug.apk`, compare to before).
- [ ] **Step 5: No commit.**

### Task 3: Components (`ui/Glass.kt`, `ui/MnemonicField.kt`)

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/Glass.kt`, `android/app/src/main/kotlin/com/fortis/wallet/ui/MnemonicField.kt`

**Interfaces:**
- Consumes: Task 1 tokens, Task 2 `FxType`, `Mono`.
- Produces (signatures unchanged except the one addition):
  - `AmbientBackground(modifier)` — flat `Fx.bg0`; no `rememberInfiniteTransition`.
  - `GlassCard(modifier, fill = Fx.panel, corner = Fx.r, hero: Boolean = false, content)` — 1 dp `Fx.line` border; `hero = true` draws a 1 dp `Fx.signal` line along the top edge.
  - `PrimaryButton(text, modifier, enabled, dense, onClick)` — 40 dp (dense 34 dp), `Fx.primary` fill, `Fx.rSm`, label `FxType.button` in `Fx.onPrimary`, uppercased; whole button `alpha(0.4f)` when `!enabled`.
  - `GhostButton(text, modifier, tint, dense, onClick)` — 40 dp (dense 34 dp), transparent, 1 dp `Fx.control` border, `Fx.rSm`, `FxType.button` uppercased.
  - `Field(...)` — container `Fx.panel`, unfocused border `Fx.control`, focused `Fx.signal`, shape `Fx.rSm`, mono fields use `Mono`, label `FxType.label` uppercased.
  - `ErrorText`, `Screen` — token renames only. `BrandMark` — wordmark in `FxType.brand`.
  - `MnemonicField` — chip shapes `Fx.rSm`, borders `Fx.control`, fills `Fx.panelRaised`.

- [ ] **Step 1: Implement** the above. (No JVM-testable logic; Task 1/2 tests guard the values these read.)
- [ ] **Step 2: Run** `$GW :app:testDebugUnitTest :app:assembleDebug` → all PASS / BUILD SUCCESSFUL, no new warnings from `Glass.kt` / `MnemonicField.kt`.
- [ ] **Step 3: Device check** — `adb install -r app/build/outputs/apk/debug/app-debug.apk`, launch `com.fortistechlabs.wallet.dev/com.fortis.wallet.MainActivity`, screenshot Onboard and Hardware connect: flat background, light-grey primary, outlined secondary, Add at 40% alpha with no key.
- [ ] **Step 4: No commit.**

### Task 4: Screen sweep (`Screens.kt`, `HwConnectScreen.kt`)

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/Screens.kt`, `android/app/src/main/kotlin/com/fortis/wallet/ui/screens/HwConnectScreen.kt`

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces: `@Composable private fun TextTabs(options: List<Pair<String, String>>, selected: String, onSelect: (String) -> Unit, modifier: Modifier = Modifier)` in `Screens.kt` — `FxType.nav` uppercased; active `Fx.text` + 1 dp `Fx.signal` underline, inactive `Fx.textDim`. Used by `NavBar`, `Segmented`, and the Wallet tab's Receive/Send/History switch (replacing the three pill implementations at ~lines 420, 476, 821).

- [ ] **Step 1: Replace styles**, mapping by role per the spec's Type table:
  - hero balance (~line 742) → `FxType.balance`; its unit → `FxType.label` in `Fx.signal`; put number + unit in a `FlowRow` so the unit wraps under a wide number (Review Focus 1).
  - every `fiat_approx` / `fmtUsd` text (~530, 602, 749, 977, 1089) → `FxType.fiat` in the hero, `FxType.fiatSmall` elsewhere.
  - amounts in Home/Settings lists, history rows and `ConfirmSheet` `kv` values → `FxType.amount`; incoming history amounts in `Fx.signal`.
  - hints / block height / "address #N" / timestamps → `FxType.meta`; only genuine instructional hints keep `Fx.textFaint`.
  - `c.display` and section labels → `FxType.label` uppercased.
  - the hero `GlassCard(fill = Fx.glassHero, corner = Fx.rLg)` → `GlassCard(hero = true)`.
  - Gen screen pad (~189) and progress bar (~212–215): `Fx.rLg`/`Fx.pill` → `Fx.r`/`Fx.rSm`; gradient → solid `Fx.signal`.
  - bottom sheets (`ConfirmSheet`, `XbtOfferSheet`): `containerColor = Fx.panel`.
  - remove every `Color(0xFF0A0C16)` and `Color.White.copy(...)`; keep the QR's `Color.White` background.
- [ ] **Step 2: Verify the sweep is complete:**
  `grep -nE 'fontSize *= *[0-9]|Color\(0x|Color\.White\.copy|Fx\.(glass1|glass2|glassHero|hair|accent2?|pill|bg1|bg2|blob)' android/app/src/main/kotlin/com/fortis/wallet/ui/screens/*.kt` → no output, except the one QR `Color.White` background line if matched.
- [ ] **Step 3: Run** `$GW :app:testDebugUnitTest :app:assembleDebug` → PASS / BUILD SUCCESSFUL.
- [ ] **Step 4: No commit.**

### Task 5: Logo — master files and Android launcher icon

**Files:**
- Create: `brand/fortis-mark.svg`, `brand/fortis-mark-light.svg`, `brand/fortis-mark-lockup.svg`, `brand/fortis-mark-small-32.svg`, `brand/fortis-mark-small-16.svg`, `brand/fortis-wordmark.svg`, `brand/README.md` (which file for which size/background; colours)
- Create: `android/app/src/main/res/drawable/ic_mark.xml`
- Modify: `android/app/src/main/res/drawable/ic_launcher_background.xml`, `ic_launcher_foreground.xml` (`ic_launcher.xml` unchanged — it already wires background/foreground/monochrome)
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/Glass.kt` (`BrandMark` → `painterResource(R.drawable.ic_mark)`)

**Interfaces:**
- Produces: the `brand/` files (consumed later by the web-wallet and site specs) and `R.drawable.ic_mark`.

- [ ] **Step 1:** Write the five mark SVGs from the spec's geometry table (64 × 64 `viewBox`, colours per variant). For `fortis-wordmark.svg`, convert "fortis" in `res/font/jetbrains_mono_medium.ttf` to outlines (e.g. `fonttools`' pen API in a scratchpad script run with `python3 -I`) so the SVG embeds no font.
- [ ] **Step 2:** Verify the SVGs render: `rsvg-convert -w 512 brand/fortis-mark.svg -o $SCRATCH/mark-512.png` (and `-w 32`/`-w 16` for the small ones; install `librsvg` only if absent and the owner agrees) and view the PNGs — no clipping at the 64 grid edge, keyhole centred.
- [ ] **Step 3:** Launcher: background = solid `#111315`; foreground = the `fortis-mark` paths in a `<group>` translated/scaled so the shield's 42 × 49.5 bounding box (x 11–53, y 7–56.5) sits centred inside the 66 dp safe zone of the 108 dp canvas (scale ≈ 1.2, keeping stroke 2.2 in mark units). `ic_mark.xml`: the same paths on a 64 × 64 viewport, transparent.
- [ ] **Step 4:** `$GW :app:assembleDebug` → BUILD SUCCESSFUL; install the dev build; screenshot the launcher (home screen / app drawer) and Onboard; with the phone's themed icons on, the monochrome icon shows the shield + keyhole.
- [ ] **Step 5: No commit.**

### Task 6: Remove the deprecated aliases; DESIGN.md and CHANGELOG

**Files:**
- Modify: `android/app/src/main/kotlin/com/fortis/wallet/ui/theme/Theme.kt` (delete Task 1's `@Deprecated` aliases)
- Modify: `DESIGN.md` (rewrite), `CHANGELOG.md` (Unreleased), `android/README.md` (layout table: `ui/theme/Theme.kt · ui/Glass.kt` row → "the Graphite signal tokens, type scale (`FxType`) + components")

- [ ] **Step 1: Delete the aliases**; `$GW :app:assembleDebug` → BUILD SUCCESSFUL with no deprecation warnings from `com.fortis.wallet` sources (any remaining use is a Task 4 miss — fix it there).
- [ ] **Step 2: Rewrite `DESIGN.md`** as "Graphite signal": principles (flat, quiet, one signal accent, mono figures, contrast over size), the token table with ratios, the type table, component rules, the logo (pointing at `brand/README.md`), and a note that `web/` and `site/` still use the previous "Glassy & layered" language and logo until their own restyles.
- [ ] **Step 3: CHANGELOG** Unreleased entry, user-facing: "New look: a darker, flatter graphite design with much higher-contrast text, larger amounts and dollar values, a cleaner typeface for numbers and addresses, and a new app icon."
- [ ] **Step 4: Run** `$GW :app:testDebugUnitTest :app:assembleDebug` and `cd /home/donal/Work/fortis && cargo test --workspace` → all green.
- [ ] **Step 5: No commit.**

### Task 7: Device verification

**Files:** none (screenshots to the scratchpad).

- [ ] **Step 1:** Install the dev build (`adb install -r …/app-debug.apk`) — never touch `com.fortistechlabs.wallet`.
- [ ] **Step 2:** Screenshot and review: launcher icon (normal and themed), Onboard, Gen, Create, Restore, Watch, Hardware connect, Home, Wallet → Receive, Wallet → History, Settings, XBT offer sheet (connect a hardware key again to trigger it), App lock (Settings → lock, if a lock is configured). Check against the spec: flat graphite, no blobs, green only on signal uses, no value below 13 sp, nothing faint that is a value.
- [ ] **Step 3:** Settings → language → Deutsch; repeat Home, Wallet, Settings, Hardware connect; check wrapping/truncation (Review Focus 2). Switch back to English.
- [ ] **Step 4:** Ask the owner to scan the dev app's receive QR with the phone's camera app or Google Lens from another device; the decoded text must equal the shown address (Review Focus 4).
- [ ] **Step 5:** Report the screenshots to the owner with any deviations found; fix in the owning task before sign-off.
