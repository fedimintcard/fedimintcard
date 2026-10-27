package org.fedimintcard.relay

fun satsToMsat(sats: Long): Long = sats * 1000L

private fun group(n: Long): String {
    val s = n.toString()
    val sb = StringBuilder()
    for ((i, c) in s.withIndex()) {
        if (i > 0 && (s.length - i) % 3 == 0) sb.append(',')
        sb.append(c)
    }
    return sb.toString()
}

/** Whole sats with thousands separators; up to 3 decimals only when msat is not a whole sat. */
fun formatSats(msat: Long): String {
    val neg = msat < 0
    val a = if (neg) -msat else msat
    val frac = a % 1000
    val whole = group(a / 1000)
    val f = if (frac != 0L) "." + frac.toString().padStart(3, '0').trimEnd('0') else ""
    return (if (neg) "-" else "") + whole + f
}

fun shortTxid(txid: String): String =
    if (txid.length <= 13) txid else txid.substring(0, 8) + "…" + txid.substring(txid.length - 4)
