package com.teddytennant.wizard.session

import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.claude.ClaudeBackend
import com.teddytennant.wizard.ssh.RemoteScripts
import com.teddytennant.wizard.testing.LocalExec
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.plus
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/**
 * Each agent through the app's own launch scripts, run in a local shell with
 * the real HOME so the installed agents are found the way SSH finds them.
 * Only calls that reach no model: initialize, session/new, config options,
 * session/list, Claude's initialize control request and its session logs.
 * Skipped for any agent the probe doesn't find.
 */
class AgentIntegrationTest {
    @get:Rule val tmp = TemporaryFolder()
    private val exec = LocalExec()
    private val probe by lazy { RemoteScripts.parseProbe(exec.run(RemoteScripts.probe, 60).stdout) }

    private fun acpRoundTrip(agent: Agent, expectedOptions: List<String>) = runBlocking {
        assumeTrue("${agent.displayName} isn't installed here", probe.agents.getValue(agent).ready)
        val backend = AcpBackend.start(agent, exec, this + Dispatchers.IO, "test", onUpdate = {}, onPermission = { com.teddytennant.wizard.acp.PermissionAnswer(null) })
        try {
            withTimeout(90_000) {
                val session = backend.newSession(tmp.root.path)
                val ids = session.configOptions.map { it.id }
                assertTrue("$ids", ids.containsAll(expectedOptions))
                val effort = session.configOptions.first { it.id == "thought_level" }
                val high = effort.choices.first { it.value == "high" }.value
                val changed = backend.setOption(session.sessionId, "thought_level", high)
                assertEquals(high, changed.first { it.id == "thought_level" }.currentValue)
                // Nothing was ever said in this directory.
                assertTrue(backend.listSessions(tmp.root.path).none { it.sessionId == session.sessionId })
            }
        } finally {
            backend.close()
        }
    }

    @Test
    fun wizardOverItsLaunchScript() = acpRoundTrip(Agent.Wizard, listOf("model", "thought_level", "wizard_mode"))

    @Test
    fun piOverPiAcp() {
        try {
            acpRoundTrip(Agent.Pi, listOf("model", "thought_level"))
        } finally {
            // pi makes an (empty) folder for every cwd it opens a session in.
            val slug = "--" + tmp.root.path.trim('/').replace('/', '-') + "--"
            File(System.getProperty("user.home"), ".pi/agent/sessions/$slug").takeIf { it.isDirectory && it.list().isNullOrEmpty() }?.delete()
        }
    }

    @Test
    fun claudeModelsAndSavedSessions(): Unit = runBlocking {
        assumeTrue("Claude Code isn't installed here", probe.agents.getValue(Agent.ClaudeCode).ready)
        val backend = ClaudeBackend(exec, onUpdate = {}, onPermission = { com.teddytennant.wizard.acp.PermissionAnswer(null) })
        withTimeout(90_000) {
            val models = backend.models()
            assertTrue("no models from initialize", models.isNotEmpty())
            val sessions = backend.listSessions(null)
            val logs = File(System.getProperty("user.home"), ".claude/projects").listFiles().orEmpty().any { d -> d.listFiles().orEmpty().any { it.name.endsWith(".jsonl") } }
            if (logs) assertTrue("saved sessions exist but none were listed", sessions.isNotEmpty())
            sessions.forEach { assertTrue(it.sessionId, it.sessionId.matches(Regex("[0-9a-f-]{36}"))) }
            // Reopening one reads its log without running anything.
            sessions.firstOrNull()?.let { backend.loadSession(it.sessionId, it.cwd) }
        }
    }
}
