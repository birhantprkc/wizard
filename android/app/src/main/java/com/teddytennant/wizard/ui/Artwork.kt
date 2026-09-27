package com.teddytennant.wizard.ui

import android.content.Context
import android.graphics.BitmapFactory
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.painter.BitmapPainter
import androidx.compose.ui.graphics.painter.Painter
import com.teddytennant.wizard.R

/**
 * The Starship backdrop, decoded once per process. `painterResource` decodes
 * again every time the home screen re-enters composition, which landed in
 * the middle of the back transition.
 */
object Artwork {
    @Volatile private var bitmap: ImageBitmap? = null
    @Volatile private var painter: Painter? = null

    /** Decodes off the main thread at startup, so the first frame of home doesn't wait on it. */
    fun prewarm(context: Context) {
        Thread({ load(context) }, "artwork").apply { isDaemon = true }.start()
    }

    private fun load(context: Context): ImageBitmap = bitmap ?: synchronized(this) {
        bitmap ?: BitmapFactory.decodeResource(context.resources, R.drawable.wizard_starship).asImageBitmap().also { bitmap = it }
    }

    fun starship(context: Context): Painter = painter ?: BitmapPainter(load(context)).also { painter = it }
}
