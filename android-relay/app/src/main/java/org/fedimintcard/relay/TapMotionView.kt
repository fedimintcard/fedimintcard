package org.fedimintcard.relay

import android.animation.ValueAnimator
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.RadialGradient
import android.graphics.RectF
import android.graphics.Shader
import android.graphics.SweepGradient
import android.os.SystemClock
import android.util.AttributeSet
import android.view.View
import android.view.animation.PathInterpolator
import kotlin.math.PI
import kotlin.math.min
import kotlin.math.pow
import kotlin.math.sin

/**
 * The tap screen's motion. Two moods, one continuous drawing:
 *
 * - [Mode.Waiting]: contactless rings leave the centre one after another, an invitation to bring
 *   the card. Slow (2.4 s a ring) so it reads as calm, not as busy.
 * - [Mode.Working]: the card is on the phone. The rings gather into a ring and a gold arc runs
 *   round it, its length breathing, so a 4 to 6 second signature reads as steady progress.
 *
 * Switching mode morphs one into the other over 420 ms rather than cutting. With the system's
 * animations off, each mode draws one still frame.
 */
class TapMotionView @JvmOverloads constructor(ctx: Context, attrs: AttributeSet? = null) : View(ctx, attrs) {
    enum class Mode { Waiting, Working }

    private val dp = resources.displayMetrics.density
    private val gold = ctx.getColor(R.color.gold)
    private val goldHi = ctx.getColor(R.color.gold_hi)
    private val ink = ctx.getColor(R.color.ink)

    private val ringPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE; strokeWidth = 2 * dp; color = goldHi }
    private val trackPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE; strokeWidth = 3 * dp; color = ink }
    private val arcPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE; strokeWidth = 4 * dp; strokeCap = Paint.Cap.ROUND }
    private val dotPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = gold }
    private val glowPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val oval = RectF()
    private var glowFor = -1f

    private var mode = Mode.Waiting
    private var running = false
    /** 0 = waiting, 1 = working; eased between the two by [morph]. */
    private var blend = 0f
    private val morph = ValueAnimator.ofFloat(0f, 1f).apply {
        duration = 420
        interpolator = PathInterpolator(0.2f, 0f, 0f, 1f) // standard decelerate
        addUpdateListener { blend = it.animatedValue as Float; invalidate() }
    }
    private var t0 = SystemClock.uptimeMillis()

    fun setMode(m: Mode) {
        if (m == mode) return
        mode = m
        val to = if (m == Mode.Working) 1f else 0f
        if (!ValueAnimator.areAnimatorsEnabled()) { blend = to; invalidate(); return }
        morph.cancel()
        morph.setFloatValues(blend, to)
        morph.start()
    }

    fun start() {
        if (running) return
        running = true
        t0 = SystemClock.uptimeMillis()
        invalidate()
    }

    fun stop() {
        running = false
        morph.cancel()
        blend = if (mode == Mode.Working) 1f else 0f
    }

    override fun onDetachedFromWindow() { stop(); super.onDetachedFromWindow() }

    override fun onDraw(c: Canvas) {
        val cx = width / 2f
        val cy = height / 2f
        val r = min(width, height) * 0.36f
        if (r <= 0f) return
        val still = !ValueAnimator.areAnimatorsEnabled()
        val ms = if (still) 0L else SystemClock.uptimeMillis() - t0
        val waiting = 1f - blend
        val working = blend

        // Waiting: three rings, a third of a period apart, easing out as they widen and fade.
        if (waiting > 0.01f) {
            val period = 2400f
            for (i in 0 until 3) {
                val p = if (still) 0.35f + i * 0.2f else ((ms + i * period / 3f) % period) / period
                val eased = 1f - (1f - p).pow(3)
                val radius = r * (0.30f + 0.70f * eased)
                ringPaint.alpha = (255 * 0.6f * (1f - p).pow(1.6f) * waiting).toInt()
                c.drawCircle(cx, cy, radius, ringPaint)
            }
            val breath = if (still) 0f else sin(2 * PI * ms / 2400.0).toFloat()
            dotPaint.alpha = (255 * waiting).toInt()
            c.drawCircle(cx, cy, (7f + 1.5f * breath) * dp, dotPaint)
        }

        // Working: the ring grows in from the rings' start, then a gold arc runs round it.
        if (working > 0.01f) {
            val ringR = r * (0.55f + 0.30f * working)
            if (glowFor != ringR) {
                glowPaint.shader = RadialGradient(cx, cy, ringR, intArrayOf(withAlpha(gold, 70), withAlpha(gold, 0)), floatArrayOf(0f, 1f), Shader.TileMode.CLAMP)
                glowFor = ringR
            }
            val glow = if (still) 1f else 0.85f + 0.15f * sin(2 * PI * ms / 1600.0).toFloat()
            glowPaint.alpha = (255 * working * glow).toInt()
            c.drawCircle(cx, cy, ringR, glowPaint)

            trackPaint.alpha = (255 * 0.14f * working).toInt()
            c.drawCircle(cx, cy, ringR, trackPaint)

            // The head turns once every 1.2 s; the arc's length breathes between 40° and 150° over
            // 1.8 s, so the motion never settles into a mechanical spin.
            val head = if (still) 300f else (ms % 1200L) / 1200f * 360f
            val sweep = if (still) 120f else 95f + 55f * sin(2 * PI * ms / 1800.0).toFloat()
            val startAngle = head - sweep - 90f
            val stops = floatArrayOf(0f, sweep / 360f)
            arcPaint.shader = SweepGradient(cx, cy, intArrayOf(withAlpha(goldHi, 0), goldHi), stops).also {
                val m = android.graphics.Matrix()
                m.setRotate(startAngle, cx, cy)
                it.setLocalMatrix(m)
            }
            arcPaint.alpha = (255 * working).toInt()
            oval.set(cx - ringR, cy - ringR, cx + ringR, cy + ringR)
            c.drawArc(oval, startAngle, sweep, false, arcPaint)
        }

        if (running && !still) postInvalidateOnAnimation()
    }

    private fun withAlpha(color: Int, a: Int) = Color.argb(a, Color.red(color), Color.green(color), Color.blue(color))
}
