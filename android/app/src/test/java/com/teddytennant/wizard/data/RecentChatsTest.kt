package com.teddytennant.wizard.data

import com.teddytennant.wizard.agent.Agent
import org.junit.Assert.assertEquals
import org.junit.Test

class RecentChatsTest {
    private fun chat(machine: String, agent: Agent, id: String, at: Long, title: String? = null) =
        RecentChat(machine, agent.id, id, "/home/dev/src/app", title, at)

    @Test
    fun mergesAcrossMachinesAndAgentsNewestFirst() {
        val existing = listOf(chat("m1", Agent.Wizard, "a", 100, "Fix the build"), chat("m2", Agent.Pi, "b", 50))
        val merged = RecentChats.merge(
            existing,
            listOf(chat("m1", Agent.Wizard, "a", 90, null), chat("m1", Agent.ClaudeCode, "a", 300, "Same id, other agent")),
        )
        assertEquals(listOf("claude", "wizard", "pi"), merged.map { it.agentId })
        // The older listing doesn't move the chat back or drop its title.
        assertEquals(100, merged[1].updatedAt)
        assertEquals("Fix the build", merged[1].title)
    }

    @Test
    fun capsAndForgets() {
        val many = (1..300).map { chat("m1", Agent.Wizard, "s$it", it.toLong()) }
        val merged = RecentChats.merge(emptyList(), many)
        assertEquals(RecentChats.LIMIT, merged.size)
        assertEquals("s300", merged.first().sessionId)
        assertEquals(0, RecentChats.forgetMachine(merged, "m1").size)
        assertEquals(Agent.Wizard, Agent.fromId("nope"))
    }
}
