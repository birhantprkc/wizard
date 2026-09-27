package com.teddytennant.wizard.claude

import com.teddytennant.wizard.acp.PermissionAnswer
import com.teddytennant.wizard.acp.PermissionRequest
import com.teddytennant.wizard.acp.SessionNotification
import com.teddytennant.wizard.acp.ToolStatus
import com.teddytennant.wizard.session.Transcript
import com.teddytennant.wizard.session.TranscriptItem
import com.teddytennant.wizard.testing.LocalExec
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.IOException

/**
 * Drives [ClaudeBackend] through the real launch script against Wizard GUI's
 * fake Claude CLI (tests/fixtures/fake-claude.sh in the harness crate), which
 * plays scripted stream-json shaped like live 2.1.228 captures.
 */
class ClaudeBackendTest {
    @get:Rule val tmp = TemporaryFolder()
    private val updates = mutableListOf<SessionNotification>()
    private val questions = mutableListOf<PermissionRequest>()

    @Before
    fun installFake() {
        val bin = File(tmp.root, ".local/bin").apply { mkdirs() }
        File(bin, "claude").apply {
            writeText(ClaudeBackendTest::class.java.getResource("/claude/fake-claude.sh")!!.readText())
            setExecutable(true)
        }
    }

    private fun backend() = ClaudeBackend(
        LocalExec(home = tmp.root),
        onUpdate = { synchronized(updates) { updates += it } },
        onPermission = { request ->
            questions += request
            PermissionAnswer(request.options.last().optionId)
        },
    )

    private fun items() = synchronized(updates) { updates.map { it.update } }
        .fold(emptyList<TranscriptItem>()) { acc, u -> Transcript.apply(acc, u) }

    @Test
    fun aTurnStreamsIntoTheTranscript() = runBlocking {
        val backend = backend()
        val session = backend.newSession(tmp.root.path)
        assertTrue(session.configOptions.any { it.id == "thought_level" })
        val stop = withTimeout(30_000) { backend.prompt(session.sessionId, tmp.root.path, "scenario:happy") }
        assertEquals("end_turn", stop)
        val items = items()
        assertEquals("pondering", (items[0] as TranscriptItem.Thinking).text)
        assertEquals("Hello", (items[1] as TranscriptItem.Agent).text)
        val tools = items.filterIsInstance<TranscriptItem.Tool>()
        assertEquals(listOf("Bash: ls -la", "mcp__linear__search"), tools.map { it.title })
        assertEquals(listOf(ToolStatus.Completed, ToolStatus.Failed), tools.map { it.status })
        assertTrue("subagent text leaked", items.none { it is TranscriptItem.Agent && it.text.contains("SUBAGENT") })
    }

    @Test
    fun toolsAreAllowedAndQuestionsGoToTheUser() = runBlocking {
        val backend = backend()
        val session = backend.newSession(tmp.root.path)
        // The fake fails the turn unless Bash was allowed and "Pick one" came back as "B".
        val stop = withTimeout(30_000) { backend.prompt(session.sessionId, tmp.root.path, "scenario:askuser") }
        assertEquals("end_turn", stop)
        assertEquals("Pick one", questions.single().title)
        assertEquals(listOf("A", "B"), questions.single().options.map { it.name })
    }

    @Test
    fun anErrorResultFailsTheTurn() = runBlocking {
        val backend = backend()
        val session = backend.newSession(tmp.root.path)
        val error = runCatching { withTimeout(30_000) { backend.prompt(session.sessionId, tmp.root.path, "scenario:error") } }.exceptionOrNull()
        assertTrue("got $error", error is IOException)
        assertTrue(error!!.message!!.contains("error_max_turns"))
    }

    @Test
    fun theFirstTurnNamesTheSessionAndLaterOnesResumeIt() = runBlocking {
        val backend = backend()
        val session = backend.newSession(tmp.root.path)
        backend.setOption(session.sessionId, "thought_level", "high")
        val state = ClaudeBackend.Session(tmp.root.path, started = false, effort = "high")
        val first = backend.flags(session.sessionId, state)
        assertTrue(first.containsAll(listOf("--session-id", session.sessionId, "--effort", "high", "--permission-prompt-tool", "stdio")))
        state.started = true
        assertTrue("--resume=${session.sessionId}" in backend.flags(session.sessionId, state))
    }
}
