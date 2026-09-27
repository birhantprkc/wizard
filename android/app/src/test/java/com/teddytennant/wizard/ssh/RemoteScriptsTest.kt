package com.teddytennant.wizard.ssh

import com.teddytennant.wizard.agent.Agent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class RemoteScriptsTest {
    @get:Rule val tmp = TemporaryFolder()

    /** What a remote login shell does with the command: hand it to a shell. */
    private fun run(command: String, home: File = tmp.root): Pair<Int, String> {
        val p = ProcessBuilder("/bin/sh", "-c", command).apply {
            environment()["HOME"] = home.path
        }.start()
        val out = p.inputStream.bufferedReader().readText()
        return p.waitFor() to out
    }

    @Test
    fun quotingSurvivesQuotesAndSpaces() {
        val weird = "it's a \"dir\" with \$HOME and `ticks`"
        val (code, out) = run(RemoteScripts.sh("printf '%s' \"\$1\"", weird))
        assertEquals(0, code)
        assertEquals(weird, out)
    }

    @Test
    fun listsDirectoriesAndMarksRepos() {
        val root = tmp.newFolder("projects")
        File(root, "wizard/.git").mkdirs()
        File(root, "notes").mkdirs()
        File(root, "Zed's stuff").mkdirs()
        File(root, "file.txt").writeText("x")
        File(root, ".hidden").mkdirs()
        val (code, out) = run(RemoteScripts.listDirs(root.path))
        assertEquals(0, code)
        val listing = RemoteScripts.parseListing(out)
        assertEquals(root.canonicalPath, File(listing.path).canonicalPath)
        assertEquals(listOf("wizard", "notes", "Zed's stuff"), listing.dirs.map { it.name })
        assertTrue(listing.dirs.first().isRepo)
    }

    @Test
    fun tildeMeansHome() {
        File(tmp.root, "code").mkdirs()
        val (_, out) = run(RemoteScripts.listDirs("~"))
        assertEquals(tmp.root.canonicalPath, File(RemoteScripts.parseListing(out).path).canonicalPath)
        val (_, sub) = run(RemoteScripts.listDirs("~/code"))
        assertEquals(File(tmp.root, "code").canonicalPath, File(RemoteScripts.parseListing(sub).path).canonicalPath)
        val (missing, _) = run(RemoteScripts.listDirs("~/nope"))
        assertEquals(3, missing)
    }

    @Test
    fun probeReportsEachAgent() {
        // HOME is a temp dir; the agents found depend on the build machine, the shape does not.
        val (code, out) = run(RemoteScripts.probe)
        assertEquals(0, code)
        val probe = RemoteScripts.parseProbe(out)
        assertEquals(tmp.root.path, probe.home)
        assertTrue(probe.running >= 0)
        assertEquals(setOf(Agent.Wizard, Agent.Pi, Agent.ClaudeCode), probe.agents.keys)
    }

    @Test
    fun probeParsing() {
        val empty = RemoteScripts.parseProbe("wizard=\npi=\npiacp=\nclaude=\nrunning=x\n")
        assertEquals(0, empty.running)
        assertTrue(empty.agents.values.none { it.installed || it.ready })

        val all = RemoteScripts.parseProbe(
            "wizard=wizard 3.5\npi=0.87.1\npiacp=/home/dev/.zeron/adapters/pi-acp/0.0.33/node_modules/.bin/pi-acp\n" +
                "claude=2.1.283 (Claude Code)\nnpm=/usr/bin/npm\nrunning=2\nhome=/home/dev",
        )
        assertEquals("wizard 3.5", all.wizardVersion)
        assertTrue(all.agents.values.all { it.ready })
        assertEquals(2, all.running)

        // pi without its adapter is installed but not ready, and says why.
        val piOnly = RemoteScripts.parseProbe("pi=0.87.1\npiacp=\n")
        val pi = piOnly.agents.getValue(Agent.Pi)
        assertTrue(pi.installed)
        assertFalse(pi.ready)
        assertEquals("Needs the pi-acp adapter", pi.missing)
    }

    @Test
    fun installsUseTheDesktopAppsLines() {
        assertTrue(RemoteScripts.install(Agent.Wizard).contains("curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_INSTALL_DIR="))
        assertTrue(RemoteScripts.install(Agent.ClaudeCode).contains("curl -fsSL https://claude.ai/install.sh | bash"))
        val pi = RemoteScripts.install(Agent.Pi)
        assertTrue(pi.contains("curl -fsSL https://pi.dev/install.sh | sh"))
        assertTrue(pi.contains("pi-acp@0.0.33"))
        assertTrue(pi.contains(".zeron/adapters/pi-acp/0.0.33"))
        assertTrue(pi.contains(".zeron-install-ok"))
    }

    @Test
    fun piInstallFindsAnAdapterAlreadyInTheManagedDir() {
        // Lay out a finished managed install the way Wizard GUI leaves it, and a fake pi on PATH.
        val bin = File(tmp.root, ".local/bin").apply { mkdirs() }
        File(bin, "pi").apply { writeText("#!/bin/sh\necho 0.87.1\n"); setExecutable(true) }
        val dir = File(tmp.root, ".zeron/adapters/pi-acp/0.0.33").apply { mkdirs() }
        File(dir, ".zeron-install-ok").writeText("0.0.33")
        File(dir, "node_modules/.bin").mkdirs()
        File(dir, "node_modules/.bin/pi-acp").apply { writeText("#!/bin/sh\n"); setExecutable(true) }
        val (code, out) = run(RemoteScripts.install(Agent.Pi))
        assertEquals(out, 0, code)
        assertTrue(out, out.contains("pi-acp is already installed"))
        val probe = RemoteScripts.parseProbe(run(RemoteScripts.probe).second)
        assertTrue(probe.agents.getValue(Agent.Pi).ready)
    }

    @Test
    fun claudeSessionListReadsTheProjectsDir() {
        val project = File(tmp.root, ".claude/projects/-home-dev-src-app").apply { mkdirs() }
        File(project, "0f38ff65-2fa0-426a-bc3e-03203e166ceb.jsonl").writeText(
            """{"type":"user","cwd":"/home/dev/src/app","message":{"role":"user","content":"make the \"build\" pass"},"sessionId":"0f38ff65-2fa0-426a-bc3e-03203e166ceb"}""" + "\n" +
                """{"type":"ai-title","aiTitle":"Fix the build","sessionId":"0f38ff65-2fa0-426a-bc3e-03203e166ceb"}""" + "\n",
        )
        File(project, "e286ecdc-fd99-480b-8f77-df36622a74c5.jsonl").writeText(
            """{"type":"user","cwd":"/home/dev/src/app","message":{"role":"user","content":"why is it \"slow\"?\nsecond line"}}""" + "\n",
        )
        val (code, out) = run(RemoteScripts.claudeSessions())
        assertEquals(0, code)
        val sessions = out.lines().mapNotNull { com.teddytennant.wizard.claude.ClaudeNormalizer.sessionLine(it) }.associateBy { it.sessionId }
        assertEquals("Fix the build", sessions.getValue("0f38ff65-2fa0-426a-bc3e-03203e166ceb").title)
        assertEquals("why is it \"slow\"?", sessions.getValue("e286ecdc-fd99-480b-8f77-df36622a74c5").title)
        assertEquals("/home/dev/src/app", sessions.getValue("e286ecdc-fd99-480b-8f77-df36622a74c5").cwd)

        val (_, log) = run(RemoteScripts.claudeTranscript("e286ecdc-fd99-480b-8f77-df36622a74c5"))
        assertTrue(log.contains("second line"))
        assertEquals(3, run(RemoteScripts.claudeTranscript("missing")).first)
    }
}
