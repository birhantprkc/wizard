package com.teddytennant.wizard.ssh

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.util.Base64

class HostKeysTest {
    @get:Rule val tmp = TemporaryFolder()

    // From `ssh-keygen -t ed25519`; `ssh-keygen -lf` prints SHA256:WKYFZFSoY9ATfSbq7u+Xfym3JKInQA8knEnuVJh2PLY.
    private val blob = Base64.getDecoder().decode("AAAAC3NzaC1lZDI1NTE5AAAAIIsfxMidb6GmIlWavPk0xG+uL39oOwR5Kca60Y5PWBR7")
    private val other = Base64.getDecoder().decode("AAAAC3NzaC1lZDI1NTE5AAAAIKypliER+iYdXdMLiJVrTTNSHiET7UICjRe4fpxvo69D")

    @Test
    fun fingerprintMatchesSshKeygen() {
        assertEquals("SHA256:WKYFZFSoY9ATfSbq7u+Xfym3JKInQA8knEnuVJh2PLY", sshFingerprint(blob))
    }

    @Test
    fun firstContactIsUnknownThenTrusted() {
        val presented = PresentedKey("Box.local", 22, "ssh-ed25519", blob)
        val empty = KnownHosts()
        assertEquals(HostKeyCheck.Unknown(presented), empty.check(presented))
        val trusted = empty.trust(presented)
        assertEquals(HostKeyCheck.Trusted, trusted.check(presented))
        // Host names compare case-insensitively; ports don't share trust.
        assertEquals(HostKeyCheck.Trusted, trusted.check(presented.copy(host = "box.local")))
        assertTrue(trusted.check(presented.copy(port = 2222)) is HostKeyCheck.Unknown)
        assertEquals(listOf("ssh-ed25519"), trusted.algorithmsFor("box.local", 22))
    }

    @Test
    fun aDifferentKeyIsAChangeNotAPrompt() {
        val known = KnownHosts().trust(PresentedKey("box", 22, "ssh-ed25519", blob))
        val swapped = PresentedKey("box", 22, "ssh-ed25519", other)
        val check = known.check(swapped)
        assertTrue(check is HostKeyCheck.Changed)
        check as HostKeyCheck.Changed
        assertEquals("SHA256:WKYFZFSoY9ATfSbq7u+Xfym3JKInQA8knEnuVJh2PLY", check.trusted.fingerprint)
        assertEquals(sshFingerprint(other), check.presented.fingerprint)
        // A different key type is also a change.
        assertTrue(known.check(PresentedKey("box", 22, "ecdsa-sha2-nistp256", blob)) is HostKeyCheck.Changed)
        // Replacing is explicit: forget, then trust.
        val replaced = known.forget("box", 22).trust(swapped)
        assertEquals(HostKeyCheck.Trusted, replaced.check(swapped))
        assertEquals(1, replaced.entries.size)
    }

    @Test
    fun storeRoundTripsAndSurvivesGarbage() {
        val file = File(tmp.root, "known_hosts.json")
        val store = KnownHostsStore(file)
        store.update { it.trust(PresentedKey("a", 22, "ssh-ed25519", blob)) }
        store.update { it.trust(PresentedKey("b", 2222, "ssh-ed25519", other)) }
        assertEquals(2, KnownHostsStore(file).load().entries.size)
        file.writeText("{not json")
        assertEquals(0, KnownHostsStore(file).load().entries.size)
    }
}
