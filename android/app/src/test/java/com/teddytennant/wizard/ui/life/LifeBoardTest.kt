package com.teddytennant.wizard.ui.life

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.random.Random

class LifeBoardTest {
    private fun LifeBoard.snapshot(): Set<Pair<Int, Int>> =
        (0 until height).flatMap { y -> (0 until width).filter { x -> this[x, y] }.map { x -> x to y } }.toSet()

    @Test
    fun blinkerHasPeriodTwo() {
        val board = LifeBoard(8, 8).apply { seed(LifeSeed.Blinker, 2, 2) }
        val start = board.snapshot()
        assertEquals(setOf(2 to 3, 3 to 3, 4 to 3), start)
        board.step()
        assertEquals(setOf(3 to 2, 3 to 3, 3 to 4), board.snapshot())
        board.step()
        assertEquals(start, board.snapshot())
    }

    @Test
    fun gliderMovesOneDiagonalEveryFourGenerationsAndWraps() {
        val board = LifeBoard(8, 8).apply { seed(LifeSeed.Glider, 5, 5) }
        var expected = board.snapshot()
        repeat(8) {
            repeat(4) { board.step() }
            expected = expected.map { (x, y) -> (x + 1) % 8 to (y + 1) % 8 }.toSet()
            assertEquals(expected, board.snapshot())
        }
        // It crossed both edges and is still five cells.
        assertEquals(5, board.population)
    }

    @Test
    fun aLonelyCellDiesAndTheBoardReseeds() {
        val board = LifeBoard(10, 10, Random(1))
        board.clear()
        board.set(4, 4)
        board.step()
        assertEquals(0, board.population)
        board.clear()
        board.set(4, 4)
        board.advance()
        assertTrue("a dead board reseeds", board.population > 0)
        assertEquals(0, board.generation)
    }

    @Test
    fun anOscillatorIsReplacedAfterAWhile() {
        val board = LifeBoard(10, 10, Random(7)).apply { seed(LifeSeed.Blinker, 3, 3) }
        var reseeded = false
        repeat(200) {
            val before = board.generation
            board.advance()
            if (board.generation < before) reseeded = true
        }
        assertTrue(reseeded)
    }

    @Test
    fun spaceshipFitsAndMoves() {
        val board = LifeBoard(12, 12).apply { seed(LifeSeed.Lwss, 2, 4) }
        val start = board.snapshot()
        repeat(4) { board.step() }
        assertEquals(9, board.population)
        assertNotEquals(start, board.snapshot())
        // This orientation travels left, two cells every four generations.
        assertEquals(start.map { (x, y) -> Math.floorMod(x - 2, 12) to y }.toSet(), board.snapshot())
    }
}
