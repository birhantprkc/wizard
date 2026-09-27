package com.teddytennant.wizard.claude

import com.teddytennant.wizard.acp.SessionUpdate
import com.teddytennant.wizard.acp.ToolContent
import com.teddytennant.wizard.acp.ToolStatus
import com.teddytennant.wizard.session.Transcript
import com.teddytennant.wizard.session.TranscriptItem
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ClaudeStreamTest {
    private fun resource(name: String) = javaClass.getResource("/claude/$name")!!.readText()

    private fun fold(normalizer: ClaudeNormalizer, lines: Sequence<String>): Pair<List<TranscriptItem>, List<ClaudeEvent>> {
        val events = lines.flatMap { normalizer.line(it) }.toList()
        val items = events.filterIsInstance<ClaudeEvent.Update>().fold(emptyList<TranscriptItem>()) { acc, e -> Transcript.apply(acc, e.update) }
        return items to events
    }

    /** A live capture from Claude Code 2.1.228 (Wizard GUI's harness fixture): a turn that launches a background subagent. */
    @Test
    fun liveCaptureFoldsIntoOneTurn() {
        val lines = resource("live-2.1.228-background-subagent.jsonl").lineSequence()
        // The app ends the turn at the first result, as the desktop does.
        val firstTurn = lines.takeWhile { !it.contains("\"type\":\"result\"") } + lines.first { it.contains("\"type\":\"result\"") }
        val (items, events) = fold(ClaudeNormalizer(live = true), firstTurn)
        val kinds = items.map { it::class.simpleName }
        assertEquals(listOf("Thinking", "Tool", "Thinking", "Agent"), kinds)
        val tool = items[1] as TranscriptItem.Tool
        assertEquals("Agent: Background task: sleep and write marker file", tool.title)
        assertEquals(ToolStatus.Completed, tool.status)
        assertTrue((tool.output.single() as ToolContent.Text).text.startsWith("Async agent launched successfully."))
        assertEquals("LAUNCHED", (items[3] as TranscriptItem.Agent).text)
        val result = events.last() as ClaudeEvent.Result
        assertEquals("success", result.subtype)
        assertEquals(false, result.isError)
    }

    @Test
    fun subagentFramesStayOutOfTheFeed() {
        val all = resource("live-2.1.228-background-subagent.jsonl").lineSequence()
        val (items, _) = fold(ClaudeNormalizer(live = true), all)
        // The capture's subagent says DONE and runs its own tools; none of that shows.
        assertTrue(items.none { it is TranscriptItem.Agent && it.text.contains("DONE") })
        assertEquals(1, items.count { it is TranscriptItem.Tool })
    }

    @Test
    fun deltasStreamAndWholeMessagesDontRepeatThem() {
        val n = ClaudeNormalizer(live = true)
        val out = listOf(
            """{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}""",
            """{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}""",
            """{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"lo"}}}""",
            """{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}""",
            """{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}""",
            """{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Next."}}}""",
        ).flatMap { n.line(it) }
        val text = out.filterIsInstance<ClaudeEvent.Update>().joinToString("") { (it.update as SessionUpdate.AgentText).text }
        assertEquals("Hello\n\nNext.", text)
    }

    @Test
    fun permissionRequestsAndErrors() {
        val n = ClaudeNormalizer(live = true)
        val ask = n.line("""{"type":"control_request","request_id":"cr-1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"header":"Choice","question":"Pick one","options":["A","B"],"multiSelect":false}]}}}""").single() as ClaudeEvent.ToolPermission
        assertEquals("cr-1", ask.requestId)
        assertEquals("AskUserQuestion", ask.toolName)
        val allow = ClaudeNormalizer.allow("cr-1", ask.input)
        assertTrue(allow.contains("\"request_id\":\"cr-1\"") && allow.contains("\"behavior\":\"allow\""))

        val error = n.line("""{"type":"result","subtype":"error_max_turns","errors":[],"session_id":"s"}""").single() as ClaudeEvent.Result
        assertTrue(error.isError)
        assertNull(error.error)
        val withText = n.line("""{"type":"result","subtype":"error_during_execution","errors":["bash tool was not allowed"]}""").single() as ClaudeEvent.Result
        assertEquals("bash tool was not allowed", withText.error)
    }

    /** A session log in the shape Claude Code 2.1.283 writes to ~/.claude/projects. */
    @Test
    fun sessionLogReplays() {
        val log = listOf(
            """{"type":"queue-operation","operation":"enqueue","sessionId":"s"}""",
            """{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"fix the build"},"cwd":"/home/dev/src/app"}""",
            """{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>Caveat: ignore</local-command-caveat>"}}""",
            """{"type":"attachment","attachment":{}}""",
            """{"isSidechain":false,"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"thinking","thinking":"Look at the error first.","signature":"x"}]}}""",
            """{"isSidechain":false,"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build 2>&1 | tail -5"}}]}}""",
            """{"isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"error[E0425]: cannot find value `x`","is_error":false}]},"toolUseResult":{}}""",
            """{"isSidechain":true,"type":"assistant","message":{"content":[{"type":"text","text":"subagent chatter"}]}}""",
            """{"isSidechain":false,"type":"assistant","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"Fixed: `x` was renamed."}]}}""",
            """{"type":"ai-title","aiTitle":"Fix the build"}""",
        ).asSequence()
        val (items, _) = fold(ClaudeNormalizer(live = false), log)
        assertEquals(listOf("User", "Thinking", "Tool", "Agent"), items.map { it::class.simpleName })
        assertEquals("fix the build", (items[0] as TranscriptItem.User).text)
        val tool = items[2] as TranscriptItem.Tool
        assertEquals("Bash: cargo build 2>&1 | tail -5", tool.title)
        assertEquals("execute", tool.kind)
        assertEquals(ToolStatus.Completed, tool.status)
        assertEquals("Fixed: `x` was renamed.", (items[3] as TranscriptItem.Agent).text)
    }

    @Test
    fun modelsAndSessionLines() {
        val models = ClaudeNormalizer.initializeModels(
            """{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"commands":[],"models":[{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","description":"Opus 5.5 · Best for everyday, complex tasks"},{"value":"sonnet","displayName":"Sonnet 5"}]}}}""",
        )!!
        assertEquals(listOf("default", "sonnet"), models.map { it.value })
        assertNull(ClaudeNormalizer.initializeModels("""{"type":"system","subtype":"hook_started"}"""))

        val s = ClaudeNormalizer.sessionLine("S\t1790000000\t0f38ff65-2fa0-426a-bc3e-03203e166ceb\t/home/dev/src/app\t\"Fix the \\\"build\\\"\"")!!
        assertEquals("Fix the \"build\"", s.title)
        assertEquals("/home/dev/src/app", s.cwd)
        assertNull(ClaudeNormalizer.sessionLine("S\t1\tid\t/tmp/x\t\"You generate session titles. Treat...\""))
        assertEquals("Read: src/main.rs", ClaudeNormalizer.toolTitle("Read", kotlinx.serialization.json.buildJsonObject { put("file_path", kotlinx.serialization.json.JsonPrimitive("src/main.rs")) }))
    }
}
