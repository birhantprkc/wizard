package com.teddytennant.wizard.ssh

import com.teddytennant.wizard.acp.AcpClient
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.plus
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.net.ServerSocket
import java.net.Socket
import java.util.Base64

/**
 * Starts a throwaway, unprivileged sshd on 127.0.0.1 with its own host key
 * and authorized_keys, and runs the real connection path against it: TOFU,
 * key auth, the probe, and `wizard acp` over the exec channel. Skipped when
 * sshd or wizard isn't installed. No prompt is ever sent.
 */
class SshEndToEndTest {
    @get:Rule val tmp = TemporaryFolder()
    private var sshd: Process? = null
    private var port = 0
    private lateinit var hostKeyLine: String
    private lateinit var client: KeyMaterial

    private fun find(vararg paths: String) = paths.map(::File).firstOrNull(File::canExecute)

    @Before
    fun startSshd() {
        val sshdBin = find("/run/current-system/sw/bin/sshd", "/usr/sbin/sshd")
        val keygen = find("/run/current-system/sw/bin/ssh-keygen", "/usr/bin/ssh-keygen")
        val wizard = find(System.getProperty("user.home")!! + "/.local/bin/wizard")
        assumeTrue("needs sshd, ssh-keygen and wizard", sshdBin != null && keygen != null && wizard != null)
        val dir = tmp.newFolder("sshd")
        val hostKey = File(dir, "host_ed25519")
        assertEquals(0, ProcessBuilder(keygen!!.path, "-q", "-t", "ed25519", "-N", "", "-f", hostKey.path).start().waitFor())
        hostKeyLine = File(hostKey.path + ".pub").readText().trim()
        client = SshKeys.generateEd25519("test")
        val authorized = File(dir, "authorized_keys").apply { writeText(client.publicKey + "\n") }
        port = ServerSocket(0).use { it.localPort }
        val config = File(dir, "sshd_config").apply {
            writeText(
                """
                Port $port
                ListenAddress 127.0.0.1
                HostKey ${hostKey.path}
                PidFile ${dir.path}/sshd.pid
                AuthorizedKeysFile ${authorized.path}
                PubkeyAuthentication yes
                PasswordAuthentication no
                KbdInteractiveAuthentication no
                UsePAM no
                StrictModes no
                LogLevel ERROR
                """.trimIndent() + "\n",
            )
        }
        sshd = ProcessBuilder(sshdBin!!.path, "-D", "-e", "-f", config.path)
            .redirectErrorStream(true).redirectOutput(File(dir, "sshd.log")).start()
        val deadline = System.currentTimeMillis() + 10_000
        while (System.currentTimeMillis() < deadline) {
            if (runCatching { Socket("127.0.0.1", port).close() }.isSuccess) return
            Thread.sleep(100)
        }
        error("sshd didn't start: " + File(dir, "sshd.log").readText())
    }

    @After
    fun stopSshd() {
        sshd?.destroy()
    }

    private val target get() = SshTarget("127.0.0.1", port, System.getProperty("user.name")!!)

    @Test
    fun trustOnFirstUseThenDriveWizardAcp() = runBlocking {
        val first = runCatching { SshLink.connect(target, SshAuth.Key(client), KnownHosts()) }.exceptionOrNull()
        assertTrue("expected a host key prompt, got $first", first is HostKeyUnknownException)
        val presented = (first as HostKeyUnknownException).presented
        assertEquals(sshFingerprint(Base64.getDecoder().decode(hostKeyLine.split(" ")[1])), presented.fingerprint)

        val trusted = KnownHosts().trust(presented)
        SshLink.connect(target, SshAuth.Key(client), trusted).use { link ->
            val echo = link.exec(RemoteScripts.sh("printf '%s' \"\$1\"", "it's fine"))
            assertEquals(0, echo.exitStatus)
            assertEquals("it's fine", echo.stdout)

            val probe = RemoteScripts.parseProbe(link.exec(RemoteScripts.probe).stdout)
            assertTrue("wizard not found on PATH over ssh", probe.wizardVersion != null)

            val cwd = tmp.newFolder("project")
            val acp = AcpClient(link.openAcp(), this + Dispatchers.IO, onUpdate = {})
            try {
                withTimeout(60_000) {
                    assertEquals("Wizard", acp.initialize("test").name)
                    val session = acp.newSession(cwd.path)
                    assertTrue(session.configOptions.any { it.id == "model" && it.choices.isNotEmpty() })
                    assertTrue(acp.listSessions(cwd.path).sessions.isEmpty())
                }
            } finally {
                acp.close()
            }
        }
    }

    @Test
    fun aChangedHostKeyIsRefusedBeforeAuth() {
        val impostor = PresentedKey("127.0.0.1", port, "ssh-ed25519", Base64.getDecoder().decode("AAAAC3NzaC1lZDI1NTE5AAAAIKypliER+iYdXdMLiJVrTTNSHiET7UICjRe4fpxvo69D"))
        val error = runCatching { SshLink.connect(target, SshAuth.Key(client), KnownHosts().trust(impostor)) }.exceptionOrNull()
        assertTrue("got $error", error is HostKeyChangedException)
    }

    @Test
    fun anUnknownClientKeyIsAnAuthError() {
        val first = runCatching { SshLink.connect(target, SshAuth.Key(client), KnownHosts()) }.exceptionOrNull() as HostKeyUnknownException
        val stranger = SshKeys.generateEd25519("stranger")
        val error = runCatching { SshLink.connect(target, SshAuth.Key(stranger), KnownHosts().trust(first.presented)) }.exceptionOrNull()
        assertTrue("got $error", error is SshAuthException)
    }
}
