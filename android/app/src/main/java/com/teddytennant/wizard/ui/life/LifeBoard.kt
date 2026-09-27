package com.teddytennant.wizard.ui.life

import kotlin.random.Random

/** Small patterns that stay interesting on a tiny torus. Coordinates are (x, y). */
enum class LifeSeed(val cells: IntArray, val minSize: Int) {
    Glider(intArrayOf(1, 0, 2, 1, 0, 2, 1, 2, 2, 2), 5),
    Lwss(intArrayOf(1, 0, 4, 0, 0, 1, 0, 2, 4, 2, 0, 3, 1, 3, 2, 3, 3, 3), 8),
    Blinker(intArrayOf(0, 1, 1, 1, 2, 1), 4),
    Toad(intArrayOf(1, 0, 2, 0, 3, 0, 0, 1, 1, 1, 2, 1), 6),
    Beacon(intArrayOf(0, 0, 1, 0, 0, 1, 3, 2, 2, 3, 3, 3), 6),
}

/**
 * Conway's Game of Life on a torus, sized for a loading indicator. Two
 * preallocated buffers, so stepping never allocates. [advance] also reseeds
 * when the board dies or has been cycling for a while.
 */
class LifeBoard(val width: Int, val height: Int, private val random: Random = Random.Default) {
    private var cells = BooleanArray(width * height)
    private var next = BooleanArray(width * height)
    private val history = LongArray(HISTORY)
    private var historySize = 0
    private var historyHead = 0

    /** Generations since the last seed. */
    var generation = 0
        private set

    operator fun get(x: Int, y: Int): Boolean = cells[index(x, y)]
    fun alive(i: Int): Boolean = cells[i]
    val population: Int
        get() {
            var n = 0
            for (c in cells) if (c) n++
            return n
        }

    private fun index(x: Int, y: Int) = Math.floorMod(y, height) * width + Math.floorMod(x, width)

    fun clear() {
        cells.fill(false)
        generation = 0
        historySize = 0
    }

    fun set(x: Int, y: Int, alive: Boolean = true) {
        cells[index(x, y)] = alive
    }

    fun seed(pattern: LifeSeed, x: Int, y: Int) {
        clear()
        val p = pattern.cells
        var i = 0
        while (i < p.size) {
            set(x + p[i], y + p[i + 1])
            i += 2
        }
    }

    private val fitting = LifeSeed.entries.filter { it.minSize <= minOf(width, height) }.ifEmpty { listOf(LifeSeed.Blinker) }

    /** A pattern that fits, at a random spot. */
    fun reseed() {
        seed(fitting[random.nextInt(fitting.size)], random.nextInt(width), random.nextInt(height))
    }

    /** One generation under B3/S23, wrapping at the edges. */
    fun step() {
        for (y in 0 until height) {
            val up = (y + height - 1) % height * width
            val row = y * width
            val down = (y + 1) % height * width
            for (x in 0 until width) {
                val left = (x + width - 1) % width
                val right = (x + 1) % width
                var n = 0
                if (cells[up + left]) n++
                if (cells[up + x]) n++
                if (cells[up + right]) n++
                if (cells[row + left]) n++
                if (cells[row + right]) n++
                if (cells[down + left]) n++
                if (cells[down + x]) n++
                if (cells[down + right]) n++
                next[row + x] = n == 3 || (n == 2 && cells[row + x])
            }
        }
        val t = cells
        cells = next
        next = t
        generation++
    }

    /** [step], then reseed if everything died or the board has repeated itself for long enough to be boring. */
    fun advance() {
        step()
        val dead = isEmpty()
        val hash = hash()
        val repeated = !dead && seen(hash)
        remember(hash)
        if (dead || (repeated && generation >= MIN_RUN)) reseed()
    }

    private fun isEmpty(): Boolean {
        for (c in cells) if (c) return false
        return true
    }

    private fun seen(hash: Long): Boolean {
        for (i in 0 until historySize) if (history[i] == hash) return true
        return false
    }

    private fun remember(hash: Long) {
        history[historyHead] = hash
        historyHead = (historyHead + 1) % HISTORY
        if (historySize < HISTORY) historySize++
    }

    private fun hash(): Long {
        var h = FNV_OFFSET
        for (i in cells.indices) {
            h = (h xor (if (cells[i]) i.toLong() + 1 else 0L)) * FNV_PRIME
        }
        return h
    }

    private companion object {
        const val HISTORY = 48
        /** An oscillator or a lapping glider gets this many generations before a new pattern. */
        const val MIN_RUN = 40
        const val FNV_OFFSET = -3750763034362895579L
        const val FNV_PRIME = 1099511628211L
    }
}
