package com.fortis.wallet.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.material3.Text
import androidx.compose.ui.res.stringResource
import com.fortis.wallet.R
import com.fortis.wallet.security.backupAnswersMatch
import com.fortis.wallet.security.backupChallenge
import com.fortis.wallet.ui.theme.Fx
import com.fortis.wallet.ui.theme.FxType

/**
 * "Re-type a few words of your recovery phrase": proof the user really holds the phrase,
 * not just a tick in a box. [onVerified] runs once the asked words match. When [verified]
 * is already true it only shows the confirmation.
 */
@Composable
fun BackupQuiz(words: List<String>, verified: Boolean, onVerified: () -> Unit) {
    if (verified) {
        Text(stringResource(R.string.backup_quiz_ok), style = FxType.meta, color = Fx.signal)
        return
    }
    val indices = remember(words) { backupChallenge(words.size) }
    var answers by remember(words) { mutableStateOf(List(indices.size) { "" }) }
    var wrong by remember(words) { mutableStateOf(false) }
    Column(verticalArrangement = Arrangement.spacedBy(Fx.s2)) {
        Text(stringResource(R.string.backup_quiz_title), color = Fx.text)
        Text(stringResource(R.string.backup_quiz_body), style = FxType.meta, color = Fx.textDim)
        indices.forEachIndexed { i, wordIndex ->
            // Hidden like a password: the keyboard must not learn the phrase.
            Field(
                answers[i],
                { v -> answers = answers.toMutableList().also { it[i] = v }; wrong = false },
                stringResource(R.string.backup_quiz_word, wordIndex + 1),
                password = true,
            )
        }
        if (wrong) Text(stringResource(R.string.backup_quiz_wrong), style = FxType.meta, color = Fx.bad)
        PrimaryButton(stringResource(R.string.backup_quiz_check)) {
            if (backupAnswersMatch(words, indices, answers)) onVerified() else wrong = true
        }
    }
}
