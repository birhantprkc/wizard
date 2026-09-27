package com.teddytennant.wizard.ui.life

import android.provider.Settings
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalInspectionMode
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.teddytennant.wizard.ui.theme.WizardTheme
import kotlin.random.Random

/** How long a dying cell takes to fade out. */
private const val FADE_NANOS = 280_000_000f

/** Dead cells stay faintly visible, so the indicator reads as a board. */
private const val GHOST = 0.08f

/**
 * A tiny Game of Life on a torus, the app's wait indicator. It runs about
 * seven generations a second from one frame loop that allocates nothing,
 * pauses when the screen stops, and holds still when the system turns
 * animations off.
 */
@Composable
fun LifeIndicator(
    modifier: Modifier = Modifier,
    size: Dp = 16.dp,
    cells: Int = 6,
    color: Color = WizardTheme.colors.muted,
    seed: LifeSeed = LifeSeed.Glider,
    /** Generations to run before the first frame, so a still render isn't just the seed. */
    warmup: Int = 0,
    generationsPerSecond: Float = 7f,
    contentDescription: String = "Working",
) {
    val board = remember(cells, seed) {
        LifeBoard(cells, cells, Random(cells * 31 + seed.ordinal)).apply {
            // Centred, so a still frame shows the whole pattern rather than one split across the wrap.
            seed(seed, cells / 2 - 2, cells / 2 - 2)
            repeat(warmup) { advance() }
        }
    }
    val alpha = remember(board) { FloatArray(cells * cells) { if (board.alive(it)) 1f else 0f } }
    var frame by remember { mutableIntStateOf(0) }
    val context = LocalContext.current
    val still = LocalInspectionMode.current ||
        Settings.Global.getFloat(context.contentResolver, Settings.Global.ANIMATOR_DURATION_SCALE, 1f) == 0f
    val lifecycle = LocalLifecycleOwner.current.lifecycle

    if (!still) {
        LaunchedEffect(board, lifecycle) {
            val stepNanos = (1_000_000_000 / generationsPerSecond).toLong()
            lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
                var lastStep = 0L
                var lastFrame = 0L
                // One callback for the whole loop, so a frame allocates nothing of ours.
                val onFrame: (Long) -> Unit = { now ->
                    if (lastStep == 0L) {
                        lastStep = now
                        lastFrame = now
                    }
                    if (now - lastStep >= stepNanos) {
                        board.advance()
                        lastStep = now
                    }
                    val fade = (now - lastFrame) / FADE_NANOS
                    lastFrame = now
                    for (i in alpha.indices) {
                        alpha[i] = if (board.alive(i)) 1f else (alpha[i] - fade).coerceAtLeast(0f)
                    }
                    frame++
                }
                while (true) withFrameNanos(onFrame)
            }
        }
    }

    Canvas(modifier.size(size).semantics { this.contentDescription = contentDescription }) {
        if (frame < 0) return@Canvas // reading the counter redraws on every frame
        val pitch = this.size.width / cells
        val gap = pitch * 0.22f
        val side = pitch - gap
        val radius = CornerRadius(side * 0.3f)
        val cell = Size(side, side)
        for (y in 0 until cells) {
            for (x in 0 until cells) {
                val a = if (still) (if (board.alive(y * cells + x)) 1f else 0f) else alpha[y * cells + x]
                drawRoundRect(color, Offset(x * pitch + gap / 2, y * pitch + gap / 2), cell, radius, alpha = GHOST + (1f - GHOST) * a)
            }
        }
    }
}
