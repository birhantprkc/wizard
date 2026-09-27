package com.teddytennant.wizard.ssh

import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json
import java.io.File
import java.util.UUID

@Serializable
data class StoredKey(
    val id: String,
    val label: String,
    val publicKey: String,
    val imported: Boolean,
    val createdAt: Long,
) {
    val fingerprint: String? get() = SshKeys.fingerprintOfLine(publicKey)
    val algorithm: String get() = publicKey.substringBefore(' ')
}

@Serializable
private data class SealedKey(val privateKey: String, val passphrase: String? = null)

/**
 * SSH keys and passwords, sealed with [SecretBox] (the Android Keystore on a
 * device). The index holds only public halves and labels.
 */
class KeyVault(private val dir: File, private val box: SecretBox, private val clock: () -> Long = System::currentTimeMillis) {
    private val json = Json { ignoreUnknownKeys = true }
    private val indexFile get() = File(dir, "keys.json")
    private val indexSerializer = ListSerializer(StoredKey.serializer())

    @Synchronized
    fun list(): List<StoredKey> =
        if (!indexFile.exists()) emptyList()
        else runCatching { json.decodeFromString(indexSerializer, indexFile.readText()) }.getOrElse { emptyList() }

    fun get(id: String): StoredKey? = list().firstOrNull { it.id == id }

    fun generate(label: String): StoredKey = store(label, SshKeys.generateEd25519(label), imported = false)

    fun import(label: String, privateKey: String, passphrase: String?): StoredKey =
        store(label, SshKeys.import(privateKey, passphrase, label), imported = true)

    @Synchronized
    private fun store(label: String, material: KeyMaterial, imported: Boolean): StoredKey {
        val key = StoredKey(UUID.randomUUID().toString(), label, material.publicKey, imported, clock())
        write(File(dir, "${key.id}.key"), box.seal(json.encodeToString(SealedKey.serializer(), SealedKey(material.privateKey, material.passphrase)).toByteArray()))
        writeIndex(list() + key)
        return key
    }

    /** The private half, unsealed, for one connection. */
    fun material(id: String): KeyMaterial {
        val stored = get(id) ?: throw NoSuchElementException("key $id is gone")
        val sealed = json.decodeFromString(SealedKey.serializer(), String(box.open(File(dir, "$id.key").readBytes())))
        return KeyMaterial(sealed.privateKey, sealed.passphrase, stored.publicKey)
    }

    @Synchronized
    fun rename(id: String, label: String) = writeIndex(list().map { if (it.id == id) it.copy(label = label) else it })

    @Synchronized
    fun delete(id: String) {
        File(dir, "$id.key").delete()
        writeIndex(list().filterNot { it.id == id })
    }

    fun putSecret(name: String, value: String) = write(secretFile(name), box.seal(value.toByteArray()))

    fun secret(name: String): String? = secretFile(name).takeIf { it.exists() }?.let { String(box.open(it.readBytes())) }

    fun deleteSecret(name: String) {
        secretFile(name).delete()
    }

    private fun secretFile(name: String) = File(dir, "secret-" + name.replace(Regex("[^A-Za-z0-9_.-]"), "_") + ".bin")

    private fun writeIndex(keys: List<StoredKey>) = write(indexFile, json.encodeToString(indexSerializer, keys).toByteArray())

    private fun write(file: File, bytes: ByteArray) {
        dir.mkdirs()
        val tmp = File(dir, file.name + ".tmp")
        tmp.writeBytes(bytes)
        if (!tmp.renameTo(file)) {
            file.writeBytes(bytes)
            tmp.delete()
        }
    }
}
