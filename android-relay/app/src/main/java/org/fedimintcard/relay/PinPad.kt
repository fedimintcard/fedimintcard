package org.fedimintcard.relay

/**
 * The holder's PIN, held only as characters this object owns. [wipe] overwrites them; the PIN is
 * never in a [String] here, never in [toString], and never written anywhere but the charge request.
 */
class Pin internal constructor(private var chars: CharArray) {
    val isWiped: Boolean get() = chars.isEmpty()

    /** Runs [block] on the digits. The array is this object's: do not keep it. */
    internal fun <T> withChars(block: (CharArray) -> T): T = block(chars)

    fun wipe() {
        chars.fill('\u0000')
        chars = CharArray(0)
    }

    override fun toString(): String = "Pin(****)"
}

/** Masked PIN entry for the holder: digits only, [MIN] to [MAX] of them. Pure, no Android. */
class PinPad {
    private val buf = CharArray(MAX)
    var length = 0
        private set

    fun press(d: Int) {
        if (d !in 0..9 || length >= MAX) return
        buf[length++] = '0' + d
    }

    fun backspace() {
        if (length == 0) return
        buf[--length] = '\u0000'
    }

    fun clear() {
        buf.fill('\u0000')
        length = 0
    }

    val isComplete: Boolean get() = length in MIN..MAX

    /** One dot per digit typed; never the digits. */
    val masked: String get() = "●".repeat(length)

    /** Moves the digits out into a [Pin] and clears the pad, or null while too short. */
    fun take(): Pin? {
        if (!isComplete) return null
        val p = Pin(buf.copyOf(length))
        clear()
        return p
    }

    override fun toString(): String = "PinPad(length=$length)"

    companion object {
        /** The applet's and the till's bounds (host/card PIN_MIN, PIN_MAX). */
        const val MIN = 4
        const val MAX = 8
    }
}
