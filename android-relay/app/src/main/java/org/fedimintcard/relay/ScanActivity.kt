@file:Suppress("DEPRECATION")

package org.fedimintcard.relay

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.hardware.Camera
import android.os.Bundle
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.widget.FrameLayout
import android.widget.TextView
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.MultiFormatReader
import com.google.zxing.NotFoundException
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.common.HybridBinarizer

/**
 * Scans the till's pairing QR code (`till pair`) and returns its text as [EXTRA_TEXT].
 *
 * The camera is read through the platform `android.hardware.Camera` preview frames and decoded
 * with zxing-core, so it needs no Play Services (the test Pixel's user 0 has none).
 */
@Suppress("DEPRECATION")
class ScanActivity : Activity(), SurfaceHolder.Callback, Camera.PreviewCallback {
    private var camera: Camera? = null
    private lateinit var surface: SurfaceView
    private lateinit var hint: TextView
    private val reader = pairingReader()
    @Volatile private var decoding = false
    @Volatile private var done = false
    private var hasSurface = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        surface = SurfaceView(this)
        hint = TextView(this).apply {
            text = "Scan the QR code the till printed (till pair)"
            setTextColor(Color.WHITE); setBackgroundColor(0x99000000.toInt()); setPadding(32, 24, 32, 24)
            gravity = Gravity.CENTER
        }
        setContentView(FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(surface, FrameLayout.LayoutParams(-1, -1))
            addView(hint, FrameLayout.LayoutParams(-1, -2, Gravity.BOTTOM))
        })
        surface.holder.addCallback(this)
    }

    override fun onResume() {
        super.onResume()
        if (checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), 1)
            return
        }
        if (hasSurface) startCamera()
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (grantResults.firstOrNull() != PackageManager.PERMISSION_GRANTED) {
            hint.text = "No camera permission: paste the pairing text instead"
            return
        }
        if (hasSurface) startCamera()
    }

    override fun onPause() {
        stopCamera()
        super.onPause()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        hasSurface = true
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) startCamera()
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        hasSurface = false
        stopCamera()
    }

    private fun startCamera() {
        if (camera != null) return
        val c = try { Camera.open() } catch (e: Exception) { hint.text = "Camera unavailable: paste the pairing text instead"; return } ?: return
        try {
            val p = c.parameters
            // Portrait: the preview's width runs along the screen's height. Prefer the size whose
            // shape is closest to the screen's, then the largest, so the letterbox is small.
            val root = surface.parent as View
            val screen = if (root.width > 0) root.height.toDouble() / root.width else 16.0 / 9
            val size = p.supportedPreviewSizes.filter { it.width <= 1920 }
                .sortedWith(compareBy<Camera.Size> { Math.abs(it.width.toDouble() / it.height - screen) }.thenByDescending { it.width * it.height })
                .firstOrNull()
            if (size != null) {
                p.setPreviewSize(size.width, size.height)
                fitPreview(root, size.height, size.width)
            }
            if (Camera.Parameters.FOCUS_MODE_CONTINUOUS_PICTURE in p.supportedFocusModes) p.focusMode = Camera.Parameters.FOCUS_MODE_CONTINUOUS_PICTURE
            c.parameters = p
            c.setDisplayOrientation(90) // portrait activity, back camera
            c.setPreviewDisplay(surface.holder)
            c.setPreviewCallback(this)
            c.startPreview()
            camera = c
        } catch (e: Exception) {
            c.release()
            hint.text = "Camera failed: paste the pairing text instead"
        }
    }

    /**
     * Size the preview to the camera's own shape, centred: a SurfaceView stretches the frames to
     * whatever size it is given, and the screen is taller than any preview size (11/4: stretched).
     */
    private fun fitPreview(root: View, w: Int, h: Int) {
        if (root.width == 0 || root.height == 0) return
        val scale = minOf(root.width.toDouble() / w, root.height.toDouble() / h)
        surface.layoutParams = FrameLayout.LayoutParams((w * scale).toInt(), (h * scale).toInt(), Gravity.CENTER)
    }

    private fun stopCamera() {
        camera?.let { try { it.setPreviewCallback(null); it.stopPreview() } catch (_: Exception) {}; it.release() }
        camera = null
    }

    @Deprecated("Deprecated in Java")
    override fun onPreviewFrame(data: ByteArray, cam: Camera) {
        if (decoding || done) return
        val size = try { cam.parameters.previewSize } catch (_: Exception) { return }
        decoding = true
        Thread {
            try {
                val src = PlanarYUVLuminanceSource(data, size.width, size.height, 0, 0, size.width, size.height, false)
                val text = try { reader.decodeWithState(BinaryBitmap(HybridBinarizer(src))).text } catch (_: NotFoundException) { null } finally { reader.reset() }
                if (text != null && parsePairing(text) != null && !done) {
                    done = true
                    runOnUiThread {
                        setResult(RESULT_OK, Intent().putExtra(EXTRA_TEXT, text))
                        finish()
                    }
                } else if (text != null) runOnUiThread { hint.text = "That QR code is not a till pairing code" }
            } catch (_: Exception) {
            } finally {
                decoding = false
            }
        }.start()
    }

    companion object {
        const val EXTRA_TEXT = "pairing"

        /**
         * `till pair` prints its QR in a terminal, which is usually dark: light modules on black,
         * the inverse of a printed code. zxing reads that only with ALSO_INVERTED (11/4: the
         * iPhone scanned it, the Pixel never did).
         */
        fun pairingReader() = MultiFormatReader().apply {
            setHints(
                mapOf(
                    DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE),
                    DecodeHintType.TRY_HARDER to true,
                    DecodeHintType.ALSO_INVERTED to true,
                )
            )
        }
    }
}
