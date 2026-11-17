package com.carryfs.android

import android.animation.ValueAnimator
import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.view.View
import android.view.animation.DecelerateInterpolator
import kotlin.math.ceil

/** Space as a row of whole segments, used ones in ink. */
class CapacityBar(context: Context, private val used: Long, private val total: Long) : View(context) {
    companion object {
        const val SEGMENTS = 24
    }

    private val density = resources.displayMetrics.density
    private val usedPaint = Paint().apply { color = context.getColor(R.color.ink) }
    private val freePaint = Paint().apply { color = context.getColor(R.color.tonal) }

    /** Any used space shows at least one segment. */
    private val usedSegments =
        if (total <= 0 || used <= 0) 0 else ceil(used.toDouble() / total * SEGMENTS).toInt().coerceIn(1, SEGMENTS)
    private var shown = usedSegments.toFloat()

    /** Fill the used segments one by one, as when the volume first appears. */
    fun fill() {
        if (usedSegments == 0) return
        ValueAnimator.ofFloat(0f, usedSegments.toFloat()).apply {
            duration = 60L * usedSegments + 200
            interpolator = DecelerateInterpolator(2f)
            addUpdateListener { shown = it.animatedValue as Float; invalidate() }
        }.start()
    }

    override fun onMeasure(widthSpec: Int, heightSpec: Int) {
        setMeasuredDimension(MeasureSpec.getSize(widthSpec), (10 * density).toInt())
    }

    override fun onDraw(canvas: Canvas) {
        val gap = 2 * density
        val width = (width - gap * (SEGMENTS - 1)) / SEGMENTS
        for (i in 0 until SEGMENTS) {
            val left = i * (width + gap)
            canvas.drawRect(left, 0f, left + width, height.toFloat(), if (i < shown) usedPaint else freePaint)
        }
    }
}
