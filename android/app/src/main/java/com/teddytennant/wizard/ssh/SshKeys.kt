package com.teddytennant.wizard.ssh

import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.common.Buffer
import net.schmizz.sshj.common.KeyType
import net.schmizz.sshj.userauth.keyprovider.KeyFormat
import net.schmizz.sshj.userauth.keyprovider.KeyProvider
import net.schmizz.sshj.userauth.keyprovider.KeyProviderUtil
import net.schmizz.sshj.userauth.keyprovider.FileKeyProvider
import net.schmizz.sshj.userauth.password.PasswordUtils
import net.schmizz.sshj.common.Factory
import org.bouncycastle.crypto.generators.Ed25519KeyPairGenerator
import org.bouncycastle.crypto.params.Ed25519KeyGenerationParameters
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.params.Ed25519PublicKeyParameters
import org.bouncycastle.crypto.util.OpenSSHPrivateKeyUtil
import org.bouncycastle.crypto.util.OpenSSHPublicKeyUtil
import java.security.PublicKey
import java.security.SecureRandom
import java.util.Base64

/** A private key in a text format sshj reads, plus what to show the user. */
data class KeyMaterial(val privateKey: String, val passphrase: String?, val publicKey: String)

class KeyImportException(message: String, cause: Throwable? = null) : Exception(message, cause)

object SshKeys {
    /** A fresh Ed25519 key as an unencrypted OpenSSH private key. It is sealed by [KeyVault] before storage. */
    fun generateEd25519(comment: String, random: SecureRandom = SecureRandom()): KeyMaterial {
        val generator = Ed25519KeyPairGenerator()
        generator.init(Ed25519KeyGenerationParameters(random))
        val pair = generator.generateKeyPair()
        val private = pair.private as Ed25519PrivateKeyParameters
        val public = pair.public as Ed25519PublicKeyParameters
        val pem = pem("OPENSSH PRIVATE KEY", OpenSSHPrivateKeyUtil.encodePrivateKey(private))
        val line = "ssh-ed25519 " + Base64.getEncoder().encodeToString(OpenSSHPublicKeyUtil.encodePublicKey(public)) +
            (if (comment.isBlank()) "" else " $comment")
        return KeyMaterial(pem, null, line)
    }

    /** Reads a pasted or picked private key (OpenSSH, PEM/PKCS#8 or PuTTY) and checks it opens. */
    fun import(privateKey: String, passphrase: String?, comment: String): KeyMaterial {
        val text = privateKey.trim().replace("\r\n", "\n") + "\n"
        val provider = try {
            provider(text, passphrase?.takeIf { it.isNotEmpty() })
        } catch (e: Exception) {
            throw KeyImportException("That doesn't look like a private key.", e)
        }
        val public = try {
            provider.public
        } catch (e: Exception) {
            val wrongPassphrase = e.javaClass.simpleName.contains("Password", ignoreCase = true) ||
                e.message.orEmpty().contains("passphrase", ignoreCase = true) ||
                e.message.orEmpty().contains("decrypt", ignoreCase = true)
            throw KeyImportException(
                if (wrongPassphrase || passphrase.isNullOrEmpty()) "The key didn't open. Check the passphrase." else "The key couldn't be read.",
                e,
            )
        }
        return KeyMaterial(text, passphrase?.takeIf { it.isNotEmpty() }, publicKeyLine(public, comment))
    }

    fun provider(privateKey: String, passphrase: String?): KeyProvider {
        val format = KeyProviderUtil.detectKeyFileFormat(privateKey, false)
        if (format == KeyFormat.Unknown) throw KeyImportException("unknown key format")
        val factory = Factory.Named.Util.create(DefaultConfig().fileKeyProviderFactories, format.toString())
            ?: throw KeyImportException("unsupported key format $format")
        val file = factory as FileKeyProvider
        if (passphrase != null) {
            file.init(privateKey, null, PasswordUtils.createOneOff(passphrase.toCharArray()))
        } else {
            file.init(privateKey, null)
        }
        return file
    }

    fun publicKeyBlob(key: PublicKey): ByteArray = Buffer.PlainBuffer().putPublicKey(key).compactData

    fun publicKeyLine(key: PublicKey, comment: String): String =
        KeyType.fromKey(key).toString() + " " + Base64.getEncoder().encodeToString(publicKeyBlob(key)) +
            (if (comment.isBlank()) "" else " $comment")

    fun fingerprintOfLine(line: String): String? =
        line.trim().split(Regex("\\s+")).getOrNull(1)?.let { runCatching { sshFingerprint(Base64.getDecoder().decode(it)) }.getOrNull() }

    private fun pem(label: String, der: ByteArray): String {
        val body = Base64.getEncoder().encodeToString(der).chunked(70).joinToString("\n")
        return "-----BEGIN $label-----\n$body\n-----END $label-----\n"
    }
}
