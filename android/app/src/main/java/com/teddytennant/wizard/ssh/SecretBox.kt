package com.teddytennant.wizard.ssh

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.nio.ByteBuffer
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Seals small secrets (private keys, passwords) before they touch disk. */
interface SecretBox {
    fun seal(plain: ByteArray): ByteArray
    fun open(sealed: ByteArray): ByteArray
}

/** AES-GCM; the output is `[iv length][iv][ciphertext+tag]`. */
class AesGcmBox(private val key: () -> SecretKey) : SecretBox {
    override fun seal(plain: ByteArray): ByteArray {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val iv = cipher.iv
        val body = cipher.doFinal(plain)
        return ByteBuffer.allocate(1 + iv.size + body.size).put(iv.size.toByte()).put(iv).put(body).array()
    }

    override fun open(sealed: ByteArray): ByteArray {
        require(sealed.isNotEmpty()) { "empty secret" }
        val ivLength = sealed[0].toInt()
        require(ivLength in 12..16 && sealed.size > 1 + ivLength) { "not a sealed secret" }
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, sealed, 1, ivLength))
        return cipher.doFinal(sealed, 1 + ivLength, sealed.size - 1 - ivLength)
    }

    private companion object {
        const val TRANSFORMATION = "AES/GCM/NoPadding"
    }
}

/** A non-exportable AES key in the Android Keystore. */
object KeystoreKey {
    private const val ALIAS = "wizard-vault"

    fun get(): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return generator.generateKey()
    }
}
