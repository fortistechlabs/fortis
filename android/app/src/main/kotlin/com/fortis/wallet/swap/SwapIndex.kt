package com.fortis.wallet.swap

import com.fortis.wallet.data.SwapHttpException

/** The edge's refusal of a recovery tag it already holds (`409`). */
const val INDEX_USED = "this swap index was used before"

/** The edge refused [e] because its recovery tag (so its swap index) was used before. */
fun indexUsed(e: SwapHttpException): Boolean = e.code == 409 && INDEX_USED in (e.message ?: "")

/**
 * Run [attempt] (a create or take that draws its own next swap index), and
 * when the edge says that index was used before — another phone with this
 * seed, or a restore whose counter is behind — first [onUsed] (catch the
 * counter up past the seed's used indices), then run it again on the next
 * index, up to [tries] times. Any other failure is the caller's.
 */
internal suspend fun <T> withFreshIndex(
    tries: Int = 5,
    onUsed: suspend () -> Unit = {},
    attempt: suspend () -> T,
): T {
    var n = 0
    while (true) {
        try {
            return attempt()
        } catch (e: SwapHttpException) {
            if (!indexUsed(e) || ++n >= tries) throw e
            onUsed()
        }
    }
}
