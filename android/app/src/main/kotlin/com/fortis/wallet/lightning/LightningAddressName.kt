package com.fortis.wallet.lightning

/** The host of Lightning addresses (`name@fortis.cash`), the one place it is written. */
const val LN_ADDRESS_DOMAIN = "fortis.cash"

/** The chain as the edge's name operations spell it. */
fun uniffi.wallet_ffi.LnChain.wire(): String = when (this) {
    uniffi.wallet_ffi.LnChain.BTC -> "btc"
    uniffi.wallet_ffi.LnChain.XBT -> "xbt"
}

/** Why a name is refused; the same rules, in the same order, as the server's. */
enum class NameProblem(val serverReason: String) {
    Length("length"), Characters("characters"), Edges("edges"), DoubleDot("double_dot"), Reserved("reserved");

    companion object {
        fun fromServer(reason: String): NameProblem? = values().firstOrNull { it.serverReason == reason }
    }
}

private val RESERVED_NAMES = setOf(
    "abuse", "admin", "administrator", "api", "billing", "bitcoin", "btc", "contact", "dns", "ftp", "fortis", "help",
    "hostmaster", "imap", "info", "lightning", "lnurlp", "mail", "no-reply", "noreply", "postmaster", "pop", "root",
    "sales", "satoshi", "security", "smtp", "staff", "status", "support", "team", "wallet", "webmaster", "www", "xbt",
)

private fun isNameChar(c: Char) = c in 'a'..'z' || c in '0'..'9' || c == '.' || c == '_' || c == '-'
private fun isAlnum(c: Char) = c in 'a'..'z' || c in '0'..'9'

/** What the user typed, as it is checked and sent: trimmed and lowercased (the server refuses capitals). */
fun normalizeName(input: String): String = input.trim().lowercase(java.util.Locale.ROOT)

/** The first rule [name] breaks, or null when it is allowed. Check order: characters, length, edges, double dot, reserved. */
fun checkName(name: String): NameProblem? = when {
    !name.all(::isNameChar) -> NameProblem.Characters
    name.length !in 3..32 -> NameProblem.Length
    !isAlnum(name.first()) || !isAlnum(name.last()) -> NameProblem.Edges
    ".." in name -> NameProblem.DoubleDot
    name in RESERVED_NAMES -> NameProblem.Reserved
    else -> null
}

/** Whether a name can be claimed, from the public lookup; [Unknown] when that could not be told. */
enum class NameAvailability { Available, Taken, Unknown }

/** The public `/.well-known/lnurlp/<name>` answer: a `payRequest` means taken, an LNURL ERROR (HTTP 200)
 *  means free. Anything else (network, 5xx, not JSON) is unknown; the claim itself is the final word. */
fun parseAvailability(status: Int, body: String): NameAvailability {
    val o = runCatching { org.json.JSONObject(body) }.getOrNull() ?: return NameAvailability.Unknown
    return when {
        status in 200..299 && o.optString("tag") == "payRequest" -> NameAvailability.Taken
        status == 200 && o.optString("status") == "ERROR" -> NameAvailability.Available
        else -> NameAvailability.Unknown
    }
}
