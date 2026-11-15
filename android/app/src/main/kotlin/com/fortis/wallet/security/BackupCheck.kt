package com.fortis.wallet.security

import com.fortis.wallet.data.WalletConfig
import kotlin.random.Random

/**
 * Recovery-phrase backup checks.
 *
 * The phone's screen lock wraps the app's unlock key; if the lock is removed Android
 * deletes that key and the on-device wallets can never be opened again. The recovery
 * phrase is then the only way back, so the app makes the user prove they hold it:
 * re-type a few random words at creation, and keep a warning on any wallet with funds
 * until that has been done.
 */

/** How many words the user is asked to re-type. */
const val BACKUP_CHALLENGE_WORDS = 3

/** [n] distinct word positions out of [wordCount], ascending (0-based). Fewer when the
 *  phrase is shorter than [n]. */
fun backupChallenge(wordCount: Int, random: Random = Random.Default, n: Int = BACKUP_CHALLENGE_WORDS): List<Int> =
    (0 until wordCount).shuffled(random).take(n).sorted()

/** True when every asked position was answered with its word (case and surrounding space
 *  ignored). An empty challenge never passes. */
fun backupAnswersMatch(words: List<String>, indices: List<Int>, answers: List<String>): Boolean =
    indices.isNotEmpty() && answers.size == indices.size &&
        indices.indices.all { i ->
            val w = words.getOrNull(indices[i]) ?: return false
            answers[i].trim().equals(w.trim(), ignoreCase = true) && answers[i].isNotBlank()
        }

/**
 * Should the app warn that this wallet has no verified recovery-phrase backup?
 * Only a wallet that holds a seed here can be lost with the app key. An on-chain wallet
 * warns once it has funds; a Lightning wallet warns while unconfirmed, because its
 * balance is not part of [balanceSat].
 */
fun needsBackupWarning(c: WalletConfig, balanceSat: Long?): Boolean = when {
    c.watchOnly || c.hardware || c.sealed == null -> false
    c.backupConfirmed -> false
    c.isLightning -> true
    else -> (balanceSat ?: 0L) > 0L
}
