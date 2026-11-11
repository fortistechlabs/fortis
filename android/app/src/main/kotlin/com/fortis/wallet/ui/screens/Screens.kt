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
import com.fortis.wallet.data.FIAT_CODES
import com.fortis.wallet.data.defaultFiat
import com.fortis.wallet.data.fiatLabel
import com.fortis.wallet.data.fiatSymbol
import com.fortis.wallet.data.LN_CHAIN
import com.fortis.wallet.data.LN_XBT_CHAIN
import com.fortis.wallet.data.isLightningChain
import com.fortis.wallet.lightning.LnState
import com.fortis.wallet.lightning.lightningWalletOf
import com.fortis.wallet.lightning.deleteWalletLightningWarningSat
import com.fortis.wallet.lightning.isLightningWallet
import com.fortis.wallet.ui.*
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType
import com.fortis.wallet.ui.theme.caps
import com.fortis.wallet.ui.theme.Mono
import com.fortis.wallet.wallet.EntropyCollector
import com.fortis.wallet.wallet.entropyProgress
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private fun fmt(sat: Long) = fmtCoin(sat)

/** Ease the displayed satoshi value toward [sat] whenever it changes — 0 → balance
 *  on first load, then old → new on each refresh. ~650 ms, decelerating. */
@Composable
internal fun countUpSat(sat: Long): Long {
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

/** Each fee a ledger row paid, what it was for and who received it; null when it paid none.
 *  "Fees: network fee 183 sat (to miners) · transfer fee 600 sat (to Fortis)". */
@Composable
internal fun historyFeeLine(h: HistoryRow): String? {
    if (h.netSat == null) return null
    val move = h.noteRes == R.string.history_moved_onchain || h.noteRes == R.string.history_moved_onchain_closed
    val parts = buildList {
        when {
            move -> {
                val network = h.feeSat - h.transferFeeSat
                if (network > 0L) add(stringResource(R.string.fee_network_miners, network))
                if (h.transferFeeSat > 0L) add(stringResource(R.string.fee_transfer_fortis, h.transferFeeSat))
                if (h.noteRes == R.string.history_moved_onchain_closed) add(stringResource(R.string.fee_close_paid_by_fortis))
            }
            h.noteRes == R.string.history_move_fee -> add(stringResource(R.string.fee_channel_fortis, -h.amountSat))
            h.noteRes == R.string.ln_account_withdrawn && h.feeSat > 0L -> add(stringResource(R.string.fee_withdraw_fortis, h.feeSat))
            h.send && h.feeSat > 0L -> add(stringResource(R.string.fee_routing, h.feeSat))
        }
    }
    return if (parts.isEmpty()) null else stringResource(R.string.fee_line, parts.joinToString(" · "))
}

/** When a history entry happened, in the phone's time zone, language and 12/24-hour setting:
 *  "Today, 10:36", "Yesterday, 18:31", or "4 Nov 2026, 17:27". */
@Composable
internal fun historyTimeText(epochSecs: Long): String {
    val ctx = LocalContext.current
    val today = stringResource(R.string.history_today)
    val yesterday = stringResource(R.string.history_yesterday)
    val ms = epochSecs * 1_000
    val time = android.text.format.DateFormat.getTimeFormat(ctx).format(java.util.Date(ms))
    val zone = java.time.ZoneId.systemDefault()
    val day = java.time.Instant.ofEpochMilli(ms).atZone(zone).toLocalDate()
    val now = java.time.LocalDate.now(zone)
    return when (day) {
        now -> "$today, $time"
        now.minusDays(1) -> "$yesterday, $time"
        else -> android.text.format.DateFormat.getMediumDateFormat(ctx).format(java.util.Date(ms)) + ", " + time
    }
}

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
internal fun copyImageToClipboard(ctx: Context, bitmap: Bitmap) {
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
    // Whether the edge offers Lightning (XBT) yet (the create / restore screens' chain choice).
    LaunchedEffect(Unit) { vm.refreshXbtLightningOffered() }
    // One Lightning wallet per app: offered until the app has it.
    if (vm.canAddLightning(LN_CHAIN)) {
        GhostButton(stringResource(R.string.onboard_create_lightning)) { vm.goCreateLightning() }
    }
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
    var chain by remember { mutableStateOf(startChain(vm, restoring = false)) }
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
        ChainRow(chain, vm.lightningOnboardChoices(restoring = false)) { chain = it }
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
                        vm.createWallet(name, chain, networkFor(chain), passphrase, lock)
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
    var chain by remember { mutableStateOf(startChain(vm, restoring = true)) }
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
        // Lightning (XBT) is restorable whatever the edge says: its funds must be reachable.
        ChainRow(chain, vm.lightningOnboardChoices(restoring = true)) { chain = it }
        // Only an on-chain wallet can also be listed on the other chain.
        if (!isLightningChain(chain)) Text(
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
                        vm.restoreWallet(name, phrase, passphrase, chain, networkFor(chain), lock)
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
        ChainRow(chain, lightning = emptyList()) { chain = it }
        ErrorText(vm.error)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_back), Modifier.weight(1f)) { vm.cancelOnboard() }
            PrimaryButton(stringResource(R.string.action_watch), Modifier.weight(1f)) {
                scope.launch { vm.watchWallet(name, xpub, chain) }
            }
        }
    }
}

/** XBT · BTC, then the Lightning wallets in [lightning] ("ln" → "Lightning", "ln-xbt" →
 *  "Lightning (XBT)"): one of each per app, never watch-only. */
@Composable
private fun ChainRow(chain: String, lightning: List<String>, onChain: (String) -> Unit) {
    val labels = mapOf(
        LN_CHAIN to stringResource(R.string.wallet_chain_lightning),
        LN_XBT_CHAIN to stringResource(R.string.wallet_chain_lightning_xbt),
    )
    val options = listOf("xbt" to "XBT", "btc" to "BTC") + lightning.mapNotNull { c -> labels[c]?.let { c to it } }
    Segmented(options, chain, onChain)
}

/** The chain the create / restore screens open on: Lightning when that flow was asked for. */
private fun startChain(vm: WalletViewModel, restoring: Boolean): String =
    vm.onboardChain?.takeIf { !isLightningChain(it) || it in vm.lightningOnboardChoices(restoring) } ?: "xbt"

/** A Lightning wallet's network is the node's ("bitcoin"); on-chain wallets use "mainnet". */
private fun networkFor(chain: String): String = if (isLightningChain(chain)) "bitcoin" else "mainnet"

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
internal fun <T> TextTabs(
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
    val ctx = LocalContext.current
    val unit = chainUnit(vm.config?.chain)
    val rxToastFmt = stringResource(R.string.toast_ln_received)
    // Both Lightning wallets' nodes run whichever wallet is open: each announces its own receipts.
    for (walletChain in listOf(LN_CHAIN, LN_XBT_CHAIN)) {
        val session = vm.lnSide(walletChain).session
        LaunchedEffect(session) {
            session.events.collect { ev ->
                // The Fortis balance moving into the channel is not a new payment: it was
                // announced when it reached the balance.
                if (ev is uniffi.wallet_ffi.LnEventRecord.PaymentReceived && !vm.isAccountMove(ev.amountMsat, walletChain)) {
                    val sat = displaySatFromMsat(ev.amountMsat)
                    Toast.makeText(ctx, rxToastFmt.format(fmt(sat)), Toast.LENGTH_SHORT).show()
                }
            }
        }
    }
    LaunchedEffect(vm) {
        vm.accountReceived.collect { sat ->
            Toast.makeText(ctx, rxToastFmt.format(fmt(sat)), Toast.LENGTH_SHORT).show()
        }
    }
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
    vm.pendingLn?.let { LnConfirmSheet(vm, it, unit) }
    if (vm.hwOfferXbtFor != null) XbtOfferSheet(vm)
    if (vm.lightningWalletPrompt) CreateLightningWalletDialog(vm)
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
    // Both Lightning wallets can be held elsewhere: each banner names its wallet.
    for (lnChain in listOf(LN_CHAIN, LN_XBT_CHAIN)) {
        LightningTakeoverBanner(
            vm.lnSide(lnChain).session,
            walletName = com.fortis.wallet.lightning.lightningWalletOf(vm.wallets, lnChain)?.display,
        )
    }
    Text(stringResource(R.string.home_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
    // Each wallet is its own key — pressable like a button, not a line in a list.
    Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
        vm.wallets.forEach { w ->
            val wUnit = chainUnit(w.chain)
            val lnBal = if (w.isLightning) lightningWalletBalance(vm, w.chain) else null
            val bal = if (w.isLightning) lightningWalletTotalSat(vm, w.chain) else vm.walletBalances[w.id]
            PressableTile(selected = w.id == vm.selectedId, onClick = { vm.selectWallet(w.id) }) {
                Column(Modifier.weight(1f)) {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        if (w.isLightning) com.fortis.wallet.ui.LightningBoltIcon()
                        Text(w.display, color = Fx.text, fontWeight = FontWeight.Medium)
                    }
                    Text(
                        bal?.let { "${fmt(it)} $wUnit" } ?: stringResource(R.string.home_tap_to_open),
                        style = if (bal != null) FxType.amount else FxType.meta,
                        color = if (bal != null) Fx.text else Fx.dimOnSlate,
                    )
                    bal?.let { vm.fiatText(it, w.chain) }?.let {
                        Text(stringResource(R.string.fiat_approx, it), style = FxType.fiatSmall, color = Fx.dimOnSlate)
                    }
                    // Lightning funds still moving (a channel opening or closing).
                    if (lnBal?.moving == true) Text(
                        stringResource(R.string.wallet_split_moving),
                        style = FxType.fiatSmall, color = Fx.warn,
                    )
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
                val wUnit = chainUnit(w.chain)
                val bal = if (w.isLightning) lightningWalletTotalSat(vm, w.chain) else vm.walletBalances[w.id]
                Column(
                    Modifier.fillMaxWidth().clip(RoundedCornerShape(Fx.rSm))
                        .background(if (current) Fx.panelRaised else Color.Transparent).padding(Fx.s3),
                    verticalArrangement = Arrangement.spacedBy(Fx.s2),
                ) {
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                if (w.isLightning) com.fortis.wallet.ui.LightningBoltIcon()
                                Text(w.display, color = Fx.text, fontWeight = FontWeight.Medium)
                            }
                            Text(
                                bal?.let { "${fmt(it)} $wUnit" }
                                    ?: listOfNotNull(w.network.takeIf { it != "mainnet" }, if (current) stringResource(R.string.home_open) else null)
                                        .joinToString(" · ").ifEmpty { "…" },
                                style = if (bal != null) FxType.amount else FxType.meta,
                                color = if (bal != null) Fx.text else Fx.textFaint,
                            )
                            bal?.let { vm.fiatText(it, w.chain) }?.let {
                                Text(stringResource(R.string.fiat_approx, it), style = FxType.fiatSmall)
                            }
                        }
                        if (!current) TextButton({ vm.selectWallet(w.id) }) { Text(stringResource(R.string.action_open), color = Fx.signal) }
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                        GhostButton(stringResource(R.string.action_rename), Modifier.weight(1f), dense = true) { renaming = w.id }
                        // The Lightning wallet has no account xpub.
                        if (!w.isLightning) GhostButton(stringResource(R.string.action_copy_xpub), Modifier.weight(1f), dense = true) {
                            val xpub = vm.accountKey(w.id)
                            if (xpub != null) copyToClipboard(ctx, "xpub", xpub)
                            else Toast.makeText(ctx, xpubLoading, Toast.LENGTH_SHORT).show()
                        }
                    }
                    if (!w.watchOnly) GhostButton(stringResource(R.string.action_recovery_phrase), dense = true) { revealWarn = w.id }
                    val other = w.otherChain
                    val hasOther = other == null || vm.wallets.any { it.name == w.name && it.chain == other }
                    if (!hasOther && vm.canAddWallet) GhostButton(stringResource(R.string.also_add_on, other.uppercase()), dense = true) {
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

        // The nodes run whichever wallet is open, so each Lightning wallet's card shows while it exists.
        LightningWalletSettingsCard(vm, LN_CHAIN)
        LightningWalletSettingsCard(vm, LN_XBT_CHAIN)

        OwnServerCard(vm)
        LanguageCard()
        CurrencyCard(vm)

        val openSupport = com.fortis.wallet.ui.LocalSendToSupport.current
        if (openSupport != null) com.fortis.wallet.ui.SupportCard { openSupport("") }
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
        // The removed wallet's own Lightning side (its node runs whichever wallet is open).
        val side = vm.lnSide(w?.chain ?: LN_CHAIN)
        val lnState by side.session.state.collectAsState()
        // Only a Lightning wallet holds Lightning funds.
        // Funds held by Fortis count too: the phrase restores them, but warn all the same.
        val accountSat = ((side.accountBalance?.balanceMsat ?: 0uL) / 1_000uL).toLong()
        val lnWarnSat = if (isLightningWallet(w)) {
            ((deleteWalletLightningWarningSat(lnState) ?: 0L) + accountSat).takeIf { it > 0L }
        } else null
        // With the node not running the balance is unknown: warn that funds may be there.
        val lnUnknown = isLightningWallet(w) && com.fortis.wallet.lightning.deleteWalletLightningBalanceUnknown(lnState)
        val baseRemoveBody = stringResource(R.string.dialog_remove_body)
        val removeBody = when {
            lnWarnSat != null -> stringResource(R.string.dialog_remove_lightning_warning, fmt(lnWarnSat), baseRemoveBody)
            lnUnknown -> stringResource(R.string.dialog_remove_lightning_unknown, baseRemoveBody)
            else -> baseRemoveBody
        }
        AlertDialog(
            onDismissRequest = { removing = null },
            containerColor = Fx.panel,
            title = { Text(stringResource(R.string.dialog_remove_title, w?.display ?: stringResource(R.string.nav_wallet)), color = Fx.text) },
            text = { Text(removeBody, color = if (lnWarnSat != null || lnUnknown) Fx.warn else Fx.textDim) },
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
    // Before anything on-chain: the Lightning wallet has no session, backend or address.
    if (c.isLightning) { LightningWalletTab(vm); return }
    var tab by remember { mutableStateOf("receive") }
    val unit = chainUnit(c.chain)
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
                        vm.fiatText(bal.confirmedSat, c.chain)?.let {
                            Text(stringResource(R.string.fiat_approx, it), style = FxType.fiat)
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
                        vm.lastRefreshError != null -> vm.ownServerHost
                            ?.let { stringResource(R.string.wallet_own_server_unreachable, it) }
                            ?: stringResource(R.string.wallet_refresh_failed)
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
        // Into the Lightning wallet of this wallet's own chain (BTC → Lightning, XBT → Lightning (XBT)).
        val canMove = !c.watchOnly && !c.hardware &&
            (c.network == "mainnet" || c.network == "bitcoin") &&
            com.fortis.wallet.lightning.lightningChainForOnchain(c.chain)?.let { lightningWalletOf(vm.wallets, it) } != null
        val tabKeys = when {
            c.watchOnly -> listOf("receive", "history")
            canMove -> listOf("receive", "send", "move", "history")
            else -> listOf("receive", "send", "history")
        }
        LaunchedEffect(vm.pendingSendText, c.id) {
            if (vm.pendingSendText != null && "send" in tabKeys) {
                tab = "send"
            }
        }
        if (tab !in tabKeys) tab = "receive" // e.g. a stale "send" selection on a watch-only wallet
        val tabLabels = mapOf(
            "receive" to stringResource(R.string.tab_receive),
            "send" to stringResource(R.string.tab_send),
            "move" to stringResource(R.string.transfer_tab),
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
                "move" -> key(vm.sentCount) { TransferToLightningTab(vm) }
                else -> HistoryTab(vm, unit)
            }
        }
    }
}

/** "Payment sent", until the user taps OK. */
@Composable
internal fun SentDialog(vm: WalletViewModel, txid: String) {
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
    val c = vm.config
    val i = c?.nextReceive ?: 0
    val addr = remember(i, vm.session) { runCatching { vm.session?.receiveAddress(i)?.address }.getOrNull() ?: "…" }
    // On-chain only: Lightning is received in the Lightning wallet.
    val rxQr = remember(addr) { buildReceiveQr(address = addr, amountSat = null) }

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
            val qr = remember(rxQr.uri) { qrBitmap(rxQr.uri) }
            val copyQr = { copyImageToClipboard(ctx, qr) }
            Image(
                bitmap = qr.asImageBitmap(),
                contentDescription = stringResource(R.string.cd_address_qr),
                modifier = Modifier
                    .align(Alignment.CenterHorizontally)
                    .size(224.dp)
                    .clip(RoundedCornerShape(Fx.rSm))
                    .background(Color.White)
                    .combinedClickable(onClick = copyQr, onLongClick = copyQr, onDoubleClick = copyQr)
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
    var to by remember { mutableStateOf(vm.consumePendingSendText() ?: "") }
    var amount by remember { mutableStateOf("") }
    var amountInSat by remember { mutableStateOf(true) }
    var sweep by remember { mutableStateOf(false) }
    var feeFromAmount by remember { mutableStateOf(false) }
    var target by remember { mutableStateOf(6) }
    var custom by remember { mutableStateOf("") }
    var replayProtect by remember { mutableStateOf(false) }
    val c = vm.config
    val isBtc = c?.chain == "btc"
    val network = c?.network ?: "bitcoin"
    val coinUnit = chainUnit(c?.chain)
    val amountSat = amountToSat(amount, amountInSat)
    val onchainSat = vm.balances?.confirmedSat ?: 0L

    LaunchedEffect(vm.pendingSendText) {
        vm.consumePendingSendText()?.let { to = it }
    }

    // A Lightning target is a Lightning wallet's to pay: the one of its chain (an XBT invoice's
    // is Lightning (XBT)); an address or LNURL, this wallet's chain's. This wallet only pays
    // on-chain (a BIP21 URI with a Lightning part still has its on-chain address).
    val isXbt = c?.chain == "xbt"
    // What this wallet offers: the Lightning wallet of the target's chain, and an on-chain send
    // only when there is no Lightning part or it is this wallet's own chain's.
    val offer = remember(to, c?.chain, network) {
        if (!isBtc && !isXbt) OnchainSendOffer(null, onchain = true)
        else onchainSendOffer(c?.chain ?: "btc", to, network)
    }
    val lightningTarget = offer.lightningChain != null
    val lnChain = offer.lightningChain
    val sendPlan = remember(to, isBtc, network, onchainSat, offer) {
        if (!isBtc || to.isBlank() || (lightningTarget && !offer.onchain)) null
        else planSend(text = to, onchainSat = onchainSat, ln = LnState.Off, network = network)
    }

    LaunchedEffect(sendPlan) {
        if (sendPlan is SendPlan.OnChain && sendPlan.amountSat != null && amount.isEmpty()) {
            amountInSat = true
            amount = sendPlan.amountSat.toString()
        }
    }

    var scanning by remember { mutableStateOf(false) }
    if (scanning) QrScanDialog(
        onResult = { raw ->
            scanning = false
            val text = textFromIntent(raw) ?: raw.trim()
            // An XBT wallet keeps a scanned Lightning target whole (it is offered to Lightning (XBT)).
            to = if (isBtc || (isXbt && isLightningTarget(text, network))) text else addressFromScan(raw)
        },
        onDismiss = { scanning = false },
    )
    GlassCard {
        Text(stringResource(R.string.tab_send), color = Fx.text, fontWeight = FontWeight.SemiBold)
        // An address or invoice never contains whitespace; stripping it on input kills the
        // cryptic "base58 error" you otherwise get from a pasted trailing newline.
        Field(to, { new -> to = new.filterNot(Char::isWhitespace) }, stringResource(R.string.field_to_address), mono = true)
        GhostButton(stringResource(R.string.action_scan_qr)) { scanning = true }

        offer.wrongChain?.let { wc ->
            // e.g. a BIP21 link whose invoice is BTC's, in an XBT wallet: its address is not this chain's.
            val ctx = LocalContext.current
            com.fortis.wallet.lightning.wrongChainMessage(
                com.fortis.wallet.lightning.lightningChainForOnchain(c?.chain ?: "btc") ?: LN_CHAIN, wc,
                isOffer = runCatching { parseScan(to.trim(), network) }.getOrNull()?.let { com.fortis.wallet.lightning.isOfferTarget(it) } == true,
            ) { ctx.getString(it) }?.let { ErrorText(it) }
        }
        if (lnChain != null) {
            val xbtLn = lnChain == LN_XBT_CHAIN
            if (lightningWalletOf(vm.wallets, lnChain) != null) {
                PrimaryButton(
                    stringResource(if (xbtLn) R.string.ln_pay_from_xbt_wallet else R.string.send_pay_with_lightning_wallet),
                ) { vm.payWithLightningWallet(to, lnChain) }
            } else {
                GhostButton(
                    stringResource(if (xbtLn) R.string.send_create_lightning_xbt_wallet else R.string.send_create_lightning_wallet),
                ) { vm.addLightningWallet(lnChain) }
            }
        }
        if (sendPlan is SendPlan.Invalid && !lightningTarget) ErrorText(sendPlan.reason)
        // Only Lightning can pay it (an invoice, offer or Lightning address): no on-chain form.
        if (!lightningTarget || (offer.onchain && (isXbt || sendPlan is SendPlan.OnChain))) {
            val effectiveAddress = (sendPlan as? SendPlan.OnChain)?.address ?: addressFromScan(to)
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
                    val fiat = vm.fiatText(sat, if (isBtc) "btc" else "xbt")
                        ?.let { stringResource(R.string.send_alt_fiat, it) } ?: ""
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
                enabled = effectiveAddress.isNotBlank() && (sweep || (amountSat != null && amountSat > 0)),
                working = vm.reviewing,
            ) {
                focus.clearFocus() // the keyboard goes, and the address field lets go
                vm.buildPayment(effectiveAddress, amountSat ?: 0L, sweep, custom.toLongOrNull(), target, replayProtect && isBtc, feeFromAmount && !sweep)
            }
        }
    }
}

/** The "sat / BTC" (or "sat / XBT") unit picker that sits beside the Amount
 *  field. Styled to match [Field] so the two line up. */
@Composable
internal fun AmountUnitPicker(isSat: Boolean, coinUnit: String, onChange: (Boolean) -> Unit) {
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

@Composable
private fun HistoryTab(vm: WalletViewModel, unit: String) {
    val c = vm.config
    // Match moves from the Lightning wallet to the transactions that delivered them here.
    LaunchedEffect(vm.history) { vm.loadLnMoves().join(); vm.settleLnMoves(vm.history) }
    val fromLn = vm.lnMoves.mapNotNull { it.txid }.toSet()
    val rows = remember(vm.history, fromLn) {
        mergeHistory(vm.history, emptyList()).map { if (it.id in fromLn) it.copy(fromLightning = true) else it }
    }
    HistoryList(vm, rows, unit, chain = c?.chain ?: "xbt", network = c?.network ?: "mainnet")
}

/** A wallet's history rows (on-chain or Lightning): tap opens the explorer (or copies the id). */
@OptIn(ExperimentalFoundationApi::class)
@Composable
internal fun HistoryList(
    vm: WalletViewModel, rows: List<HistoryRow>, unit: String, chain: String, network: String,
    /** A ledger's balance after each row, by row id (the Lightning wallet). */
    balanceAfter: Map<String, Long> = emptyMap(),
) {
    val ctx = LocalContext.current
    if (rows.isEmpty()) {
        GlassCard { Text(stringResource(R.string.history_empty), color = Fx.textFaint) }
        return
    }
    val sendLabel = stringResource(R.string.history_send)
    val receiveLabel = stringResource(R.string.history_receive)
    val internalLabel = stringResource(R.string.history_internal)
    val lnSendLabel = stringResource(R.string.history_ln_send)
    val lnReceiveLabel = stringResource(R.string.history_ln_receive)
    GlassCard {
        rows.forEach { h ->
            val url = if (!h.lightning) explorerTxUrl(chain, network, h.id) else null
            Row(
                Modifier
                    .fillMaxWidth()
                    .combinedClickable(
                        onClick = { if (url != null) openInBrowser(ctx, url) else if (h.hasRealId) copyToClipboard(ctx, "txid", h.id) },
                        onLongClick = { if (h.hasRealId) copyToClipboard(ctx, "txid", h.id) },
                    )
                    .padding(vertical = 10.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Column(Modifier.weight(1f)) {
                    FlowRow(itemVerticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        val amtPrefix = when {
                            h.lightning -> "⚡ " + (if (h.amountSat > 0) "+" else "")
                            h.amountSat > 0 -> "+"
                            else -> ""
                        }
                        val amtColor = when {
                            h.failed -> Fx.bad
                            h.amountSat > 0 -> Fx.signal
                            else -> Fx.text
                        }
                        Text(
                            amtPrefix + fmt(h.amountSat) + " " + unit,
                            style = FxType.amount,
                            color = amtColor,
                        )
                        if (h.amountSat != 0L) vm.fiatText(if (h.amountSat < 0) -h.amountSat else h.amountSat, chain)?.let {
                            Text(stringResource(R.string.fiat_approx, it), style = FxType.fiatSmall)
                        }
                    }
                    val label = when {
                        h.lightning && h.send -> lnSendLabel
                        h.lightning -> lnReceiveLabel
                        h.internal -> internalLabel
                        h.send -> sendLabel
                        else -> receiveLabel
                    }
                    val baseSubtitle = when {
                        !h.hasRealId && h.feeSat > 0L -> label + " · " + stringResource(R.string.history_fee_incl_only, h.feeSat)
                        !h.hasRealId -> label
                        h.feeSat > 0L && (h.send || h.lightning) ->
                            stringResource(if (h.netSat != null) R.string.history_row_subtitle_fee_incl else R.string.history_row_subtitle_fee, label, h.id.take(10), h.feeSat)
                        else -> stringResource(R.string.history_row_subtitle, label, h.id.take(10))
                    }
                    val notes = listOfNotNull(
                        if (h.noteRes != 0) stringResource(h.noteRes) else null,
                        h.description.takeIf { it.isNotBlank() },
                        h.arrivedSat?.let { stringResource(R.string.history_arrived, fmt(it), unit) },
                        if (h.viaFortis) stringResource(R.string.history_via_fortis) else null,
                        if (h.fromLightning) stringResource(R.string.history_from_lightning) else null,
                    )
                    val subtitle = (listOf(baseSubtitle) + notes).joinToString(" · ")
                    Text(subtitle, style = FxType.meta)
                    historyFeeLine(h)?.let { Text(it, style = FxType.meta, color = Fx.textDim) }
                    if (h.time > 0L) Text(historyTimeText(h.time), style = FxType.meta, color = Fx.textFaint)
                    balanceAfter[h.id]?.let { bal ->
                        Text(stringResource(R.string.history_balance_after, fmt(bal), unit), style = FxType.meta, color = Fx.textFaint)
                    }
                }
                val statusText = when {
                    h.failed -> stringResource(R.string.history_failed)
                    h.pending -> stringResource(R.string.history_pending)
                    !h.lightning && h.confirmations < 6 -> stringResource(R.string.history_conf_n, h.confirmations.toInt())
                    else -> stringResource(R.string.history_confirmed)
                }
                val statusColor = when {
                    h.failed -> Fx.bad
                    h.pending -> Fx.warn
                    else -> Fx.textDim
                }
                Text(
                    statusText,
                    style = FxType.meta,
                    color = statusColor,
                    modifier = Modifier.padding(start = Fx.s3),
                )
            }
        }
        Text(
            stringResource(if (rows.all { it.lightning }) R.string.ln_history_hint else R.string.history_hint),
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
                when {
                    p.transfer != null -> stringResource(R.string.transfer_to_lightning_title)
                    p.sweep -> stringResource(R.string.confirm_sweep_title)
                    else -> stringResource(R.string.confirm_payment_title)
                },
                style = MaterialTheme.typography.titleLarge, color = Fx.text,
            )
            val chain = vm.config?.chain ?: "xbt"
            val inTotal = p.plan.selected.sumOf { it.valueSat.toLong() }
            val svcFee = p.plan.serviceFeeSat?.toLong() ?: 0L
            val out = inTotal - p.plan.feeSat.toLong() - svcFee - (p.plan.changeSat?.toLong() ?: 0L)
            val total = out + p.plan.feeSat.toLong() + svcFee

            @Composable
            fun amountLine(sat: Long): String {
                val fiat = vm.fiatText(sat, chain)
                return if (fiat != null) stringResource(R.string.value_with_fiat, fmt(sat), unit, fiat)
                else "${fmt(sat)} $unit"
            }

            val t = p.transfer
            if (t != null) {
                // A move to the user's own Lightning wallet: the fee in one line, what arrives.
                kv(stringResource(R.string.confirm_you_send), amountLine(total))
                kv(stringResource(R.string.confirm_transfer_fee), amountLine(t.feeSat))
                kv(stringResource(R.string.confirm_transfer_arrives), amountLine(t.arrivesSat))
                Text(stringResource(R.string.transfer_to_lightning_help), style = FxType.meta, color = Fx.textDim)
            } else {
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
            }
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

/** The currency fiat values are shown in. Empty = the device region's. */
/** What a failed own-server check tells the user. */
@Composable
private fun ownServerErrorText(c: com.fortis.wallet.data.OwnServerCheck): String? = when (c) {
    is com.fortis.wallet.data.OwnServerCheck.Ok -> null
    com.fortis.wallet.data.OwnServerCheck.NotHttps -> stringResource(R.string.own_server_err_https)
    com.fortis.wallet.data.OwnServerCheck.Unreachable -> stringResource(R.string.own_server_err_unreachable)
    com.fortis.wallet.data.OwnServerCheck.NotEsplora -> stringResource(R.string.own_server_err_not_esplora)
    com.fortis.wallet.data.OwnServerCheck.WrongChain -> stringResource(R.string.own_server_err_wrong_chain)
    is com.fortis.wallet.data.OwnServerCheck.NotSynced -> stringResource(R.string.own_server_err_not_synced, c.tip.toString())
}

/** Per chain: read it from fortis servers or the user's own Esplora server. */
@Composable
private fun OwnServerCard(vm: WalletViewModel) {
    var editing by remember { mutableStateOf<String?>(null) }
    GlassCard {
        Text(stringResource(R.string.settings_own_server), color = Fx.text, fontWeight = FontWeight.SemiBold)
        Text(stringResource(R.string.own_server_note), style = FxType.meta, color = Fx.textFaint)
        for (chain in listOf("btc", "xbt")) {
            val prefs = vm.ownServers[chain]
            val url = prefs?.url
            val host = url?.let { runCatching { java.net.URI(it).host }.getOrNull() ?: it }
            val busy = vm.ownServerBusy == chain
            Column(verticalArrangement = Arrangement.spacedBy(Fx.s1)) {
                Text(chain.uppercase(), color = Fx.text, fontWeight = FontWeight.Bold, modifier = Modifier.padding(top = Fx.s2))
                @Composable
                fun option(label: String, selected: Boolean, onPick: () -> Unit) = Row(
                    Modifier.fillMaxWidth().heightIn(min = 44.dp)
                        .clickable(enabled = !busy && !selected, onClick = onPick),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(if (selected) "●" else "○", color = if (selected) Fx.signal else Fx.textDim)
                    Spacer(Modifier.width(Fx.s2))
                    Text(label, color = if (selected) Fx.text else Fx.textDim)
                }
                val on = prefs?.on == true
                option(stringResource(R.string.own_server_fortis), !on) { vm.useOwnServer(chain, false) }
                option(
                    host?.let { stringResource(R.string.own_server_yours, it) } ?: stringResource(R.string.own_server_yours_unset),
                    on,
                ) { if (url != null) vm.useOwnServer(chain, true) else editing = chain }
                if (busy) Text(stringResource(R.string.own_server_testing), style = FxType.meta, color = Fx.textDim)
                if (editing != chain) vm.ownServerError[chain]?.let { ErrorText(ownServerErrorText(it)) }
                if (url != null) Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                    GhostButton(stringResource(R.string.action_change), Modifier.weight(1f), dense = true, enabled = !busy) { editing = chain }
                    GhostButton(stringResource(R.string.action_remove), Modifier.weight(1f), tint = Fx.bad, dense = true, enabled = !busy) {
                        vm.removeOwnServer(chain)
                    }
                }
            }
        }
    }
    editing?.let { chain -> OwnServerDialog(vm, chain) { editing = null } }
}

/** Paste a server URL; Test & save checks it and, if it passes, switches to it. */
@Composable
private fun OwnServerDialog(vm: WalletViewModel, chain: String, onDone: () -> Unit) {
    var text by remember(chain) { mutableStateOf(vm.ownServers[chain]?.url ?: "") }
    var tried by remember(chain) { mutableStateOf(false) }
    val busy = vm.ownServerBusy == chain
    // A finished check with no error for this chain means it saved and switched.
    LaunchedEffect(busy) {
        if (tried && !busy && vm.ownServerError[chain] == null) onDone()
    }
    AlertDialog(
        onDismissRequest = onDone,
        containerColor = Fx.panel,
        title = { Text(stringResource(R.string.own_server_dialog_title, chain.uppercase()), color = Fx.text) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
                Field(text, { text = it }, stringResource(R.string.own_server_hint), mono = true, keyboardType = KeyboardType.Uri)
                if (busy) Text(stringResource(R.string.own_server_testing), style = FxType.meta, color = Fx.textDim)
                else if (tried) vm.ownServerError[chain]?.let { ErrorText(ownServerErrorText(it)) }
            }
        },
        confirmButton = {
            TextButton({ tried = true; vm.saveOwnServer(chain, text) }, enabled = !busy && text.isNotBlank()) {
                Text(stringResource(R.string.own_server_test_save), color = Fx.signal)
            }
        },
        dismissButton = { TextButton(onDone) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
    )
}

@Composable
private fun CurrencyCard(vm: WalletViewModel) {
    var open by remember { mutableStateOf(false) }
    val automatic = stringResource(R.string.currency_automatic, defaultFiat())
    GlassCard {
        Row(
            Modifier.fillMaxWidth().clickable { open = true },
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(stringResource(R.string.settings_currency), color = Fx.text, fontWeight = FontWeight.SemiBold)
            Text(if (vm.fiatChosen) vm.fiat else automatic, color = Fx.textDim)
        }
    }
    if (open) CurrencyPicker(
        chosen = if (vm.fiatChosen) vm.fiat else null,
        onPick = { open = false; vm.chooseFiat(it) },
        onDismiss = { open = false },
    )
}

/** The currency list: "Automatic (EUR)" then each currency as symbol · code ·
 *  name. [chosen] null = Automatic. Public so the debug preview can show it. */
@Composable
fun CurrencyPicker(chosen: String?, onPick: (String?) -> Unit, onDismiss: () -> Unit) {
    val automatic = stringResource(R.string.currency_automatic, defaultFiat())
    val entries = remember { listOf<String?>(null) + FIAT_CODES }
    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = Fx.panel,
        title = { Text(stringResource(R.string.settings_currency), color = Fx.text) },
        text = {
            LazyColumn(Modifier.heightIn(max = 420.dp)) {
                items(entries) { code ->
                    val selected = code == chosen
                    Row(
                        Modifier.fillMaxWidth()
                            .heightIn(min = 48.dp)
                            .clickable { onPick(code) }
                            .padding(vertical = 10.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            code?.let { fiatSymbol(it) } ?: "",
                            color = if (selected) Fx.signal else Fx.textDim,
                            fontWeight = FontWeight.SemiBold,
                            textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                            modifier = Modifier.width(40.dp),
                        )
                        Text(
                            code?.let { fiatLabel(it) } ?: automatic,
                            color = if (selected) Fx.signal else Fx.text,
                            modifier = Modifier.padding(start = 8.dp),
                        )
                    }
                }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onDismiss) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
    )
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
