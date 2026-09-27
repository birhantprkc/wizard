package com.teddytennant.wizard.claude

import com.teddytennant.wizard.acp.ConnectionClosedException
import com.teddytennant.wizard.acp.PermissionAnswer
import com.teddytennant.wizard.acp.SessionNotification
import com.teddytennant.wizard.session.Activity
import com.teddytennant.wizard.session.Transcript
import com.teddytennant.wizard.session.TranscriptItem
import com.teddytennant.wizard.testing.FakeClaude
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * The process outlives the turn. Plays the live 2.1.228 capture of a turn
 * that launches a background subagent, split where the app sees it: the turn
 * up to its `result`, the subagent working (and asking for Bash) after it,
 * then the wake turn the CLI runs when the subagent finishes.
 */
class ClaudeLifetimeTest {
    private val capture = javaClass.getResource("/claude/live-2.1.228-background-subagent.jsonl")!!.readText().lines().filter { it.isNotBlank() }
    private val firstTurn = capture.subList(0, 60)
    private val meanwhile = capture.subList(60, 76)
    private val wake = capture.subList(76, capture.size)
    private val subagentBash = "66e23d57-26a6-4150-9002-f94991a36290"

    private val fake = FakeClaude()
    private val updates = CopyOnWriteArrayList<SessionNotification>()
    private val activity = LinkedBlockingQueue<Activity>()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    @After
    fun stop() = scope.cancel()

    private fun backend(settleMs: Long = 200, idleCloseMs: Long = 60_000) = ClaudeBackend(
        fake,
        onUpdate = { updates += it },
        onPermission = { PermissionAnswer(it.options.last().optionId) },
        onActivity = { activity += it },
        scope = scope,
        timing = ClaudeBackend.Timing(settleMs = settleMs, idleCloseMs = idleCloseMs, interruptGraceMs = 500),
    )

    private fun nextActivity(): Activity = activity.poll(10, TimeUnit.SECONDS) ?: error("no activity")

    private fun replies() = updates.map { it.update }.fold(emptyList<TranscriptItem>()) { acc, u -> Transcript.apply(acc, u) }
        .filterIsInstance<TranscriptItem.Agent>().map { it.text }

    private fun waitFor(what: String, check: () -> Boolean) {
        val until = System.currentTimeMillis() + 10_000
        while (!check()) {
            if (System.currentTimeMillis() > until) error("timed out waiting for $what")
            Thread.sleep(20)
        }
    }

    /** Starts a session and plays the capture's first turn; returns the backend, the session id and the process. */
    private fun launched(backend: ClaudeBackend = backend()) = runBlocking {
        val id = backend.newSession("/work").sessionId
        val stop = async(Dispatchers.IO) { backend.prompt(id, "/work", "launch a background agent") }
        val process = fake.next()
        process.awaitWrite { it.contains("launch a background agent") }
        process.emit(firstTurn)
        assertEquals("end_turn", withTimeout(10_000) { stop.await() })
        assertEquals(Activity.Tasks(id, 1), nextActivity())
        Triple(backend, id, process)
    }

    @Test
    fun theTurnReturnsAtItsResultAndTheProcessStaysOpen() {
        val (backend, id, process) = launched()
        assertTrue(process.command.contains("'--permission-mode' 'bypassPermissions' '--dangerously-skip-permissions'"))
        assertTrue(process.command.contains("'--permission-prompt-tool' 'stdio'"))
        assertFalse(process.closed)
        assertEquals(1, backend.backgroundTasks(id))
        assertEquals(listOf("LAUNCHED"), replies())
    }

    @Test
    fun permissionRequestsAfterTheResultAreAnswered() {
        val (_, _, process) = launched()
        process.emit(meanwhile)
        val answer = process.awaitWrite { it.contains(subagentBash) }
        assertTrue(answer, answer.contains("\"behavior\":\"allow\""))
    }

    @Test
    fun theWakeTurnIsForwardedAndIdleComesOnce() {
        val (backend, id, process) = launched()
        process.emit(meanwhile)
        assertEquals(Activity.Tasks(id, 0), nextActivity())
        process.emit(wake)
        assertEquals(Activity.WakeStarted(id), nextActivity())
        assertEquals(Activity.WakeEnded(id, null), nextActivity())
        assertEquals(Activity.Idle(id), nextActivity())
        assertNull(activity.poll(600, TimeUnit.MILLISECONDS))
        assertTrue(replies().last().startsWith("The background subagent has completed successfully."))
        assertFalse(process.closed)
        assertEquals(0, backend.backgroundTasks(id))
    }

    @Test
    fun tasksFinishingTogetherWakeTwiceButGoIdleOnce() {
        val backend = backend()
        val id = runBlocking { backend.newSession("/work").sessionId }
        val stop = scope.async { backend.prompt(id, "/work", "two agents") }
        val process = fake.next()
        process.awaitWrite { it.contains("two agents") }
        process.emit(
            """{"type":"system","subtype":"init","session_id":"s"}""",
            """{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a"},{"task_id":"b"}]}""",
            """{"type":"result","subtype":"success","is_error":false,"session_id":"s"}""",
        )
        assertEquals("end_turn", runBlocking { withTimeout(10_000) { stop.await() } })
        assertEquals(Activity.Tasks(id, 2), nextActivity())
        process.emit(
            """{"type":"system","subtype":"background_tasks_changed","tasks":[]}""",
            """{"type":"system","subtype":"init","session_id":"s"}""",
            """{"type":"result","subtype":"success","is_error":false,"session_id":"s"}""",
            """{"type":"system","subtype":"init","session_id":"s"}""",
            """{"type":"result","subtype":"success","is_error":false,"session_id":"s"}""",
        )
        val seen = generateSequence { activity.poll(1, TimeUnit.SECONDS) }.toList()
        assertEquals(
            listOf(Activity.Tasks(id, 0), Activity.WakeStarted(id), Activity.WakeEnded(id, null), Activity.WakeStarted(id), Activity.WakeEnded(id, null), Activity.Idle(id)),
            seen,
        )
    }

    @Test
    fun aQuietSessionClosesAndTheNextPromptResumes() {
        val (backend, id, process) = launched(backend(idleCloseMs = 300))
        process.emit(meanwhile + wake)
        waitFor("the idle close") { process.closed }
        val stop = scope.async { backend.prompt(id, "/work", "next") }
        val again = fake.next()
        assertTrue(again.command, again.command.contains("'--resume=$id'"))
        again.awaitWrite { it.contains("next") }
        again.emit("""{"type":"result","subtype":"success","is_error":false,"session_id":"$id"}""")
        assertEquals("end_turn", runBlocking { withTimeout(10_000) { stop.await() } })
    }

    @Test
    fun cancelInterruptsTheTurnAndCloses() {
        val backend = backend()
        val id = runBlocking { backend.newSession("/work").sessionId }
        val stop = scope.async { backend.prompt(id, "/work", "long job") }
        val process = fake.next()
        process.awaitWrite { it.contains("long job") }
        process.emit(firstTurn.take(30))
        runBlocking { backend.cancel(id) }
        process.awaitWrite { it.contains("\"subtype\":\"interrupt\"") }
        process.emit("""{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"$id"}""")
        assertEquals("cancelled", runBlocking { withTimeout(10_000) { stop.await() } })
        waitFor("the process to close") { process.closed }
    }

    @Test
    fun cancelWithOnlyBackgroundTasksLeftCloses() {
        val (backend, id, process) = launched()
        runBlocking { backend.cancel(id) }
        assertTrue(process.closed)
        assertEquals(Activity.Stopped(id, lost = false, detail = null), nextActivity())
    }

    @Test
    fun aDroppedLinkIsReportedAndTheNextPromptResumes() {
        val (backend, id, process) = launched()
        fake.isConnected = false
        process.drop()
        assertEquals(Activity.Stopped(id, lost = true, detail = null), nextActivity())
        fake.isConnected = true
        val stop = scope.async { backend.prompt(id, "/work", "still there?") }
        val again = fake.next()
        assertTrue(again.command.contains("'--resume=$id'"))
        again.awaitWrite { it.contains("still there?") }
        again.emit("""{"type":"result","subtype":"success","is_error":false,"session_id":"$id"}""")
        assertEquals("end_turn", runBlocking { withTimeout(10_000) { stop.await() } })
    }

    @Test
    fun aDroppedLinkMidTurnFailsThePromptAsLost() {
        val backend = backend()
        val id = runBlocking { backend.newSession("/work").sessionId }
        val stop = scope.async { backend.prompt(id, "/work", "hello") }
        val process = fake.next()
        process.awaitWrite { it.contains("hello") }
        process.emit(firstTurn.take(10))
        fake.isConnected = false
        process.drop()
        val error = runBlocking { runCatching { withTimeout(10_000) { stop.await() } }.exceptionOrNull() }
        assertTrue("got $error", error is ConnectionClosedException)
    }
}
