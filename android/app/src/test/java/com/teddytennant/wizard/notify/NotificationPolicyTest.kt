package com.teddytennant.wizard.notify

import com.teddytennant.wizard.data.Settings
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class NotificationPolicyTest {
    private fun situation(foreground: Boolean = false, viewing: Boolean = false, settings: Settings = Settings(), granted: Boolean = true) =
        NotificationPolicy.Situation(foreground, viewing, settings, granted)

    @Test
    fun stopReasonsMapToOutcomes() {
        assertEquals(TurnOutcome.Finished, NotificationPolicy.outcome("end_turn", failed = false))
        assertEquals(TurnOutcome.Cancelled, NotificationPolicy.outcome("cancelled", failed = false))
        assertEquals(TurnOutcome.TurnLimit, NotificationPolicy.outcome("max_turn_requests", failed = false))
        assertEquals(TurnOutcome.Finished, NotificationPolicy.outcome(null, failed = false))
        assertEquals(TurnOutcome.Failed, NotificationPolicy.outcome("end_turn", failed = true))
    }

    @Test
    fun aFinishedTurnNotifiesOnlyWhenTheAppIsAway() {
        assertTrue(NotificationPolicy.notifyTurnEnd(TurnOutcome.Finished, false, situation()))
        assertFalse(NotificationPolicy.notifyTurnEnd(TurnOutcome.Finished, false, situation(foreground = true)))
        assertFalse(NotificationPolicy.notifyTurnEnd(TurnOutcome.Finished, false, situation(granted = false)))
        assertFalse(NotificationPolicy.notifyTurnEnd(TurnOutcome.Finished, false, situation(settings = Settings(notifyFinished = false))))
        // A failure is still news, a stop the user pressed is not.
        assertTrue(NotificationPolicy.notifyTurnEnd(TurnOutcome.Failed, false, situation()))
        assertFalse(NotificationPolicy.notifyTurnEnd(TurnOutcome.Cancelled, true, situation()))
        assertTrue(NotificationPolicy.notifyTurnEnd(TurnOutcome.Cancelled, false, situation()))
    }

    @Test
    fun aQuestionNotifiesUnlessThatChatIsOnScreen() {
        assertTrue(NotificationPolicy.notifyNeedsInput(situation()))
        assertTrue(NotificationPolicy.notifyNeedsInput(situation(foreground = true, viewing = false)))
        assertFalse(NotificationPolicy.notifyNeedsInput(situation(foreground = true, viewing = true)))
        assertFalse(NotificationPolicy.notifyNeedsInput(situation(settings = Settings(notifyInput = false))))
        assertFalse(NotificationPolicy.notifyNeedsInput(situation(granted = false)))
    }

    @Test
    fun permissionIsAskedOnceOnAndroid13AndUp() {
        assertTrue(NotificationPolicy.shouldAskPermission(33, granted = false, askedBefore = false))
        assertFalse(NotificationPolicy.shouldAskPermission(33, granted = false, askedBefore = true))
        assertFalse(NotificationPolicy.shouldAskPermission(35, granted = true, askedBefore = false))
        assertFalse(NotificationPolicy.shouldAskPermission(32, granted = false, askedBefore = false))
    }

    @Test
    fun wording() {
        val done = NotificationPolicy.turnEnded("devbox", TurnOutcome.Finished, "Fixed the flaky test.", null)
        assertEquals(NotificationText("Wizard finished on devbox", "Fixed the flaky test."), done)
        assertEquals("The turn is done.", NotificationPolicy.turnEnded("devbox", TurnOutcome.Finished, null, null).body)
        val long = NotificationPolicy.turnEnded("devbox", TurnOutcome.Finished, "x".repeat(400), null).body
        assertEquals(160, long.length)
        assertTrue(long.endsWith("…"))
        assertEquals("Wizard lost devbox", NotificationPolicy.turnEnded("devbox", TurnOutcome.Failed, null, "The connection to the machine dropped.").title)
        assertEquals("Wizard needs you on devbox", NotificationPolicy.needsInput("devbox", "execute: rm -rf target").title)
    }

    @Test
    fun ongoingSummarisesOneOrManyTurns() {
        val a = RunningTurn("m1", "devbox", "s1", "Fix the build", "/home/t/wizard")
        val b = RunningTurn("m1", "devbox", "s2", null, "/home/t/reverie")
        val c = RunningTurn("m2", "pi", "s3", null, "/home/pi/code")
        assertEquals(NotificationText("Working on devbox", "Fix the build"), NotificationPolicy.ongoing(listOf(a)))
        assertEquals(NotificationText("Working on devbox", "reverie"), NotificationPolicy.ongoing(listOf(b)))
        assertEquals(NotificationText("Working in 2 sessions", "On devbox"), NotificationPolicy.ongoing(listOf(a, b)))
        assertEquals("devbox, pi", NotificationPolicy.ongoing(listOf(a, b, c)).body)
    }
}
