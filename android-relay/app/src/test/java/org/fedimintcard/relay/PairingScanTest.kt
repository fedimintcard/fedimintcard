package org.fedimintcard.relay

import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.RGBLuminanceSource
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeWriter
import org.junit.Assert.assertEquals
import org.junit.Test

class PairingScanTest {
    private val text = "fedimintcard-till:http://10.0.0.177:9877?t=" + "ab".repeat(32)

    private fun decode(inverted: Boolean): String {
        val m = QRCodeWriter().encode(text, BarcodeFormat.QR_CODE, 300, 300)
        val px = IntArray(m.width * m.height) { i ->
            val dark = m.get(i % m.width, i / m.width) != inverted
            if (dark) 0xFF000000.toInt() else 0xFFFFFFFF.toInt()
        }
        val src = RGBLuminanceSource(m.width, m.height, px)
        return ScanActivity.pairingReader().decodeWithState(BinaryBitmap(HybridBinarizer(src))).text
    }

    @Test fun readsAPrintedCode() = assertEquals(text, decode(inverted = false))

    /** A terminal with a dark background shows the code light-on-dark. */
    @Test fun readsATerminalCodeLightOnDark() = assertEquals(text, decode(inverted = true))
}
