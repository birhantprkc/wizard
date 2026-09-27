package com.teddytennant.wizard.acp

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.plus
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

/**
 * Drives a real `wizard acp` over a local process's stdio. Skipped when no
 * wizard binary is found. It never sends a prompt, so no model is called:
 * a session that is never prompted is removed when the server exits.
 */
class WizardAcpIntegrationTest {
    private fun wizardBinary(): File? {
        System.getenv("WIZARD_BIN")?.let { return File(it).takeIf(File::canExecute) }
        val home = System.getProperty("user.home")
        return listOf("$home/.local/bin/wizard", "$home/.cargo/bin/wizard", "/usr/local/bin/wizard")
            .map(::File).firstOrNull(File::canExecute)
    }

    private class ProcessTransport(private val process: Process) : AcpTransport {
        override val input get() = process.inputStream
        override val output get() = process.outputStream
        override fun close() {
            process.outputStream.close()
            if (!process.waitFor(10, java.util.concurrent.TimeUnit.SECONDS)) process.destroy()
        }
    }

    @Test
    fun newSessionConfigOptionsAndList() = runBlocking {
        val wizard = wizardBinary()
        assumeTrue("no wizard binary", wizard != null)
        val cwd = Files.createTempDirectory("wizard-acp-test").toFile()
        val process = ProcessBuilder(wizard!!.path, "acp")
            .directory(cwd)
            .redirectError(File(cwd, ".stderr"))
            .start()
        val client = AcpClient(ProcessTransport(process), this + Dispatchers.IO, onUpdate = {})
        try {
            withTimeout(60_000) {
                val info = client.initialize("test")
                assertEquals("Wizard", info.name)
                assertTrue(info.loadSession)

                val session = client.newSession(cwd.path)
                val ids = session.configOptions.map { it.id }
                assertTrue(ids.containsAll(listOf("model", "thought_level", "wizard_mode")))
                val effort = session.configOptions.first { it.id == "thought_level" }
                assertTrue(effort.choices.any { it.value == "high" })

                val changed = client.setConfigOption(session.sessionId, "thought_level", "high")
                assertEquals("high", changed.first { it.id == "thought_level" }.currentValue)

                // Nothing was ever said in this directory, so the list is empty.
                val page = client.listSessions(cwd.path)
                assertTrue(page.sessions.none { it.sessionId == session.sessionId })
            }
        } finally {
            client.close()
            cwd.deleteRecursively()
        }
    }
}
