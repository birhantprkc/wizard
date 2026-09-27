package com.teddytennant.wizard.ssh

import org.junit.Assert.assertEquals
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
    fun probeReportsWhetherWizardIsThere() {
        val (code, out) = run(RemoteScripts.probe)
        assertEquals(0, code)
        val probe = RemoteScripts.parseProbe(out)
        assertEquals(tmp.root.path, probe.home)
        assertTrue(probe.running >= 0)
        assertEquals(RemoteScripts.Probe(null, 0, null), RemoteScripts.parseProbe("wizard=\nrunning=x\n"))
        assertEquals("wizard 3.5", RemoteScripts.parseProbe("wizard=wizard 3.5\nrunning=2\nhome=/h").wizardVersion)
    }

    @Test
    fun installUsesTheDesktopAppsLine() {
        assertTrue(RemoteScripts.install.contains("curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_INSTALL_DIR="))
    }
}
