package org.fedimintcard.relay

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Bundle
import android.util.Size
import android.view.Gravity
import android.widget.FrameLayout
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.MultiFormatReader
import com.google.zxing.NotFoundException
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.common.HybridBinarizer
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * Scans the till's pairing QR code (`till pair`) and returns its text as [EXTRA_TEXT].
 *
 * CameraX shows the preview (cropped to fill, never stretched) and hands 1280×720 frames to
 * zxing-core on one background thread, newest frame only. No Play Services (the test Pixel's
 * user 0 has none).
 */
class ScanActivity : ComponentActivity() {
    private lateinit var preview: PreviewView
    private lateinit var hint: TextView
    private val reader = pairingReader()
    private val analysis: ExecutorService = Executors.newSingleThreadExecutor()
    @Volatile private var done = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        preview = PreviewView(this).apply { scaleType = PreviewView.ScaleType.FILL_CENTER }
        hint = TextView(this).apply {
            text = "Scan the QR code the till printed (till pair)"
            setTextColor(Color.WHITE); setBackgroundColor(0x99000000.toInt()); setPadding(32, 24, 32, 24)
            gravity = Gravity.CENTER
        }
        setContentView(FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(preview, FrameLayout.LayoutParams(-1, -1))
            addView(hint, FrameLayout.LayoutParams(-1, -2, Gravity.BOTTOM))
        })
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) startCamera()
        else requestPermissions(arrayOf(Manifest.permission.CAMERA), 1)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) startCamera()
        else hint.text = "No camera permission: paste the pairing text instead"
    }

    override fun onDestroy() {
        analysis.shutdown()
        super.onDestroy()
    }

    private fun startCamera() {
        val future = ProcessCameraProvider.getInstance(this)
        future.addListener({
            val provider = try { future.get() } catch (e: Exception) {
                hint.text = "Camera unavailable: paste the pairing text instead"; return@addListener
            }
            val shown = Preview.Builder().build().also { it.setSurfaceProvider(preview.surfaceProvider) }
            val frames = ImageAnalysis.Builder()
                .setResolutionSelector(
                    ResolutionSelector.Builder().setResolutionStrategy(
                        ResolutionStrategy(Size(1280, 720), ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER)
                    ).build()
                )
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .build()
                .also { it.setAnalyzer(analysis, ::analyze) }
            try {
                provider.unbindAll()
                provider.bindToLifecycle(this, CameraSelector.DEFAULT_BACK_CAMERA, shown, frames)
            } catch (e: Exception) {
                hint.text = "Camera failed: paste the pairing text instead"
            }
        }, mainExecutor)
    }

    private fun analyze(img: ImageProxy) {
        try {
            if (done) return
            // The Y plane is the luminance zxing reads; rows may be padded to rowStride.
            val plane = img.planes[0]
            val buf = plane.buffer
            val data = ByteArray(buf.remaining()).also { buf.get(it) }
            val src = PlanarYUVLuminanceSource(data, plane.rowStride, img.height, 0, 0, img.width, img.height, false)
            val text = try { reader.decodeWithState(BinaryBitmap(HybridBinarizer(src))).text }
                catch (_: NotFoundException) { null } finally { reader.reset() }
            if (text != null && parsePairing(text) != null && !done) {
                done = true
                runOnUiThread {
                    setResult(RESULT_OK, Intent().putExtra(EXTRA_TEXT, text))
                    finish()
                }
            } else if (text != null) runOnUiThread { hint.text = "That QR code is not a till pairing code" }
        } catch (_: Exception) {
        } finally {
            img.close()
        }
    }

    companion object {
        const val EXTRA_TEXT = "pairing"

        /**
         * `till pair` prints its QR in a terminal, which is usually dark: light modules on black,
         * the inverse of a printed code. zxing reads that only with ALSO_INVERTED.
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
