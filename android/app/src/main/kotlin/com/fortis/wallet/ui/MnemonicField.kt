package com.fortis.wallet.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.fortis.wallet.R
import com.fortis.wallet.ui.theme.Fx
import uniffi.wallet_ffi.bip39Wordlist

private val BIP39: List<String> by lazy(LazyThreadSafetyMode.PUBLICATION) { bip39Wordlist() }
private val BIP39_SET: Set<String> by lazy(LazyThreadSafetyMode.PUBLICATION) { BIP39.toHashSet() }

/** Recovery-phrase input: a roomy multi-line box that offers BIP-39 completions
 *  for the word at the cursor. Forces lowercase; password keyboard type so the
 *  IME shows no suggestion / clipboard / autofill strip over our own chips. */
@Composable
fun MnemonicField(
    value: String,
    onValueChange: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    var tfv by remember { mutableStateOf(TextFieldValue(value, TextRange(value.length))) }

    fun push(next: TextFieldValue) {
        val v = if (next.text.any { it.isUpperCase() }) next.copy(text = next.text.lowercase()) else next
        tfv = v
        onValueChange(v.text)
    }

    val cursor = tfv.selection.start.coerceIn(0, tfv.text.length)
    val partial = tfv.text.take(cursor).takeLastWhile { !it.isWhitespace() }
    val suggestions = remember(partial) {
        if (partial.isEmpty()) emptyList() else BIP39.filter { it.startsWith(partial) }.take(8)
    }
    val exactOnly = suggestions.size == 1 && suggestions[0] == partial

    fun choose(word: String) {
        val start = cursor - partial.length
        val text = tfv.text
        val next = text.substring(0, start) + word + " " + text.substring(cursor)
        push(TextFieldValue(next, TextRange(start + word.length + 1)))
    }

    val words = tfv.text.trim().split(Regex("\\s+")).filter { it.isNotBlank() }
    // don't flag the last word while it's still a valid prefix (user mid-type)
    val bad = words.filterIndexed { i, w ->
        w !in BIP39_SET && !(i == words.lastIndex && BIP39.any { it.startsWith(w) })
    }.size
    val ok = words.size == 12 || words.size == 24

    Column(modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(Fx.s1)) {
        Text(stringResource(R.string.reveal_title), color = Fx.textDim, style = MaterialTheme.typography.labelMedium)
        OutlinedTextField(
            value = tfv,
            onValueChange = { push(it) },
            singleLine = false,
            minLines = 5,
            keyboardOptions = KeyboardOptions(
                autoCorrectEnabled = false,
                capitalization = KeyboardCapitalization.None,
                keyboardType = KeyboardType.Password,
            ),
            textStyle = LocalTextStyle.current.copy(color = Fx.text, fontFamily = FontFamily.Monospace),
            colors = OutlinedTextFieldDefaults.colors(
                focusedTextColor = Fx.text,
                unfocusedTextColor = Fx.text,
                cursorColor = Fx.accent,
                focusedBorderColor = Fx.accent,
                unfocusedBorderColor = Fx.hair,
                focusedContainerColor = Fx.glass1,
                unfocusedContainerColor = Fx.glass1,
            ),
            modifier = Modifier.fillMaxWidth(),
        )
        if (suggestions.isNotEmpty() && !exactOnly) {
            Row(
                Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(Fx.s1),
            ) {
                suggestions.forEach { w ->
                    Box(
                        Modifier
                            .clip(RoundedCornerShape(Fx.pill))
                            .background(Fx.glass2)
                            .border(1.dp, Fx.hair, RoundedCornerShape(Fx.pill))
                            .clickable { choose(w) }
                            .padding(horizontal = 12.dp, vertical = 7.dp),
                    ) { Text(w, color = Fx.text, fontFamily = FontFamily.Monospace, fontSize = 13.sp) }
                }
            }
        }
        val count = pluralStringResource(R.plurals.word_count, words.size, words.size)
        Text(
            when {
                bad > 0 -> stringResource(R.string.mnemonic_status, count, stringResource(R.string.mnemonic_not_in_list, bad))
                ok -> stringResource(R.string.mnemonic_status, count, stringResource(R.string.mnemonic_looks_good))
                else -> count
            },
            color = if (bad > 0) Fx.warn else Fx.textFaint,
            fontSize = 12.sp,
        )
    }
}
