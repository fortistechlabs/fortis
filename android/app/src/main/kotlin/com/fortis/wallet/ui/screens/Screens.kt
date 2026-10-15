package com.fortis.wallet.ui.screens

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.tween
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowLeft
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.ArrowDropDown
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.material.icons.filled.Settings
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.foundation.text.TextAutoSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import com.fortis.wallet.BuildConfig
import com.fortis.wallet.MAX_WALLETS
import com.fortis.wallet.MAX_WALLET_NAME
import com.fortis.wallet.NavTab
import com.fortis.wallet.PlanPreview
import com.fortis.wallet.R
import com.fortis.wallet.SwapRoute
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.ui.*
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import com.fortis.wallet.ui.theme.caps
import com.fortis.wallet.ui.theme.Mono
import com.fortis.wallet.wallet.EntropyCollector
import com.fortis.wallet.wallet.entropyProgress
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private fun fmt(sat: Long) = fmtCoin(sat)

/** Ease the displayed satoshi value toward [sat] whenever it changes — 0 → balance
 *  on first load, then old → new on each refresh. ~650 ms, decelerating. */
@Composable
private fun countUpSat(sat: Long): Long {
    val progress = remember { Animatable(0f) }
    var base by remember { mutableStateOf(0L) }
    var goal by remember { mutableStateOf(sat) }
    fun shown() = base + ((goal - base) * progress.value.toDouble()).toLong()
    LaunchedEffect(sat) {
        base = shown()
        goal = sat
        progress.snapTo(0f)
        progress.animateTo(1f, tween(650, easing = FastOutSlowInEasing))
    }
    return shown()
}

/** Parse what the user typed in the Amount field into satoshis.
 *  [sat] true → a plain integer number of sats; false → a decimal coin amount
 *  (BTC / XBT). Returns null for anything unparseable or negative. */
internal fun amountToSat(text: String, sat: Boolean): Long? = runCatching {
    val t = text.trim().replace(",", "").replace("_", "").replace(" ", "")
    when {
        t.isEmpty() -> null
        sat -> t.toLong().takeIf { it >= 0 }
        else -> t.toBigDecimal()
            .movePointRight(8)
            .setScale(0, java.math.RoundingMode.DOWN)
            .longValueExact()
            .takeIf { it >= 0 }
    }
}.getOrNull()

internal fun copyToClipboard(ctx: Context, label: String, text: String) {
    val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    // Marked sensitive (ClipDescription.EXTRA_IS_SENSITIVE, by its key so older
    // Androids get it too): kept out of clipboard previews and keyboard
    // clipboard history — an xpub reveals the whole wallet's history.
    val clip = ClipData.newPlainText(label, text).apply {
        description.extras = android.os.PersistableBundle().apply {
            putBoolean("android.content.extra.IS_SENSITIVE", true)
        }
    }
    cm.setPrimaryClip(clip)
    Toast.makeText(ctx, ctx.getString(R.string.toast_copied), Toast.LENGTH_SHORT).show()
}

/** Put [bitmap] on the clipboard as a PNG so it can be pasted into a chat, email, etc.
 *  Goes through a FileProvider content URI under cacheDir/shared/. */
private fun copyImageToClipboard(ctx: Context, bitmap: Bitmap) {
    runCatching {
        val file = File(ctx.cacheDir, "shared").apply { mkdirs() }.resolve("qr.png")
        file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.fileprovider", file)
        val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        cm.setPrimaryClip(
            ClipData(
                ClipDescription(ctx.getString(R.string.cd_address_qr), arrayOf("image/png")),
                ClipData.Item(uri),
            ),
        )
        Toast.makeText(ctx, ctx.getString(R.string.toast_copied), Toast.LENGTH_SHORT).show()
    }
}

@Composable
fun OnboardScreen(vm: WalletViewModel) = Screen {
    val adding = vm.wallets.isNotEmpty()
    Spacer(Modifier.weight(1f))
    if (adding) Text(stringResource(R.string.onboard_add_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
    else BrandMark(stringResource(R.string.onboard_tagline))
    Spacer(Modifier.weight(1f))
    PrimaryButton(stringResource(R.string.onboard_create)) { vm.goCreate() }
    GhostButton(stringResource(R.string.onboard_restore)) { vm.goRestore() }
    GhostButton(stringResource(R.string.onboard_hardware)) { vm.goHwConnect() }
    GhostButton(stringResource(R.string.onboard_watch)) { vm.goWatch() }
    if (adding) GhostButton(stringResource(R.string.action_cancel), tint = Fx.textDim) { vm.cancelOnboard() }
    Spacer(Modifier.weight(1f))
}

@Composable
fun GenScreen(vm: WalletViewModel) {
    val ctx = LocalContext.current
    val collector = remember { EntropyCollector() }
    var bits by remember { mutableStateOf(0) }
    var words by remember { mutableStateOf(24) }

    // Motion-sensor noise while the user shakes the phone — the novel source.
    DisposableEffect(Unit) {
        val sm = ctx.getSystemService(Context.SENSOR_SERVICE) as SensorManager
        val listener = object : SensorEventListener {
            override fun onSensorChanged(e: SensorEvent) {
                collector.addMotion(e.values)
                bits = collector.bits
            }
            override fun onAccuracyChanged(s: Sensor?, a: Int) {}
        }
        listOf(Sensor.TYPE_ACCELEROMETER, Sensor.TYPE_GYROSCOPE).forEach { t ->
            sm.getDefaultSensor(t)?.let { sm.registerListener(listener, it, SensorManager.SENSOR_DELAY_GAME) }
        }
        onDispose { sm.unregisterListener(listener) }
    }
    // Passive timing jitter — no interaction needed.
    LaunchedEffect(Unit) {
        withContext(Dispatchers.Default) { collector.addJitter() }
        bits = collector.bits
    }

    Screen(scroll = true) {
        Text(stringResource(R.string.gen_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
        Text(stringResource(R.string.gen_body), color = Fx.textDim)
        Segmented(
            listOf("24" to stringResource(R.string.gen_words_24), "12" to stringResource(R.string.gen_words_12)),
            words.toString(), { words = it.toInt() },
        )
        Text(stringResource(R.string.gen_words_hint), style = FxType.meta, color = Fx.textFaint)
        val scribble = remember { Path() }
        var strokeRev by remember { mutableIntStateOf(0) }
        Box(
            Modifier.fillMaxWidth().height(150.dp).clip(RoundedCornerShape(Fx.r)).background(Fx.panel).border(1.dp, Fx.line, RoundedCornerShape(Fx.r))
                .pointerInput(Unit) {
                    detectDragGestures(
                        onDragStart = { scribble.moveTo(it.x, it.y); strokeRev++ },
                    ) { change, _ ->
                        scribble.lineTo(change.position.x, change.position.y)
                        strokeRev++
                        collector.addTouch(change.position.x, change.position.y, System.nanoTime())
                        bits = collector.bits
                    }
                }
                .drawWithContent {
                    drawContent()
                    strokeRev // subscribe: Path mutations aren't observable on their own
                    drawPath(
                        scribble, Fx.signal,
                        style = Stroke(width = 3.5.dp.toPx(), cap = StrokeCap.Round),
                    )
                },
            contentAlignment = Alignment.Center,
        ) {
            if (strokeRev == 0) Text(stringResource(R.string.gen_pad_hint), style = FxType.meta, color = Fx.textFaint)
        }
        Box(Modifier.fillMaxWidth().height(4.dp).clip(RoundedCornerShape(Fx.rSm)).background(Fx.panelRaised)) {
            Box(
                Modifier.fillMaxWidth(entropyProgress(bits)).fillMaxHeight()
                    .background(Fx.signal),
            )
        }
        Text(
            if (entropyProgress(bits) >= 1f) stringResource(R.string.gen_entropy_full)
            else stringResource(R.string.gen_entropy_partial, bits),
            style = FxType.meta, color = Fx.textFaint,
        )
        ErrorText(vm.error)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_back), Modifier.weight(1f)) { vm.cancelOnboard() }
            PrimaryButton(stringResource(R.string.gen_generate), Modifier.weight(1f)) { vm.generateSeed(collector.bytes(), words) }
        }
    }
}

@Composable
fun CreateScreen(vm: WalletViewModel) {
    val ctx = LocalContext.current
    var name by remember { mutableStateOf("") }
    var chain by remember { mutableStateOf("xbt") }
    var passphrase by remember { mutableStateOf("") }
    var ack by remember { mutableStateOf(false) }
    val words = (vm.draftMnemonic ?: "").split(" ")
    val settingUp = vm.settingUp
    val choice = rememberLockChoice()
    val act = rememberFragmentActivity()
    val scope = rememberCoroutineScope()
    val lockFailed = stringResource(R.string.error_lock_setup_failed)

    Screen(scroll = true) {
        Text(stringResource(R.string.create_phrase_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
        Text(stringResource(R.string.create_phrase_body, words.size), color = Fx.textDim)
        GlassCard { PhraseGrid(words) }
        Field(name, { name = it }, stringResource(R.string.field_wallet_name), maxLen = MAX_WALLET_NAME)
        ChainRow(chain) { chain = it }
        Field(passphrase, { passphrase = it }, stringResource(R.string.field_passphrase), password = true)
        Text(stringResource(R.string.create_passphrase_hint), style = FxType.meta, color = Fx.textFaint)
        if (settingUp) LockChoiceFields(choice)
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(ack, { ack = it })
            Text(stringResource(R.string.create_ack), color = Fx.text)
        }
        ErrorText(vm.error)
        val confirmPhrase = stringResource(R.string.error_confirm_phrase)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_back), Modifier.weight(1f)) { vm.cancelOnboard() }
            PrimaryButton(stringResource(R.string.action_continue), Modifier.weight(1f)) {
                val problem = when {
                    !ack -> confirmPhrase
                    settingUp -> choice.problem(ctx)
                    else -> null
                }
                if (problem != null) {
                    vm.error = problem
                    Toast.makeText(ctx, problem, Toast.LENGTH_SHORT).show()
                } else {
                    vm.error = null
                    scope.launch {
                        val lock = if (settingUp) {
                            runCatching { choice.resolve(act) }
                                .getOrElse { vm.error = it.message ?: lockFailed; null } ?: return@launch
                        } else null
                        vm.createWallet(name, chain, "mainnet", passphrase, lock)
                    }
                }
            }
        }
    }
}

@Composable
fun RestoreScreen(vm: WalletViewModel) {
    val ctx = LocalContext.current
    var name by remember { mutableStateOf("") }
    var phrase by remember { mutableStateOf("") }
    var passphrase by remember { mutableStateOf("") }
    var chain by remember { mutableStateOf("xbt") }
    val settingUp = vm.settingUp
    val choice = rememberLockChoice()
    val act = rememberFragmentActivity()
    val scope = rememberCoroutineScope()
    val lockFailed = stringResource(R.string.error_lock_setup_failed)
    Screen(scroll = true) {
        Text(stringResource(R.string.restore_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
        Text(stringResource(R.string.restore_body), color = Fx.textDim)
        Field(name, { name = it }, stringResource(R.string.field_wallet_name), maxLen = MAX_WALLET_NAME)
        MnemonicField(phrase, { phrase = it })
        Field(passphrase, { passphrase = it }, stringResource(R.string.field_passphrase), password = true)
        ChainRow(chain) { chain = it }
        Text(
            stringResource(R.string.restore_clone_hint, if (chain == "btc") "XBT" else "BTC"),
            style = FxType.meta, color = Fx.textFaint,
        )
        if (settingUp) LockChoiceFields(choice)
        ErrorText(vm.error)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_back), Modifier.weight(1f)) { vm.cancelOnboard() }
            PrimaryButton(stringResource(R.string.action_restore), Modifier.weight(1f)) {
                val problem = when {
                    phrase.isBlank() -> ctx.getString(R.string.error_enter_phrase)
                    settingUp -> choice.problem(ctx)
                    else -> null
                }
                if (problem != null) {
                    vm.error = problem
                    Toast.makeText(ctx, problem, Toast.LENGTH_SHORT).show()
                } else {
                    vm.error = null
                    scope.launch {
                        val lock = if (settingUp) {
                            runCatching { choice.resolve(act) }
                                .getOrElse { vm.error = it.message ?: lockFailed; null } ?: return@launch
                        } else null
                        vm.restoreWallet(name, phrase, passphrase, chain, "mainnet", lock)
                    }
                }
            }
        }
    }
}

@Composable
fun WatchScreen(vm: WalletViewModel) {
    var name by remember { mutableStateOf("") }
    var xpub by remember { mutableStateOf("") }
    var chain by remember { mutableStateOf("xbt") }
    val scope = rememberCoroutineScope()
    Screen(scroll = true) {
        Text(stringResource(R.string.watch_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
        Text(stringResource(R.string.watch_body), color = Fx.textDim)
        Field(name, { name = it }, stringResource(R.string.field_wallet_name), maxLen = MAX_WALLET_NAME)
        Field(xpub, { xpub = it }, stringResource(R.string.field_xpub), mono = true)
        ChainRow(chain) { chain = it }
        ErrorText(vm.error)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_back), Modifier.weight(1f)) { vm.cancelOnboard() }
            PrimaryButton(stringResource(R.string.action_watch), Modifier.weight(1f)) {
                scope.launch { vm.watchWallet(name, xpub, chain) }
            }
        }
    }
}

@Composable
private fun ChainRow(chain: String, onChain: (String) -> Unit) {
    Segmented(listOf("xbt" to "XBT", "btc" to "BTC"), chain, onChain)
}

/** Numbered 3-column grid of mnemonic words. Caller supplies the surrounding card. */
@Composable
fun PhraseGrid(words: List<String>) = Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
    words.chunked(3).forEachIndexed { row, three ->
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            three.forEachIndexed { col, w ->
                Text(
                    "${row * 3 + col + 1}  $w", style = FxType.phrase,
                    modifier = Modifier.weight(1f).background(Fx.panelRaised, RoundedCornerShape(Fx.rSm)).padding(6.dp),
                )
            }
        }
    }
}

/** Screenshot-blocked view of one wallet's recovery phrase + passphrase. */
@Composable
fun RevealSeedScreen(vm: WalletViewModel) {
    val id = vm.revealingId ?: return
    val w = vm.wallets.firstOrNull { it.id == id }
    var seed by remember(id) { mutableStateOf<Pair<List<String>, String>?>(null) }
    var failed by remember(id) { mutableStateOf(false) }
    LaunchedEffect(id) {
        seed = runCatching { vm.seedFor(id) }.getOrNull()
        if (seed == null) failed = true
    }
    Screen(scroll = true) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            IconButton({ vm.closeReveal() }) {
                Icon(Icons.AutoMirrored.Filled.KeyboardArrowLeft, stringResource(R.string.action_back), tint = Fx.text)
            }
            Text(stringResource(R.string.reveal_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
        }
        if (w != null) Text(w.display, color = Fx.textDim)
        Text(stringResource(R.string.reveal_warning), style = FxType.meta, color = Fx.bad)
        when {
            failed -> Text(stringResource(R.string.reveal_failed), color = Fx.bad)
            seed == null -> CircularProgressIndicator(Modifier.align(Alignment.CenterHorizontally))
            else -> {
                val (words, passphrase) = seed!!
                GlassCard { PhraseGrid(words) }
                if (passphrase.isNotEmpty()) {
                    Text(caps(stringResource(R.string.reveal_passphrase_label)), style = FxType.label)
                    Text(
                        passphrase, color = Fx.text, fontFamily = Mono,
                        modifier = Modifier.fillMaxWidth().clip(RoundedCornerShape(Fx.rSm)).background(Fx.panelRaised).padding(Fx.s4),
                    )
                } else {
                    Text(stringResource(R.string.reveal_no_passphrase), style = FxType.meta, color = Fx.textFaint)
                }
            }
        }
        PrimaryButton(stringResource(R.string.action_done)) { vm.closeReveal() }
    }
}

@Composable
fun Segmented(options: List<Pair<String, String>>, selected: String, onSelect: (String) -> Unit, modifier: Modifier = Modifier) =
    TextTabs(options, selected, onSelect, modifier)

/** Folder tabs over a 1 dp rule: the active one in the page colour with a
 *  [Fx.signal] top edge, the rest slate keys. Nav bar, wallet tabs and pickers. */
@Composable
private fun <T> TextTabs(
    options: List<Pair<T, String>>,
    selected: T,
    onSelect: (T) -> Unit,
    modifier: Modifier = Modifier,
    icons: Map<T, ImageVector> = emptyMap(),
) {
    // Folder tabs on a baseline: the selected tab is raised, outlined and open at the
    // bottom, with a signal edge on top; the others sit back, darker.
    Row(
        modifier.fillMaxWidth().drawBehind {
            drawLine(Fx.control, Offset(0f, size.height - 0.5f), Offset(size.width, size.height - 0.5f), 1.dp.toPx())
        },
        horizontalArrangement = Arrangement.spacedBy(Fx.s1),
    ) {
        options.forEach { (v, label) ->
            val on = v == selected
            val icon = icons[v]
            Box(
                // Fixed height: a label that auto-shrinks must not make its tab shorter.
                // An icon tab (Settings' cog) is a narrow square-ish key at the end.
                (if (icon != null) Modifier.width(52.dp) else Modifier.weight(1f))
                    .height(40.dp)
                    .drawBehind { drawTab(on) }
                    .clickable { onSelect(v) },
                contentAlignment = Alignment.Center,
            ) {
                if (icon != null) {
                    Icon(icon, contentDescription = label, tint = if (on) Fx.text else Fx.dimOnSlate, modifier = Modifier.size(22.dp))
                } else {
                    // One line always: long translations (EINSTELLUNGEN) shrink to fit rather than break mid-word.
                    Text(
                        caps(label), style = FxType.nav, color = if (on) Fx.text else Fx.dimOnSlate,
                        maxLines = 1, softWrap = false,
                        autoSize = TextAutoSize.StepBased(minFontSize = 9.sp, maxFontSize = FxType.nav.fontSize, stepSize = 0.5.sp),
                        modifier = Modifier.padding(horizontal = Fx.s1),
                    )
                }
            }
        }
    }
}

private fun DrawScope.drawTab(on: Boolean) {
    val sw = 1.dp.toPx()
    val h = sw / 2
    val r = Fx.rTab.toPx()
    val w = size.width
    val bottom = size.height
    val outline = Path().apply {
        moveTo(h, bottom)
        lineTo(h, r + h)
        arcTo(Rect(h, h, h + 2 * r, h + 2 * r), 180f, 90f, false)
        lineTo(w - h - r, h)
        arcTo(Rect(w - h - 2 * r, h, w - h, h + 2 * r), 270f, 90f, false)
        lineTo(w - h, bottom)
    }
    // The fill covers the row's baseline, so the selected tab opens into the content below.
    // Selected: the page colour, open into the content. Others: slate keys, like the
    // secondary buttons, so they read as something to press.
    val fill = Path().apply { addPath(outline); close() }
    if (on) drawPath(fill, Fx.panelRaised)
    else drawPath(fill, Brush.verticalGradient(listOf(Fx.ghostTop, Fx.ghostBottom)))
    drawPath(outline, if (on) Fx.control else Fx.ghostLip, style = Stroke(sw))
    if (on) {
        drawLine(Fx.signal, Offset(r, h + sw), Offset(w - r, h + sw), 2.dp.toPx())
    } else {
        drawLine(Fx.control, Offset(0f, bottom - h), Offset(w, bottom - h), sw)
    }
}

/**
 * The unlocked app: a persistent top nav bar (Home · Wallet · Settings) over the
 * three tab bodies.
 */
@Composable
fun Shell(vm: WalletViewModel) {
    val unit = if (vm.config?.chain == "btc") "BTC" else "XBT"
    LaunchedEffect(vm.nav, vm.wallets.map { it.id }) {
        while (vm.nav == NavTab.Home || vm.nav == NavTab.Settings || vm.nav == NavTab.Swaps) {
            vm.refreshAllBalances()
            vm.refreshQuote()
            vm.refreshSwaps()
            kotlinx.coroutines.delay(45_000)
        }
    }
    Box(Modifier.fillMaxSize()) {
        AmbientBackground()
        Column(
            Modifier.fillMaxSize().widthIn(max = 460.dp).align(Alignment.TopCenter)
                .systemBarsPadding().imePadding().padding(horizontal = Fx.s4).padding(top = Fx.s3),
            verticalArrangement = Arrangement.spacedBy(Fx.s3),
        ) {
            // A swap screen counts as the Swaps tab, wherever it was opened from.
            NavBar(if (vm.swapRoute != null) NavTab.Swaps else vm.nav, vm.showSwap || vm.nav == NavTab.Swaps) { vm.go(it) }
            Box(Modifier.weight(1f).fillMaxWidth()) {
                // A swap screen opens over the tabs (Home → Swap); a tab tap or Back closes it.
                when (val route = vm.swapRoute) {
                    SwapRoute.List -> SwapListScreen(vm)
                    SwapRoute.New -> NewSwapScreen(vm)
                    SwapRoute.Take -> TakeCodeScreen(vm)
                    is SwapRoute.TakeListed -> TakeOfferScreen(vm, route.entry)
                    is SwapRoute.Detail -> SwapDetailScreen(vm, route.id)
                    null -> when (vm.nav) {
                        NavTab.Home -> HomeTab(vm)
                        NavTab.Wallet -> WalletTab(vm)
                        NavTab.Settings -> SettingsTab(vm)
                        NavTab.Swaps -> SwapListScreen(vm)
                    }
                }
            }
        }
    }
    vm.pending?.let { ConfirmSheet(vm, it, unit) }
    if (vm.hwOfferXbtFor != null) XbtOfferSheet(vm)
}

/** Home · Wallet · Swaps · ⚙. Swaps shows only while the edge offers swaps (or
 *  one is running); Settings is a cog, labelled for screen readers. */
@Composable
private fun NavBar(current: NavTab, showSwaps: Boolean, onSelect: (NavTab) -> Unit) {
    val labels = listOfNotNull(
        NavTab.Home to stringResource(R.string.nav_home),
        NavTab.Wallet to stringResource(R.string.nav_wallet),
        (NavTab.Swaps to stringResource(R.string.nav_swaps)).takeIf { showSwaps },
        NavTab.Settings to stringResource(R.string.nav_settings),
    )
    TextTabs(labels, current, onSelect, icons = mapOf(NavTab.Settings to Icons.Filled.Settings))
}

/** Home tab — the list of wallets; tap one to open it. */
@Composable
private fun HomeTab(vm: WalletViewModel) = Column(
    Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = Fx.s2),
    verticalArrangement = Arrangement.spacedBy(Fx.s4),
) {
    // Running swaps first, while there are any.
    ActiveSwapsSection(vm.swaps, vm::openSwap)
    Text(stringResource(R.string.home_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
    // Each wallet is its own key — pressable like a button, not a line in a list.
    Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
        vm.wallets.forEach { w ->
            val wUnit = if (w.chain == "btc") "BTC" else "XBT"
            val bal = vm.walletBalances[w.id]
            PressableTile(selected = w.id == vm.selectedId, onClick = { vm.selectWallet(w.id) }) {
                Column(Modifier.weight(1f)) {
                    Text(w.display, color = Fx.text, fontWeight = FontWeight.Medium)
                    Text(
                        bal?.let { "${fmt(it)} $wUnit" } ?: stringResource(R.string.home_tap_to_open),
                        style = if (bal != null) FxType.amount else FxType.meta,
                        color = if (bal != null) Fx.text else Fx.dimOnSlate,
                    )
                    bal?.let { vm.usdValue(it, w.chain) }?.let {
                        Text(stringResource(R.string.fiat_approx, fmtUsd(it)), style = FxType.fiatSmall, color = Fx.dimOnSlate)
                    }
                    // Incoming (or outgoing) coins not yet confirmed — as on the wallet screen.
                    vm.walletPending[w.id]?.takeIf { it != 0L }?.let { p ->
                        Text(
                            stringResource(R.string.wallet_pending, (if (p > 0) "+" else "") + fmt(p), wUnit),
                            style = FxType.fiatSmall, color = Fx.warn,
                        )
                    }
                    // Its part in any running swap (why the balance moved).
                    walletSwapLines(w.id, vm.swaps).forEach { WalletSwapText(it, vm::openSwap) }
                }
            }
        }
    }
    // Only when the edge quotes swaps (it answers GET /swap/quote) — or a swap is still running.
    if (vm.canAddWallet) PrimaryButton(stringResource(R.string.action_add_wallet)) { vm.addWallet() }
    else Text(stringResource(R.string.wallets_max, MAX_WALLETS), style = FxType.meta, color = Fx.textFaint)
    ErrorText(vm.error)
}

/** The one gate into the app — fingerprint / device PIN, or a password. */
@Composable
fun AppLockScreen(vm: WalletViewModel) {
    var pw by remember { mutableStateOf("") }
    Screen {
        Spacer(Modifier.weight(1f))
        BrandMark()
        GlassCard {
            if (vm.lockMode == com.fortis.wallet.data.LOCK_BIOMETRIC) {
                BiometricAppUnlock(vm)
            } else {
                Text(stringResource(R.string.lock_password_prompt), color = Fx.textDim)
                Field(pw, { pw = it }, stringResource(R.string.field_app_password), password = true)
                ErrorText(vm.error)
                PrimaryButton(stringResource(R.string.action_unlock), enabled = pw.isNotEmpty()) { vm.appUnlockWithPassword(pw) }
            }
        }
        Spacer(Modifier.weight(1f))
    }
}

/** Settings tab — manage wallets, security, connection. */
@Composable
private fun SettingsTab(vm: WalletViewModel) {
    val ctx = LocalContext.current
    val gate = com.fortis.wallet.ui.rememberIdentityGate()
    val gateAct = com.fortis.wallet.ui.rememberFragmentActivity()
    val gateScope = rememberCoroutineScope()
    com.fortis.wallet.ui.ConfirmIdentityDialog(gate, vm)
    val st = vm.status
    var renaming by remember { mutableStateOf<String?>(null) }
    var removing by remember { mutableStateOf<String?>(null) }
    var revealWarn by remember { mutableStateOf<String?>(null) }
    val xpubLoading = stringResource(R.string.xpub_loading)

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = Fx.s2),
        verticalArrangement = Arrangement.spacedBy(Fx.s4),
    ) {
        Text(stringResource(R.string.settings_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)

        GlassCard {
            Text(stringResource(R.string.settings_wallets), color = Fx.text, fontWeight = FontWeight.SemiBold)
            vm.wallets.forEach { w ->
                val current = w.id == vm.selectedId
                val wUnit = if (w.chain == "btc") "BTC" else "XBT"
                val bal = vm.walletBalances[w.id]
                Column(
                    Modifier.fillMaxWidth().clip(RoundedCornerShape(Fx.rSm))
                        .background(if (current) Fx.panelRaised else Color.Transparent).padding(Fx.s3),
                    verticalArrangement = Arrangement.spacedBy(Fx.s2),
                ) {
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text(w.display, color = Fx.text, fontWeight = FontWeight.Medium)
                            Text(
                                bal?.let { "${fmt(it)} $wUnit" }
                                    ?: listOfNotNull(w.network.takeIf { it != "mainnet" }, if (current) stringResource(R.string.home_open) else null)
                                        .joinToString(" · ").ifEmpty { "…" },
                                style = if (bal != null) FxType.amount else FxType.meta,
                                color = if (bal != null) Fx.text else Fx.textFaint,
                            )
                            bal?.let { vm.usdValue(it, w.chain) }?.let {
                                Text(stringResource(R.string.fiat_approx, fmtUsd(it)), style = FxType.fiatSmall)
                            }
                        }
                        if (!current) TextButton({ vm.selectWallet(w.id) }) { Text(stringResource(R.string.action_open), color = Fx.signal) }
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                        GhostButton(stringResource(R.string.action_rename), Modifier.weight(1f), dense = true) { renaming = w.id }
                        GhostButton(stringResource(R.string.action_copy_xpub), Modifier.weight(1f), dense = true) {
                            val xpub = vm.accountKey(w.id)
                            if (xpub != null) copyToClipboard(ctx, "xpub", xpub)
                            else Toast.makeText(ctx, xpubLoading, Toast.LENGTH_SHORT).show()
                        }
                    }
                    if (!w.watchOnly) GhostButton(stringResource(R.string.action_recovery_phrase), dense = true) { revealWarn = w.id }
                    val hasOther = vm.wallets.any { it.name == w.name && it.chain == w.otherChain }
                    if (!hasOther && vm.canAddWallet) GhostButton(stringResource(R.string.also_add_on, w.otherChain.uppercase()), dense = true) {
                        vm.cloneToOtherChain(w.id)
                    }
                    GhostButton(stringResource(R.string.action_remove), tint = Fx.bad, dense = true) { removing = w.id }
                }
            }
            if (vm.canAddWallet) PrimaryButton(stringResource(R.string.action_add_wallet), dense = true) { vm.addWallet() }
            else Text(stringResource(R.string.wallets_max, MAX_WALLETS), style = FxType.meta, color = Fx.textFaint)
            ErrorText(vm.error)
        }

        GlassCard {
            Text(stringResource(R.string.settings_security), color = Fx.text, fontWeight = FontWeight.SemiBold)
            kv(
                stringResource(R.string.settings_app_lock),
                if (vm.lockMode == com.fortis.wallet.data.LOCK_BIOMETRIC) stringResource(R.string.lock_mode_biometric)
                else stringResource(R.string.lock_mode_password),
            )
            Text(stringResource(R.string.settings_one_unlock), style = FxType.meta, color = Fx.textFaint)
            val keySec = remember(vm.lockMode) { vm.keySecurity() }
            if (keySec == com.fortis.wallet.data.SeedKeystore.KeySecurity.SOFTWARE) {
                Text(stringResource(R.string.settings_key_software), style = FxType.meta, color = Fx.warn)
            }
        }

        if (vm.seedWallets.isNotEmpty()) GlassCard {
            Text(stringResource(R.string.swap_recover_title), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(stringResource(R.string.swap_recover_body), style = FxType.meta, color = Fx.textFaint)
            GhostButton(
                stringResource(if (vm.recovering) R.string.swap_recover_searching else R.string.swap_recover_action),
                enabled = !vm.recovering, dense = true,
            ) { vm.recoverSwaps() }
            vm.recoverNote?.let { Text(it, style = FxType.meta, color = Fx.text) }
        }

        LanguageCard()

        GlassCard {
            Text(stringResource(R.string.settings_connection), color = Fx.text, fontWeight = FontWeight.SemiBold)
            kv(
                stringResource(R.string.settings_status),
                when {
                    st == null -> stringResource(R.string.status_offline)
                    st.scanningPct != null -> stringResource(R.string.status_rescanning, st.scanningPct)
                    st.degraded -> stringResource(R.string.status_limited)
                    st.synced -> stringResource(R.string.status_connected)
                    else -> stringResource(R.string.status_syncing)
                },
            )
            kv(stringResource(R.string.settings_chain_height), st?.blocks?.toString() ?: "—")
            if (st?.degraded == true) Text(stringResource(R.string.settings_degraded_note), style = FxType.meta, color = Fx.textFaint)
            Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                GhostButton(stringResource(R.string.action_refresh), Modifier.weight(1f), dense = true) { vm.refresh() }
                GhostButton(stringResource(R.string.action_reconnect), Modifier.weight(1f), dense = true) { vm.reconnect() }
            }
        }

        GhostButton(stringResource(R.string.action_lock_app), tint = Fx.bad, dense = true) { vm.lock() }
        Text(stringResource(R.string.settings_version, BuildConfig.VERSION_NAME), style = FxType.meta, color = Fx.textFaint)
    }

    renaming?.let { id ->
        val w = vm.wallets.firstOrNull { it.id == id }
        var text by remember(id) { mutableStateOf(w?.name ?: "") }
        AlertDialog(
            onDismissRequest = { renaming = null },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.dialog_rename_title), color = Fx.text) },
            text = { Field(text, { text = it }, stringResource(R.string.field_wallet_name), maxLen = MAX_WALLET_NAME) },
            confirmButton = { TextButton({ vm.renameWallet(id, text); renaming = null }) { Text(stringResource(R.string.action_save), color = Fx.signal) } },
            dismissButton = { TextButton({ renaming = null }) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
        )
    }

    revealWarn?.let { id ->
        val w = vm.wallets.firstOrNull { it.id == id }
        AlertDialog(
            onDismissRequest = { revealWarn = null },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.dialog_reveal_title, w?.display ?: stringResource(R.string.nav_wallet)), color = Fx.text) },
            text = { Text(stringResource(R.string.dialog_reveal_body), color = Fx.textDim) },
            confirmButton = {
                TextButton({
                    revealWarn = null
                    gate.require(vm, gateAct, gateScope) { vm.startReveal(id) }
                }) { Text(stringResource(R.string.action_show), color = Fx.signal) }
            },
            dismissButton = { TextButton({ revealWarn = null }) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
        )
    }

    removing?.let { id ->
        val w = vm.wallets.firstOrNull { it.id == id }
        AlertDialog(
            onDismissRequest = { removing = null },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.dialog_remove_title, w?.display ?: stringResource(R.string.nav_wallet)), color = Fx.text) },
            text = { Text(stringResource(R.string.dialog_remove_body), color = Fx.textDim) },
            confirmButton = { TextButton({ vm.removeWallet(id); removing = null }) { Text(stringResource(R.string.action_remove), color = Fx.bad) } },
            dismissButton = { TextButton({ removing = null }) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
        )
    }
}

/** Wallet tab — the selected wallet's balance, Receive / Send / History. */
@Composable
private fun WalletTab(vm: WalletViewModel) {
    val c = vm.config
    if (c == null) {
        Column(
            Modifier.fillMaxSize(), verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(stringResource(R.string.wallet_none_open), color = Fx.textDim)
            Spacer(Modifier.height(Fx.s3))
            GhostButton(stringResource(R.string.wallet_choose), Modifier.widthIn(max = 240.dp)) { vm.goHome() }
        }
        return
    }
    var tab by remember { mutableStateOf("receive") }
    val unit = if (c.chain == "btc") "BTC" else "XBT"
    val b = vm.balances

    LaunchedEffect(vm.selectedId) {
        while (true) {
            vm.refresh()
            kotlinx.coroutines.delay(20_000)
        }
    }

    Column(
        Modifier.fillMaxSize().padding(top = Fx.s2),
        verticalArrangement = Arrangement.spacedBy(Fx.s4),
    ) {
        GlassCard(hero = true) {
            Column(Modifier.fillMaxWidth()) {
                // Lock shares the label's line, so the balance below gets the
                // card's full width and a long name or hint can't push Lock out.
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Text(caps(c.display), style = FxType.label, modifier = Modifier.weight(1f))
                    TextButton({ vm.lock() }) { Text(stringResource(R.string.action_lock), color = Fx.textDim) }
                }
                Column {
                    // Whole part, fraction and unit as separate items: a figure too
                    // wide for the card wraps at the decimal point, never mid-number.
                    FlowRow {
                        val (whole, fraction) = splitAmount(if (b != null) fmt(countUpSat(b.confirmedSat)) else "—")
                        Text(whole, style = FxType.balance, modifier = Modifier.alignByBaseline())
                        if (fraction.isNotEmpty()) Text(fraction, style = FxType.balance, modifier = Modifier.alignByBaseline())
                        Text(unit, style = FxType.label, color = Fx.signal, modifier = Modifier.alignByBaseline().padding(start = 6.dp))
                    }
                    b?.let { bal ->
                        vm.usdValue(bal.confirmedSat, c.chain)?.let {
                            Text(stringResource(R.string.fiat_approx, fmtUsd(it)), style = FxType.fiat)
                        }
                    }
                    if (b != null && b.pendingSat != 0L) Text(
                        stringResource(
                            R.string.wallet_pending,
                            (if (b.pendingSat > 0) "+" else "") + fmt(b.pendingSat), unit,
                        ),
                        style = FxType.fiatSmall, color = Fx.warn,
                    )
                    // Its part in any running swap (why the balance moved); tap to open it.
                    walletSwapLines(c.id, vm.swaps).forEach { WalletSwapText(it, vm::openSwap) }
                    // `status` resolves fast (just a tip-height + pricing call) but
                    // `balances`/`history` come from the much slower address scan in
                    // the same refresh() — for a deep watch-only BTC wallet (paced
                    // through a rate-limited public explorer, unlike XBT's local
                    // index) that can take minutes. Without gating on `b != null`
                    // too, the dot went green and the hint said "Block N" the moment
                    // status() landed, looking fully loaded while the balance/history
                    // scan was still running underneath — indistinguishable from
                    // "done" at a glance. Keep showing the "connecting…" state (already
                    // translated everywhere, so reusing it needs no new string) until
                    // the first balance actually arrives.
                    // Below the status-arrived case, distinguish three states that
                    // all used to render as the same indefinite "connecting…":
                    // actively refreshing right now (tapped reconnect, or the
                    // poll loop firing), a just-failed attempt (shows the last-
                    // known balance above this, per resetView()'s doc, so this
                    // text is what tells the user that number might be stale),
                    // and genuinely never having loaded at all yet. Found live,
                    // 2026-09-28: with no distinction, tapping refresh looked
                    // like a no-op and a real failure looked identical to a
                    // wallet that had simply never loaded.
                    val hint = vm.status?.takeIf { b != null }?.let {
                        val head = when {
                            it.scanningPct != null -> stringResource(R.string.wallet_rescanning, it.scanningPct)
                            it.synced -> stringResource(R.string.wallet_block, it.blocks.toString())
                            else -> stringResource(R.string.wallet_syncing)
                        }
                        if (it.degraded) stringResource(R.string.wallet_limited_suffix, head) else head
                    } ?: when {
                        vm.refreshing -> stringResource(R.string.wallet_refreshing)
                        vm.lastRefreshError != null -> stringResource(R.string.wallet_refresh_failed)
                        else -> stringResource(R.string.wallet_connecting)
                    }
                    // Connection dot — same ok/connecting semantics as the web wallet's
                    // `.dot`: green once a status reply landed and the scan isn't still
                    // catching up, yellow (pulsing there; a plain dot here) otherwise,
                    // including the null-status "haven't heard back yet" case. Text-only
                    // status ("Connected · block N") was the only signal before this —
                    // easy to miss at a glance, especially on a watch-only wallet where
                    // there's no other activity on screen to suggest it's still loading.
                    val dotColor = vm.status?.takeIf { b != null }
                        ?.let { if (it.synced && it.scanningPct == null) Fx.good else Fx.warn } ?: Fx.warn
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Box(Modifier.size(7.dp).clip(CircleShape).background(dotColor))
                        Spacer(Modifier.width(6.dp))
                        Text(hint, style = FxType.meta)
                    }
                }
            }
        }
        if (c.hardware) Text(
            stringResource(if (c.chain == "xbt") R.string.hw_xbt_view_only else R.string.hw_btc_view_only),
            style = FxType.meta, color = Fx.warn,
        )
        val tabKeys = if (c.watchOnly) listOf("receive", "history") else listOf("receive", "send", "history")
        if (tab !in tabKeys) tab = "receive" // e.g. a stale "send" selection on a watch-only wallet
        val tabLabels = mapOf(
            "receive" to stringResource(R.string.tab_receive),
            "send" to stringResource(R.string.tab_send),
            "history" to stringResource(R.string.tab_history),
        )
        TextTabs(tabKeys.map { it to tabLabels.getValue(it) }, tab, { tab = it })
        vm.lastSentTxid?.let { txid -> SentDialog(vm, txid) }
        Column(
            Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(Fx.s4),
        ) {
            when (tab) {
                "receive" -> ReceiveTab(vm)
                "send" -> key(vm.sentCount) { SendTab(vm) } // a sent payment starts the form over
                else -> HistoryTab(vm, unit)
            }
        }
    }
}

/** "Payment sent", until the user taps OK. */
@Composable
private fun SentDialog(vm: WalletViewModel, txid: String) {
    val ctx = LocalContext.current
    val c = vm.config
    val url = c?.let { explorerTxUrl(it.chain, it.network, txid) }
    AlertDialog(
        onDismissRequest = { vm.dismissLastSent() },
        containerColor = Fx.panel,
        title = { Text(stringResource(R.string.sent_title), color = Fx.good) },
        text = {
            Text(
                stringResource(R.string.sent_txid, txid.take(12)),
                style = FxType.meta, color = Fx.textDim,
                modifier = Modifier.clickable { copyToClipboard(ctx, "txid", txid) },
            )
        },
        confirmButton = {
            TextButton({ vm.dismissLastSent() }) { Text(stringResource(android.R.string.ok), color = Fx.signal) }
        },
        dismissButton = if (url == null) null else {
            { TextButton({ openInBrowser(ctx, url) }) { Text(stringResource(R.string.action_view), color = Fx.text) } }
        },
    )
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ReceiveTab(vm: WalletViewModel) {
    val ctx = LocalContext.current
    val i = vm.config?.nextReceive ?: 0
    val addr = remember(i, vm.session) { runCatching { vm.session?.receiveAddress(i)?.address }.getOrNull() ?: "…" }
    val chooserTitle = stringResource(R.string.share_address_chooser)
    val copy = { copyToClipboard(ctx, "address", addr) }
    val share = {
        ctx.startActivity(
            Intent.createChooser(
                Intent(Intent.ACTION_SEND).apply { type = "text/plain"; putExtra(Intent.EXTRA_TEXT, addr) },
                chooserTitle,
            ),
        )
    }
    GlassCard {
        Text(stringResource(R.string.tab_receive), color = Fx.text, fontWeight = FontWeight.SemiBold)
        if (addr.length > 3) {
            val qr = remember(addr) { qrBitmap(addr) }
            val copyQr = { copyImageToClipboard(ctx, qr) }
            Image(
                bitmap = qr.asImageBitmap(),
                contentDescription = stringResource(R.string.cd_address_qr),
                modifier = Modifier
                    .align(Alignment.CenterHorizontally)
                    .size(224.dp)
                    .clip(RoundedCornerShape(Fx.rSm))
                    .background(Color.White)
                    .combinedClickable(onClick = {}, onLongClick = copyQr, onDoubleClick = copyQr)
                    .padding(10.dp),
            )
            Text(
                stringResource(R.string.receive_qr_hint),
                style = FxType.meta,
                color = Fx.textFaint,
                modifier = Modifier.align(Alignment.CenterHorizontally),
            )
        }
        Text(
            addr,
            style = FxType.amount,
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(Fx.rSm))
                .background(Fx.panelRaised)
                .clickable { copy() }
                .padding(Fx.s4),
        )
        Text(stringResource(R.string.receive_address_n, i), style = FxType.meta, color = Fx.textFaint)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_copy), Modifier.weight(1f)) { copy() }
            GhostButton(stringResource(R.string.action_share), Modifier.weight(1f)) { share() }
        }
        GhostButton(stringResource(R.string.action_new_address)) { vm.newReceiveAddress() }
    }
}

@Composable
private fun SendTab(vm: WalletViewModel) {
    var to by remember { mutableStateOf("") }
    var amount by remember { mutableStateOf("") }
    var amountInSat by remember { mutableStateOf(true) }
    var sweep by remember { mutableStateOf(false) }
    var feeFromAmount by remember { mutableStateOf(false) }
    var target by remember { mutableStateOf(6) }
    var custom by remember { mutableStateOf("") }
    var replayProtect by remember { mutableStateOf(false) }
    val isBtc = vm.config?.chain == "btc"
    val coinUnit = if (isBtc) "BTC" else "XBT"
    val amountSat = amountToSat(amount, amountInSat)
    val scanPrompt = stringResource(R.string.scan_prompt)
    val scan = rememberLauncherForActivityResult(ScanContract()) { r ->
        r.contents?.let { to = addressFromScan(it) }
    }
    GlassCard {
        Text(stringResource(R.string.tab_send), color = Fx.text, fontWeight = FontWeight.SemiBold)
        // An address never contains whitespace; stripping it on input kills the
        // cryptic "base58 error" you otherwise get from a pasted trailing newline.
        Field(to, { new -> to = new.filterNot(Char::isWhitespace) }, stringResource(R.string.field_to_address), mono = true)
        GhostButton(stringResource(R.string.action_scan_qr)) {
            scan.launch(
                ScanOptions().apply {
                    setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                    setPrompt(scanPrompt)
                    setBeepEnabled(false)
                    setOrientationLocked(false)
                },
            )
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(sweep, { sweep = it }); Text(stringResource(R.string.send_sweep), color = Fx.text)
        }
        if (!sweep) {
            Text(caps(stringResource(R.string.field_amount)), style = FxType.label)
            Row(
                horizontalArrangement = Arrangement.spacedBy(Fx.s2),
                verticalAlignment = Alignment.Top,
            ) {
                Field(
                    amount, { amount = it }, null,
                    keyboardType = if (amountInSat) KeyboardType.Number else KeyboardType.Decimal,
                    modifier = Modifier.weight(1f),
                )
                AmountUnitPicker(amountInSat, coinUnit) { amountInSat = it }
            }
            amountSat?.let { sat ->
                val alt = if (amountInSat) stringResource(R.string.send_alt_coin, fmt(sat), coinUnit)
                    else stringResource(R.string.send_alt_sat, sat.toString())
                val fiat = vm.usdValue(sat, if (isBtc) "btc" else "xbt")
                    ?.let { stringResource(R.string.send_alt_fiat, fmtUsd(it)) } ?: ""
                Text(alt + fiat, style = FxType.fiatSmall)
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(feeFromAmount, { feeFromAmount = it })
                Text(stringResource(R.string.send_fee_from_amount), color = Fx.text)
            }
            if (feeFromAmount) Text(
                stringResource(R.string.send_fee_from_amount_note),
                style = FxType.meta, color = Fx.textFaint,
            )
        }
        Segmented(
            listOf(
                "1" to stringResource(R.string.fee_fast),
                "6" to stringResource(R.string.fee_normal),
                "144" to stringResource(R.string.fee_slow),
            ),
            target.toString(), { target = it.toInt(); custom = "" },
        )
        Field(custom, { custom = it }, stringResource(R.string.field_custom_feerate), keyboardType = KeyboardType.Number)
        if (isBtc && !sweep) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(replayProtect, { replayProtect = it })
                Text(stringResource(R.string.send_replay_protect), color = Fx.text)
            }
            if (replayProtect) Text(stringResource(R.string.send_replay_note), style = FxType.meta, color = Fx.textFaint)
        }
        vm.status?.pricing?.let {
            Text(
                stringResource(R.string.send_service_fee, "%.2f".format(it.bps / 100.0), it.floorSat),
                style = FxType.meta, color = Fx.textFaint,
            )
        }
        ErrorText(vm.error)
        val focus = LocalFocusManager.current
        PrimaryButton(
            stringResource(R.string.action_review),
            enabled = to.isNotBlank() && (sweep || (amountSat != null && amountSat > 0)),
            working = vm.reviewing,
        ) {
            focus.clearFocus() // the keyboard goes, and the address field lets go
            vm.buildPayment(to, amountSat ?: 0L, sweep, custom.toLongOrNull(), target, replayProtect && isBtc, feeFromAmount && !sweep)
        }
    }
}

/** The "sat / BTC" (or "sat / XBT") unit picker that sits beside the Amount
 *  field. Styled to match [Field] so the two line up. */
@Composable
private fun AmountUnitPicker(isSat: Boolean, coinUnit: String, onChange: (Boolean) -> Unit) {
    var open by remember { mutableStateOf(false) }
    val satLabel = stringResource(R.string.unit_sat)
    Box {
        Row(
            Modifier
                .height(56.dp)
                .clip(RoundedCornerShape(Fx.rSm))
                .background(Fx.panel)
                .border(1.dp, Fx.control, RoundedCornerShape(Fx.rSm))
                .clickable { open = true }
                .padding(start = Fx.s3, end = Fx.s1),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(if (isSat) satLabel else coinUnit, style = FxType.body, color = Fx.text)
            Icon(Icons.Filled.ArrowDropDown, contentDescription = stringResource(R.string.cd_amount_unit), tint = Fx.textDim)
        }
        DropdownMenu(
            expanded = open,
            onDismissRequest = { open = false },
            containerColor = Fx.panel,
        ) {
            DropdownMenuItem(
                text = { Text(satLabel, color = Fx.text) },
                onClick = { onChange(true); open = false },
            )
            DropdownMenuItem(
                text = { Text(coinUnit, color = Fx.text) },
                onClick = { onChange(false); open = false },
            )
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun HistoryTab(vm: WalletViewModel, unit: String) {
    val ctx = LocalContext.current
    val chain = vm.config?.chain ?: "xbt"
    val network = vm.config?.network ?: "mainnet"
    if (vm.history.isEmpty()) {
        GlassCard { Text(stringResource(R.string.history_empty), color = Fx.textFaint) }
        return
    }
    val sendLabel = stringResource(R.string.history_send)
    val receiveLabel = stringResource(R.string.history_receive)
    val internalLabel = stringResource(R.string.history_internal)
    GlassCard {
        vm.history.forEach { h ->
            val url = explorerTxUrl(chain, network, h.txid)
            Row(
                Modifier
                    .fillMaxWidth()
                    .combinedClickable(
                        onClick = { if (url != null) openInBrowser(ctx, url) else copyToClipboard(ctx, "txid", h.txid) },
                        onLongClick = { copyToClipboard(ctx, "txid", h.txid) },
                    )
                    .padding(vertical = 10.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Column(Modifier.weight(1f)) {
                    FlowRow(itemVerticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        Text((if (h.amountSat > 0) "+" else "") + fmt(h.amountSat) + " " + unit,
                            style = FxType.amount, color = if (h.amountSat > 0) Fx.signal else Fx.text)
                        if (h.amountSat != 0L) vm.usdValue(if (h.amountSat < 0) -h.amountSat else h.amountSat, chain)?.let {
                            Text(stringResource(R.string.fiat_approx, fmtUsd(it)), style = FxType.fiatSmall)
                        }
                    }
                    val label = when {
                        h.internal -> internalLabel
                        h.send -> sendLabel
                        else -> receiveLabel
                    }
                    Text(
                        if (h.send && h.feeSat > 0L)
                            stringResource(R.string.history_row_subtitle_fee, label, h.txid.take(10), h.feeSat)
                        else
                            stringResource(R.string.history_row_subtitle, label, h.txid.take(10)),
                        style = FxType.meta,
                    )
                }
                Text(
                    when {
                        h.confirmations < 1 -> stringResource(R.string.history_pending)
                        h.confirmations < 6 -> stringResource(R.string.history_conf_n, h.confirmations.toInt())
                        else -> stringResource(R.string.history_confirmed)
                    },
                    style = FxType.meta, color = if (h.confirmations < 1) Fx.warn else Fx.textDim,
                    modifier = Modifier.padding(start = Fx.s3),
                )
            }
        }
        Text(
            stringResource(R.string.history_hint),
            style = FxType.meta, color = Fx.textFaint, modifier = Modifier.padding(top = Fx.s2),
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ConfirmSheet(vm: WalletViewModel, p: PlanPreview, unit: String) {
    val gate = com.fortis.wallet.ui.rememberIdentityGate()
    val gateAct = com.fortis.wallet.ui.rememberFragmentActivity()
    val gateScope = rememberCoroutineScope()
    com.fortis.wallet.ui.ConfirmIdentityDialog(gate, vm)
    // Opened all the way: half-open, the amounts and Sign & send sat below the fold.
    // Not dismissable while signing and sending: the payment can't be half-cancelled.
    val sheet = rememberModalBottomSheetState(
        skipPartiallyExpanded = true,
        confirmValueChange = { it != SheetValue.Hidden || !vm.sending },
    )
    ModalBottomSheet(
        onDismissRequest = { if (!vm.sending) vm.cancelPending() },
        sheetState = sheet,
        containerColor = Fx.panel,
    ) {
        Column(
            Modifier.verticalScroll(rememberScrollState()).padding(horizontal = Fx.s5).padding(top = Fx.s2, bottom = Fx.s6),
            verticalArrangement = Arrangement.spacedBy(Fx.s4),
        ) {
            Text(
                if (p.sweep) stringResource(R.string.confirm_sweep_title) else stringResource(R.string.confirm_payment_title),
                style = MaterialTheme.typography.titleLarge, color = Fx.text,
            )
            val chain = vm.config?.chain ?: "xbt"
            val inTotal = p.plan.selected.sumOf { it.valueSat.toLong() }
            val svcFee = p.plan.serviceFeeSat?.toLong() ?: 0L
            val out = inTotal - p.plan.feeSat.toLong() - svcFee - (p.plan.changeSat?.toLong() ?: 0L)
            val total = out + p.plan.feeSat.toLong() + svcFee

            @Composable
            fun amountLine(sat: Long): String {
                val fiat = vm.usdValue(sat, chain)
                return if (fiat != null) stringResource(R.string.value_with_fiat, fmt(sat), unit, fmtUsd(fiat))
                else "${fmt(sat)} $unit"
            }

            // The address on its own lines, in full: it's what the user checks.
            Column(verticalArrangement = Arrangement.spacedBy(Fx.s1)) {
                Text(stringResource(R.string.confirm_to), style = FxType.body, color = Fx.textDim)
                Text(p.to, style = FxType.amount, color = Fx.text)
            }
            kv(stringResource(R.string.confirm_you_send), amountLine(total))
            kv(stringResource(R.string.confirm_recipient_gets), amountLine(out))
            kv(
                stringResource(R.string.confirm_network_fee),
                stringResource(R.string.confirm_network_fee_value, fmt(p.plan.feeSat.toLong()), unit, p.feerate.toInt()),
            )
            if (svcFee > 0) kv(stringResource(R.string.confirm_service_fee), "${fmt(svcFee)} $unit")
            p.plan.changeSat?.let { kv(stringResource(R.string.confirm_change), "${fmt(it.toLong())} $unit") }
            if (p.replayProtected) kv(stringResource(R.string.confirm_replay), stringResource(R.string.confirm_replay_value))
            ErrorText(vm.error)
            Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                GhostButton(stringResource(R.string.action_cancel), Modifier.weight(1f), enabled = !vm.sending) {
                    if (!vm.sending) vm.cancelPending()
                }
                PrimaryButton(stringResource(R.string.action_sign_send), Modifier.weight(1f), working = vm.sending) {
                    gate.require(vm, gateAct, gateScope) { vm.confirmSend() }
                }
            }
        }
    }
}

@Composable
internal fun kv(k: String, v: String) = Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
    // The value is measured first and keeps its width; a long label wraps instead
    // (otherwise "Your price (the worst you'll take)" squeezed the price to one character).
    Text(k, style = FxType.body, color = Fx.textDim, modifier = Modifier.weight(1f))
    Text(v, style = FxType.amount, modifier = Modifier.padding(start = Fx.s4), textAlign = androidx.compose.ui.text.style.TextAlign.End)
}

/** The app-language override. Empty = follow the device; picking one recreates
 *  the activity (via AppCompatDelegate). */
@Composable
private fun LanguageCard() {
    val ctx = LocalContext.current
    var open by remember { mutableStateOf(false) }
    val current = currentLocaleTag(ctx)
    val systemDefault = stringResource(R.string.language_system_default)
    GlassCard {
        Row(
            Modifier.fillMaxWidth().clickable { open = true },
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(stringResource(R.string.settings_language), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(current?.let { localeLabel(it) } ?: systemDefault, color = Fx.textDim)
        }
    }
    if (open) {
        val entries = remember { listOf<String?>(null) + SUPPORTED_LOCALES.sortedBy { localeLabel(it) } }
        AlertDialog(
            onDismissRequest = { open = false },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.settings_language), color = Fx.text) },
            text = {
                LazyColumn(Modifier.heightIn(max = 420.dp)) {
                    items(entries) { tag ->
                        val selected = tag == current
                        Text(
                            tag?.let { localeLabel(it) } ?: systemDefault,
                            color = if (selected) Fx.signal else Fx.text,
                            modifier = Modifier.fillMaxWidth()
                                .clickable { open = false; applyLocale(ctx, tag) }
                                .padding(vertical = 12.dp),
                        )
                    }
                }
            },
            confirmButton = {},
            dismissButton = { TextButton({ open = false }) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
        )
    }
}
