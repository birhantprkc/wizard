package com.teddytennant.wizard.acp

import com.teddytennant.wizard.session.Transcript
import com.teddytennant.wizard.session.TranscriptItem
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Lines captured from `wizard acp` 3.5.0. */
class AcpParseTest {
    private fun result(line: String) = (RpcMessage.parse(line) as RpcMessage.Response).result!!
    private fun update(line: String) = AcpParse.notification((RpcMessage.parse(line) as RpcMessage.Notification).params)!!

    @Test
    fun initializeAdvertisesLoadSession() {
        val info = AcpParse.agentInfo(result("""{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},"mcpCapabilities":{"http":false,"sse":false},"sessionCapabilities":{"list":{}},"auth":{}},"authMethods":[],"agentInfo":{"name":"wizard","title":"Wizard","version":"3.5.0"}}}"""))
        assertEquals("Wizard", info.name)
        assertEquals("3.5.0", info.version)
        assertTrue(info.loadSession)
    }

    @Test
    fun sessionNewCarriesTheThreeConfigOptions() {
        val opened = AcpParse.openedSession(result("""{"jsonrpc":"2.0","id":1,"result":{"sessionId":"2026-09-27T02-30-26","configOptions":[{"id":"model","name":"Model","description":"Provider and model this session runs","category":"model","type":"select","currentValue":"xai-oauth/grok-4.6","options":[{"value":"xai-oauth/grok-4.7","name":"grok-4.7 (xai-oauth)","description":"xAI · xai-oauth"},{"value":"xai-oauth/grok-4.6","name":"grok-4.6 (xai-oauth)","description":"xAI · xai-oauth · configured"}]},{"id":"thought_level","name":"Reasoning","category":"thought_level","type":"select","currentValue":"default","options":[{"value":"default","name":"Default"},{"value":"high","name":"High"}]},{"id":"wizard_mode","name":"Mode","type":"select","currentValue":"genie","options":[{"value":"genie","name":"Genie"},{"value":"sovereign","name":"Sovereign"}]}]}}"""))
        assertEquals("2026-09-27T02-30-26", opened.sessionId)
        assertEquals(listOf("model", "thought_level", "wizard_mode"), opened.configOptions.map { it.id })
        val model = opened.configOptions.first()
        assertEquals("grok-4.6 (xai-oauth)", model.currentChoice!!.name)
        assertEquals(2, model.choices.size)
        assertNull(opened.configOptions.last().category)
    }

    @Test
    fun groupedChoicesAreFlattened() {
        val options = AcpParse.configOptions(RpcMessage.json.parseToJsonElement("""[{"id":"model","name":"Model","type":"select","currentValue":"b","options":[{"group":"x","name":"X","options":[{"value":"a","name":"A"},{"value":"b","name":"B"}]},{"value":"c","name":"C"}]}]"""))
        assertEquals(listOf("a", "b", "c"), options.single().choices.map { it.value })
    }

    @Test
    fun sessionListPages() {
        val page = AcpParse.sessionPage(result("""{"jsonrpc":"2.0","id":2,"result":{"sessions":[{"sessionId":"2026-08-31T15-39-52","cwd":"/home/nixos/all-my-repos/ai/wizard","title":"Report the line count","updatedAt":"2026-08-31T15:39:57.684761348+00:00"},{"cwd":"/no/id"}],"nextCursor":"abc"}}"""))
        assertEquals(1, page.sessions.size)
        assertEquals("/home/nixos/all-my-repos/ai/wizard", page.sessions[0].cwd)
        assertEquals("abc", page.nextCursor)
    }

    @Test
    fun replayedTranscriptFoldsIntoItems() {
        val lines = listOf(
            """{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Report the line count. Then stop."}},"_meta":{"isReplay":true}}}""",
            """{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"I'll count the lines now."}},"_meta":{"isReplay":true}}}""",
            """{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"history-call-0","title":"execute: wc -l src/agent/turn.rs","kind":"execute","status":"completed","rawInput":{"command":"wc -l src/agent/turn.rs"}},"_meta":{"isReplay":true}}}""",
            """{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"history-call-0","status":"completed","content":[{"type":"content","content":{"type":"text","text":"  2714 src/agent/turn.rs"}}]},"_meta":{"isReplay":true}}}""",
            """{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"- `src/agent/turn.rs`: 2714"}},"_meta":{"isReplay":true}}}""",
        )
        val notes = lines.map(::update)
        assertTrue(notes.all { it.isReplay && it.sessionId == "s" })
        val items = notes.fold(emptyList<TranscriptItem>()) { acc, n -> Transcript.apply(acc, n.update) }
        assertEquals(4, items.size)
        val tool = items[2] as TranscriptItem.Tool
        assertEquals("execute", tool.kind)
        assertEquals(ToolStatus.Completed, tool.status)
        assertTrue(tool.input!!.contains("\"command\": \"wc -l src/agent/turn.rs\""))
        assertEquals(ToolContent.Text("  2714 src/agent/turn.rs"), tool.output.single())
        assertEquals("- `src/agent/turn.rs`: 2714", (items[3] as TranscriptItem.Agent).text)
        assertEquals("src/agent/turn.rs: 2714", Transcript.lastReplyFirstLine(items))
    }

    @Test
    fun liveChunksAppendAndToolUpdatesLandInPlace() {
        var items = Transcript.userMessage(emptyList(), "fix the test")
        val stream = listOf(
            SessionUpdate.Thought("Looking"),
            SessionUpdate.Thought(" first."),
            SessionUpdate.AgentText("On "),
            SessionUpdate.AgentText("it."),
            SessionUpdate.ToolCallStarted("t1", "read_file: a.rs", "read", ToolStatus.Running, null, emptyList()),
            SessionUpdate.AgentText("Reading."),
            SessionUpdate.ToolCallUpdated("t1", null, ToolStatus.Completed, listOf(ToolContent.Text("fn main() {}"))),
            SessionUpdate.ToolCallUpdated("missing", null, ToolStatus.Failed, null),
        )
        stream.forEach { items = Transcript.apply(items, it) }
        assertEquals(listOf("u0", "t1", "a2", "c3", "a4"), items.map { it.key })
        assertEquals("Looking first.", (items[1] as TranscriptItem.Thinking).text)
        assertEquals("On it.", (items[2] as TranscriptItem.Agent).text)
        val tool = items[3] as TranscriptItem.Tool
        assertEquals(ToolStatus.Completed, tool.status)
        assertEquals("read_file: a.rs", tool.title)
    }

    @Test
    fun settleFailsToolsLeftRunning() {
        val items = Transcript.apply(emptyList(), SessionUpdate.ToolCallStarted("t", "execute: sleep 99", "execute", ToolStatus.Running, null, emptyList()))
        assertEquals(ToolStatus.Failed, (Transcript.settle(items).single() as TranscriptItem.Tool).status)
    }

    @Test
    fun permissionRequestsParse() {
        val params = RpcMessage.json.parseToJsonElement("""{"sessionId":"s","toolCall":{"toolCallId":"c","title":"execute: rm -rf build","rawInput":{"command":"rm -rf build"}},"options":[{"optionId":"allow","name":"Allow once","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]}""")
        val request = AcpParse.permissionRequest(params)!!
        assertEquals("execute: rm -rf build", request.title)
        assertEquals(listOf("allow", "reject"), request.options.map { it.optionId })
    }

    @Test
    fun diffContentAndUnknownUpdates() {
        val u = AcpParse.update(RpcMessage.json.parseToJsonElement("""{"sessionUpdate":"tool_call","toolCallId":"e","title":"edit_file: a.rs","kind":"edit","status":"in_progress","content":[{"type":"diff","path":"a.rs","oldText":"x","newText":"y"}]}""") as kotlinx.serialization.json.JsonObject)
        val started = u as SessionUpdate.ToolCallStarted
        assertEquals(ToolStatus.Running, started.status)
        assertEquals(ToolContent.Diff("a.rs", "x", "y"), started.content.single())
        val other = AcpParse.update(RpcMessage.json.parseToJsonElement("""{"sessionUpdate":"usage_update"}""") as kotlinx.serialization.json.JsonObject)
        assertEquals(SessionUpdate.Other("usage_update"), other)
        assertFalse(Transcript.apply(emptyList(), other).isNotEmpty())
    }
}
