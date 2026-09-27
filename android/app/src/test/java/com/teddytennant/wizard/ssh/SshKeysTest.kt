package com.teddytennant.wizard.ssh

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.util.Base64
import javax.crypto.KeyGenerator

class SshKeysTest {
    @get:Rule val tmp = TemporaryFolder()

    private fun softwareBox(): AesGcmBox {
        val key = KeyGenerator.getInstance("AES").apply { init(256) }.generateKey()
        return AesGcmBox { key }
    }

    @Test
    fun generatedKeyIsOpenSshEd25519AndOpensInSshj() {
        val key = SshKeys.generateEd25519("wizard@pixel")
        assertTrue(key.privateKey.startsWith("-----BEGIN OPENSSH PRIVATE KEY-----\n"))
        val parts = key.publicKey.split(" ")
        assertEquals("ssh-ed25519", parts[0])
        assertEquals("wizard@pixel", parts[2])
        val blob = Base64.getDecoder().decode(parts[1])
        assertEquals(51, blob.size) // string "ssh-ed25519" + string of 32 bytes

        val provider = SshKeys.provider(key.privateKey, null)
        assertTrue(provider.public.algorithm.contains("25519") || provider.public.algorithm.contains("EdDSA"))
        assertTrue(blob.contentEquals(SshKeys.publicKeyBlob(provider.public)))
        assertNotEquals(key.publicKey, SshKeys.generateEd25519("wizard@pixel").publicKey)
    }

    @Test
    fun sshKeygenAgreesWithTheGeneratedPublicKey() {
        val keygen = listOf("/run/current-system/sw/bin/ssh-keygen", "/usr/bin/ssh-keygen").map(::File).firstOrNull(File::canExecute)
        assumeTrue(keygen != null)
        val key = SshKeys.generateEd25519("")
        val file = File(tmp.root, "id").apply { writeText(key.privateKey); setReadable(false, false); setReadable(true, true) }
        val out = ProcessBuilder(keygen!!.path, "-y", "-f", file.path).redirectErrorStream(true).start()
        val derived = out.inputStream.bufferedReader().readText().trim()
        assertEquals(0, out.waitFor())
        assertEquals(key.publicKey, derived.split(" ").take(2).joinToString(" "))
    }

    @Test
    fun importsAPassphraseProtectedKeyAndRejectsTheWrongPassphrase() {
        val keygen = listOf("/run/current-system/sw/bin/ssh-keygen", "/usr/bin/ssh-keygen").map(::File).firstOrNull(File::canExecute)
        assumeTrue(keygen != null)
        val path = File(tmp.root, "enc")
        val made = ProcessBuilder(keygen!!.path, "-q", "-t", "ed25519", "-N", "hunter2", "-C", "laptop", "-f", path.path).start()
        assertEquals(0, made.waitFor())
        val expectedPublic = File(path.path + ".pub").readText().trim().split(" ").take(2).joinToString(" ")

        val imported = SshKeys.import(path.readText(), "hunter2", "laptop")
        assertEquals("$expectedPublic laptop", imported.publicKey)

        val wrong = runCatching { SshKeys.import(path.readText(), "nope", "laptop") }.exceptionOrNull()
        assertTrue(wrong is KeyImportException)
        val garbage = runCatching { SshKeys.import("hello", null, "x") }.exceptionOrNull()
        assertTrue(garbage is KeyImportException)
    }

    @Test
    fun vaultSealsPrivateKeysAndPasswords() {
        val dir = tmp.newFolder("vault")
        val vault = KeyVault(dir, softwareBox(), clock = { 42L })
        val key = vault.generate("Pixel 9")
        assertEquals(listOf(key), vault.list())
        assertEquals(42L, key.createdAt)
        assertEquals("ssh-ed25519", key.algorithm)
        assertTrue(key.fingerprint!!.startsWith("SHA256:"))

        val material = vault.material(key.id)
        assertTrue(material.privateKey.contains("OPENSSH PRIVATE KEY"))
        // Nothing on disk holds the private key in the clear.
        dir.listFiles()!!.forEach { f -> assertTrue(f.name, !f.readText(Charsets.ISO_8859_1).contains("OPENSSH PRIVATE KEY")) }

        vault.putSecret("password:m1", "correct horse")
        assertEquals("correct horse", vault.secret("password:m1"))
        assertTrue(dir.listFiles()!!.none { it.readText(Charsets.ISO_8859_1).contains("correct horse") })
        vault.deleteSecret("password:m1")
        assertEquals(null, vault.secret("password:m1"))

        vault.rename(key.id, "Phone")
        assertEquals("Phone", vault.get(key.id)!!.label)
        vault.delete(key.id)
        assertTrue(vault.list().isEmpty())
        assertTrue(dir.listFiles()!!.none { it.name.endsWith(".key") })
    }

    @Test
    fun sealedSecretsDontOpenWithAnotherKey() {
        val sealed = softwareBox().seal("x".toByteArray())
        assertTrue(runCatching { softwareBox().open(sealed) }.isFailure)
        assertTrue(runCatching { softwareBox().open(byteArrayOf(1, 2, 3)) }.isFailure)
    }
}
