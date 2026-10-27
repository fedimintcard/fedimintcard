package org.fedimintcard.relay

/** Keypad entry in whole sats. Pure, no Android. */
class AmountPad {
    private var digits = ""

    fun press(d: Int) {
        if (d !in 0..9) return
        val next = digits + d
        val stripped = next.trimStart('0')
        if (stripped.length > MAX_DIGITS) return
        digits = stripped
    }

    fun backspace() { digits = digits.dropLast(1) }
    fun clear() { digits = "" }

    val sats: Long get() = if (digits.isEmpty()) 0L else digits.toLong()
    val msat: Long get() = satsToMsat(sats)
    val isChargeable: Boolean get() = sats > 0
    val display: String get() = formatSats(msat)

    companion object { const val MAX_DIGITS = 9 }
}
