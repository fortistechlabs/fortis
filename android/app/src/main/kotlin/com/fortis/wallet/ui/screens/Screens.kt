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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import com.fortis.wallet.BuildConfig
import com.fortis.wallet.MAX_WALLETS
import com.fortis.wallet.MAX_WALLET_NAME
import com.fortis.wallet.NavTab
import com.fortis.wallet.PlanPreview
import com.fortis.wallet.R
import com.fortis.wallet.WalletViewModel
import com.fortis.wallet.ui.*
import com.fortis.wallet.ui.theme.Fx
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
private fun amountToSat(text: String, sat: Boolean): Long? = runCatching {
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

private fun copyToClipboard(ctx: Context, label: String, text: String) {
    val cm = ctx.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    cm.setPrimaryClip(ClipData.newPlainText(label, text))
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
        Text(stringResource(R.string.gen_words_hint), color = Fx.textFaint, fontSize = 12.sp)
        val scribble = remember { Path() }
        var strokeRev by remember { mutableIntStateOf(0) }
        Box(
            Modifier.fillMaxWidth().height(150.dp).clip(RoundedCornerShape(Fx.rLg)).background(Fx.glass1)
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
                        scribble, Fx.accent,
                        style = Stroke(width = 3.5.dp.toPx(), cap = StrokeCap.Round),
                    )
                },
            contentAlignment = Alignment.Center,
        ) {
            if (strokeRev == 0) Text(stringResource(R.string.gen_pad_hint), color = Fx.textFaint, fontSize = 13.sp)
        }
        Box(Modifier.fillMaxWidth().height(8.dp).clip(RoundedCornerShape(Fx.pill)).background(Fx.glass1)) {
            Box(
                Modifier.fillMaxWidth(entropyProgress(bits)).fillMaxHeight()
                    .background(Brush.horizontalGradient(listOf(Fx.accent, Fx.accent2))),
            )
        }
        Text(
            if (entropyProgress(bits) >= 1f) stringResource(R.string.gen_entropy_full)
            else stringResource(R.string.gen_entropy_partial, bits),
            color = Fx.textFaint, fontSize = 12.sp,
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
        Text(stringResource(R.string.create_passphrase_hint), color = Fx.textFaint, fontSize = 12.sp)
        if (settingUp) LockChoiceFields(choice)
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(ack, { ack = it })
            Text(stringResource(R.string.create_ack), color = Fx.text)
        }
        ErrorText(vm.error)
        Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
            GhostButton(stringResource(R.string.action_back), Modifier.weight(1f)) { vm.cancelOnboard() }
            PrimaryButton(stringResource(R.string.action_continue), Modifier.weight(1f)) {
                val problem = when {
                    !ack -> ctx.getString(R.string.error_confirm_phrase)
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
            color = Fx.textFaint, fontSize = 12.sp,
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
                    "${row * 3 + col + 1}  $w", color = Fx.text,
                    fontFamily = FontFamily.Monospace, fontSize = 13.sp,
                    modifier = Modifier.weight(1f).background(Fx.glass1, RoundedCornerShape(8.dp)).padding(6.dp),
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
        Text(stringResource(R.string.reveal_warning), color = Fx.bad, fontSize = 13.sp)
        when {
            failed -> Text(stringResource(R.string.reveal_failed), color = Fx.bad)
            seed == null -> CircularProgressIndicator(Modifier.align(Alignment.CenterHorizontally))
            else -> {
                val (words, passphrase) = seed!!
                GlassCard { PhraseGrid(words) }
                if (passphrase.isNotEmpty()) {
                    Text(stringResource(R.string.reveal_passphrase_label), color = Fx.textDim, style = MaterialTheme.typography.labelMedium)
                    Text(
                        passphrase, color = Fx.text, fontFamily = FontFamily.Monospace,
                        modifier = Modifier.fillMaxWidth().clip(RoundedCornerShape(Fx.rSm)).background(Fx.glass1).padding(Fx.s4),
                    )
                } else {
                    Text(stringResource(R.string.reveal_no_passphrase), color = Fx.textFaint, fontSize = 12.sp)
                }
            }
        }
        PrimaryButton(stringResource(R.string.action_done)) { vm.closeReveal() }
    }
}

@Composable
fun Segmented(options: List<Pair<String, String>>, selected: String, onSelect: (String) -> Unit, modifier: Modifier = Modifier) {
    Row(modifier.background(Fx.glass1, RoundedCornerShape(Fx.pill)).padding(3.dp),
        horizontalArrangement = Arrangement.spacedBy(3.dp)) {
        options.forEach { (v, label) ->
            val on = v == selected
            Box(
                Modifier.weight(1f).clip(RoundedCornerShape(Fx.pill))
                    .background(if (on) Brush.linearGradient(listOf(Fx.accent, Fx.accent2)) else Brush.linearGradient(listOf(Color.Transparent, Color.Transparent)))
                    .clickable { onSelect(v) }.padding(vertical = 8.dp),
                contentAlignment = Alignment.Center,
            ) { Text(label, color = if (on) Color(0xFF0A0C16) else Color.White.copy(alpha = 0.75f), fontSize = 13.sp) }
        }
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
        while (vm.nav == NavTab.Home || vm.nav == NavTab.Settings) {
            vm.refreshAllBalances()
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
            NavBar(vm.nav) { vm.go(it) }
            Box(Modifier.weight(1f).fillMaxWidth()) {
                when (vm.nav) {
                    NavTab.Home -> HomeTab(vm)
                    NavTab.Wallet -> WalletTab(vm)
                    NavTab.Settings -> SettingsTab(vm)
                }
            }
        }
    }
    vm.pending?.let { ConfirmSheet(vm, it, unit) }
}

@Composable
private fun NavBar(current: NavTab, onSelect: (NavTab) -> Unit) {
    val labels = listOf(
        NavTab.Home to stringResource(R.string.nav_home),
        NavTab.Wallet to stringResource(R.string.nav_wallet),
        NavTab.Settings to stringResource(R.string.nav_settings),
    )
    Row(
        Modifier.fillMaxWidth().background(Fx.glass1, RoundedCornerShape(Fx.pill)).padding(3.dp),
        horizontalArrangement = Arrangement.spacedBy(3.dp),
    ) {
        labels.forEach { (t, label) ->
            val on = current == t
            Box(
                Modifier.weight(1f).clip(RoundedCornerShape(Fx.pill))
                    .background(
                        if (on) Brush.linearGradient(listOf(Fx.accent, Fx.accent2))
                        else Brush.linearGradient(listOf(Color.Transparent, Color.Transparent)),
                    )
                    .clickable { onSelect(t) }.padding(vertical = 10.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    label, fontSize = 13.sp,
                    fontWeight = if (on) FontWeight.Medium else FontWeight.Normal,
                    color = if (on) Color(0xFF0A0C16) else Color.White.copy(alpha = 0.75f),
                )
            }
        }
    }
}

/** Home tab — the list of wallets; tap one to open it. */
@Composable
private fun HomeTab(vm: WalletViewModel) = Column(
    Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(top = Fx.s2),
    verticalArrangement = Arrangement.spacedBy(Fx.s4),
) {
    Text(stringResource(R.string.home_title), style = MaterialTheme.typography.titleMedium, color = Fx.text)
    GlassCard {
        vm.wallets.forEach { w ->
            val current = w.id == vm.selectedId
            val wUnit = if (w.chain == "btc") "BTC" else "XBT"
            val bal = vm.walletBalances[w.id]
            Row(
                Modifier.fillMaxWidth().clip(RoundedCornerShape(Fx.rSm))
                    .background(if (current) Fx.glass2 else Fx.glass1)
                    .clickable { vm.selectWallet(w.id) }.padding(Fx.s3),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text(w.display, color = Fx.text, fontWeight = FontWeight.Medium)
                    Text(
                        bal?.let { "${fmt(it)} $wUnit" } ?: stringResource(R.string.home_tap_to_open),
                        color = if (bal != null) Fx.textDim else Fx.textFaint,
                        fontFamily = if (bal != null) FontFamily.Monospace else null,
                        fontSize = 12.sp,
                    )
                    bal?.let { vm.usdValue(it, w.chain) }?.let {
                        Text(stringResource(R.string.fiat_approx, fmtUsd(it)), color = Fx.textFaint, fontSize = 11.sp)
                    }
                }
                Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, null, tint = Fx.textDim)
            }
        }
    }
    if (vm.canAddWallet) PrimaryButton(stringResource(R.string.action_add_wallet)) { vm.addWallet() }
    else Text(stringResource(R.string.wallets_max, MAX_WALLETS), color = Fx.textFaint, fontSize = 12.sp)
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
                        .background(if (current) Fx.glass2 else Fx.glass1).padding(Fx.s3),
                    verticalArrangement = Arrangement.spacedBy(Fx.s2),
                ) {
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text(w.display, color = Fx.text, fontWeight = FontWeight.Medium)
                            Text(
                                bal?.let { "${fmt(it)} $wUnit" }
                                    ?: listOfNotNull(w.network.takeIf { it != "mainnet" }, if (current) stringResource(R.string.home_open) else null)
                                        .joinToString(" · ").ifEmpty { "…" },
                                color = if (bal != null) Fx.textDim else Fx.textFaint,
                                fontFamily = if (bal != null) FontFamily.Monospace else null,
                                fontSize = 12.sp,
                            )
                            bal?.let { vm.usdValue(it, w.chain) }?.let {
                                Text(stringResource(R.string.fiat_approx, fmtUsd(it)), color = Fx.textFaint, fontSize = 11.sp)
                            }
                        }
                        if (!current) TextButton({ vm.selectWallet(w.id) }) { Text(stringResource(R.string.action_open), color = Fx.accent) }
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                        GhostButton(stringResource(R.string.action_rename), Modifier.weight(1f), dense = true) { renaming = w.id }
                        GhostButton(stringResource(R.string.action_copy_xpub), Modifier.weight(1f), dense = true) {
                            val xpub = vm.accountKey(w.id)
                            if (xpub != null) copyToClipboard(ctx, "xpub", xpub)
                            else Toast.makeText(ctx, xpubLoading, Toast.LENGTH_SHORT).show()
                        }
                    }
                    GhostButton(stringResource(R.string.action_recovery_phrase), dense = true) { revealWarn = w.id }
                    val hasOther = vm.wallets.any { it.name == w.name && it.chain == w.otherChain }
                    if (!hasOther && vm.canAddWallet) GhostButton(stringResource(R.string.also_add_on, w.otherChain.uppercase()), dense = true) {
                        vm.cloneToOtherChain(w.id)
                    }
                    GhostButton(stringResource(R.string.action_remove), tint = Fx.bad, dense = true) { removing = w.id }
                }
            }
            if (vm.canAddWallet) PrimaryButton(stringResource(R.string.action_add_wallet), dense = true) { vm.addWallet() }
            else Text(stringResource(R.string.wallets_max, MAX_WALLETS), color = Fx.textFaint, fontSize = 12.sp)
            ErrorText(vm.error)
        }

        GlassCard {
            Text(stringResource(R.string.settings_security), color = Fx.text, fontWeight = FontWeight.SemiBold)
            kv(
                stringResource(R.string.settings_app_lock),
                if (vm.lockMode == com.fortis.wallet.data.LOCK_BIOMETRIC) stringResource(R.string.lock_mode_biometric)
                else stringResource(R.string.lock_mode_password),
            )
            Text(stringResource(R.string.settings_one_unlock), color = Fx.textFaint, fontSize = 12.sp)
            val keySec = remember(vm.lockMode) { vm.keySecurity() }
            if (keySec == com.fortis.wallet.data.SeedKeystore.KeySecurity.SOFTWARE) {
                Text(stringResource(R.string.settings_key_software), color = Fx.warn, fontSize = 12.sp)
            }
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
            if (st?.degraded == true) Text(stringResource(R.string.settings_degraded_note), color = Fx.textFaint, fontSize = 12.sp)
            Row(horizontalArrangement = Arrangement.spacedBy(Fx.s2)) {
                GhostButton(stringResource(R.string.action_refresh), Modifier.weight(1f), dense = true) { vm.refresh() }
                GhostButton(stringResource(R.string.action_reconnect), Modifier.weight(1f), dense = true) { vm.reconnect() }
            }
        }

        GhostButton(stringResource(R.string.action_lock_app), tint = Fx.bad, dense = true) { vm.lock() }
        Text(stringResource(R.string.settings_version, BuildConfig.VERSION_NAME), color = Fx.textFaint, fontSize = 11.sp)
    }

    renaming?.let { id ->
        val w = vm.wallets.firstOrNull { it.id == id }
        var text by remember(id) { mutableStateOf(w?.name ?: "") }
        AlertDialog(
            onDismissRequest = { renaming = null },
            containerColor = Fx.bg1,
            title = { Text(stringResource(R.string.dialog_rename_title), color = Fx.text) },
            text = { Field(text, { text = it }, stringResource(R.string.field_wallet_name), maxLen = MAX_WALLET_NAME) },
            confirmButton = { TextButton({ vm.renameWallet(id, text); renaming = null }) { Text(stringResource(R.string.action_save), color = Fx.accent) } },
            dismissButton = { TextButton({ renaming = null }) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
        )
    }

    revealWarn?.let { id ->
        val w = vm.wallets.firstOrNull { it.id == id }
        AlertDialog(
            onDismissRequest = { revealWarn = null },
            containerColor = Fx.bg1,
            title = { Text(stringResource(R.string.dialog_reveal_title, w?.display ?: stringResource(R.string.nav_wallet)), color = Fx.text) },
            text = { Text(stringResource(R.string.dialog_reveal_body), color = Fx.textDim) },
            confirmButton = { TextButton({ vm.startReveal(id); revealWarn = null }) { Text(stringResource(R.string.action_show), color = Fx.accent) } },
            dismissButton = { TextButton({ revealWarn = null }) { Text(stringResource(R.string.action_cancel), color = Fx.text) } },
        )
    }

    removing?.let { id ->
        val w = vm.wallets.firstOrNull { it.id == id }
        AlertDialog(
            onDismissRequest = { removing = null },
            containerColor = Fx.bg1,
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
    var tab by remember { mutableStateOf(0) }
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
        GlassCard(fill = Fx.glassHero, corner = Fx.rLg) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Column {
                    Text(c.display, color = Fx.textDim, fontSize = 12.sp)
                    Row(verticalAlignment = Alignment.Bottom) {
                        Text(if (b != null) fmt(countUpSat(b.confirmedSat)) else "—",
                            fontFamily = FontFamily.Monospace, fontWeight = FontWeight.SemiBold, fontSize = 32.sp,
                            color = Fx.text)
                        Spacer(Modifier.width(6.dp))
                        Text(unit, color = Fx.textDim, fontSize = 12.sp)
                    }
                    b?.let { bal ->
                        vm.usdValue(bal.confirmedSat, c.chain)?.let {
                            Text(stringResource(R.string.fiat_approx, fmtUsd(it)), color = Fx.textDim, fontSize = 12.sp)
                        }
                    }
                    if (b != null && b.pendingSat != 0L) Text(
                        stringResource(
                            R.string.wallet_pending,
                            (if (b.pendingSat > 0) "+" else "") + fmt(b.pendingSat), unit,
                        ),
                        color = Fx.warn, fontFamily = FontFamily.Monospace, fontSize = 12.sp,
                    )
                    val hint = vm.status?.let {
                        val head = when {
                            it.scanningPct != null -> stringResource(R.string.wallet_rescanning, it.scanningPct)
                            it.synced -> stringResource(R.string.wallet_block, it.blocks.toString())
                            else -> stringResource(R.string.wallet_syncing)
                        }
                        if (it.degraded) stringResource(R.string.wallet_limited_suffix, head) else head
                    } ?: stringResource(R.string.wallet_connecting)
                    Text(hint, color = Fx.textFaint, fontSize = 12.sp)
                }
                TextButton({ vm.lock() }) { Text(stringResource(R.string.action_lock), color = Fx.textDim) }
            }
        }
        val tabs = listOf(
            stringResource(R.string.tab_receive),
            stringResource(R.string.tab_send),
            stringResource(R.string.tab_history),
        )
        Row(Modifier.background(Fx.glass1, RoundedCornerShape(Fx.pill)).padding(3.dp)) {
            tabs.forEachIndexed { i, label ->
                Box(Modifier.weight(1f).clip(RoundedCornerShape(Fx.pill))
                    .background(if (tab == i) Brush.linearGradient(listOf(Fx.accent, Fx.accent2)) else Brush.linearGradient(listOf(Color.Transparent, Color.Transparent)))
                    .clickable { tab = i }.padding(vertical = 8.dp), contentAlignment = Alignment.Center) {
                    Text(label, color = if (tab == i) Color(0xFF0A0C16) else Color.White.copy(alpha = 0.75f), fontSize = 13.sp)
                }
            }
        }
        vm.lastSentTxid?.let { txid -> SentBanner(vm, txid) }
        Column(
            Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(Fx.s4),
        ) {
            when (tab) {
                0 -> ReceiveTab(vm)
                1 -> SendTab(vm)
                else -> HistoryTab(vm, unit)
            }
        }
    }
}

@Composable
private fun SentBanner(vm: WalletViewModel, txid: String) {
    val ctx = LocalContext.current
    val c = vm.config
    val url = c?.let { explorerTxUrl(it.chain, it.network, txid) }
    GlassCard {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(stringResource(R.string.sent_title), color = Fx.good, fontWeight = FontWeight.Medium)
                Text(
                    stringResource(R.string.sent_txid, txid.take(12)),
                    color = Fx.textFaint, fontSize = 12.sp,
                    modifier = Modifier.clickable { copyToClipboard(ctx, "txid", txid) },
                )
            }
            if (url != null) TextButton({ openInBrowser(ctx, url) }) { Text(stringResource(R.string.action_view), color = Fx.accent) }
            IconButton({ vm.dismissLastSent() }) { Icon(Icons.Filled.Close, stringResource(R.string.cd_dismiss), tint = Fx.textDim) }
        }
    }
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
                color = Fx.textFaint,
                fontSize = 12.sp,
                modifier = Modifier.align(Alignment.CenterHorizontally),
            )
        }
        Text(
            addr,
            fontFamily = FontFamily.Monospace,
            color = Fx.text,
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(Fx.rSm))
                .background(Fx.glass1)
                .clickable { copy() }
                .padding(Fx.s4),
        )
        Text(stringResource(R.string.receive_address_n, i), color = Fx.textFaint, fontSize = 12.sp)
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
            Text(stringResource(R.string.field_amount), color = Fx.textDim, style = MaterialTheme.typography.labelMedium)
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
                Text(alt + fiat, color = Fx.textFaint, fontSize = 12.sp)
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(feeFromAmount, { feeFromAmount = it })
                Text(stringResource(R.string.send_fee_from_amount), color = Fx.text)
            }
            if (feeFromAmount) Text(
                stringResource(R.string.send_fee_from_amount_note),
                color = Fx.textFaint, fontSize = 12.sp,
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
            if (replayProtect) Text(stringResource(R.string.send_replay_note), color = Fx.textFaint, fontSize = 12.sp)
        }
        vm.status?.pricing?.let {
            Text(
                stringResource(R.string.send_service_fee, "%.2f".format(it.bps / 100.0), it.floorSat),
                color = Fx.textFaint, fontSize = 12.sp,
            )
        }
        ErrorText(vm.error)
        PrimaryButton(
            stringResource(R.string.action_review),
            enabled = to.isNotBlank() && (sweep || (amountSat != null && amountSat > 0)),
        ) {
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
                .background(Fx.glass1)
                .border(1.dp, Fx.hair, RoundedCornerShape(Fx.rSm))
                .clickable { open = true }
                .padding(start = Fx.s3, end = Fx.s1),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(if (isSat) satLabel else coinUnit, color = Fx.text, fontSize = 14.sp)
            Icon(Icons.Filled.ArrowDropDown, contentDescription = stringResource(R.string.cd_amount_unit), tint = Fx.textDim)
        }
        DropdownMenu(
            expanded = open,
            onDismissRequest = { open = false },
            containerColor = Fx.bg1,
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
                Column {
                    Row(verticalAlignment = Alignment.Bottom) {
                        Text((if (h.amountSat > 0) "+" else "") + fmt(h.amountSat) + " " + unit,
                            fontFamily = FontFamily.Monospace, color = if (h.amountSat > 0) Fx.good else Fx.text)
                        if (h.amountSat != 0L) vm.usdValue(if (h.amountSat < 0) -h.amountSat else h.amountSat, chain)?.let {
                            Spacer(Modifier.width(6.dp))
                            Text(stringResource(R.string.fiat_approx, fmtUsd(it)), color = Fx.textFaint, fontSize = 11.sp)
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
                        color = Fx.textFaint, fontSize = 12.sp,
                    )
                }
                Text(
                    when {
                        h.confirmations < 1 -> stringResource(R.string.history_pending)
                        h.confirmations < 6 -> stringResource(R.string.history_conf_n, h.confirmations.toInt())
                        else -> stringResource(R.string.history_confirmed)
                    },
                    color = if (h.confirmations < 1) Fx.warn else Fx.textDim, fontSize = 12.sp,
                )
            }
        }
        Text(
            stringResource(R.string.history_hint),
            color = Fx.textFaint, fontSize = 11.sp, modifier = Modifier.padding(top = Fx.s2),
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ConfirmSheet(vm: WalletViewModel, p: PlanPreview, unit: String) {
    ModalBottomSheet(onDismissRequest = { vm.cancelPending() }, containerColor = Fx.bg1) {
        Column(Modifier.padding(Fx.s4).padding(bottom = Fx.s5), verticalArrangement = Arrangement.spacedBy(Fx.s3)) {
            Text(
                if (p.sweep) stringResource(R.string.confirm_sweep_title) else stringResource(R.string.confirm_payment_title),
                style = MaterialTheme.typography.titleMedium, color = Fx.text,
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

            kv(stringResource(R.string.confirm_to), p.to)
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
                GhostButton(stringResource(R.string.action_cancel), Modifier.weight(1f)) { vm.cancelPending() }
                PrimaryButton(stringResource(R.string.action_sign_send), Modifier.weight(1f)) { vm.confirmSend() }
            }
        }
    }
}

@Composable
private fun kv(k: String, v: String) = Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
    Text(k, color = Fx.textDim)
    Text(v, color = Fx.text, modifier = Modifier.padding(start = Fx.s4), textAlign = androidx.compose.ui.text.style.TextAlign.End)
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
            containerColor = Fx.bg1,
            title = { Text(stringResource(R.string.settings_language), color = Fx.text) },
            text = {
                LazyColumn(Modifier.heightIn(max = 420.dp)) {
                    items(entries) { tag ->
                        val selected = tag == current
                        Text(
                            tag?.let { localeLabel(it) } ?: systemDefault,
                            color = if (selected) Fx.accent else Fx.text,
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
