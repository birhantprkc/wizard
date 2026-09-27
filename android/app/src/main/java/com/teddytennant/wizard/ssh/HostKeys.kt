package com.teddytennant.wizard.ssh

import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json
import java.io.File
import java.security.MessageDigest
import java.util.Base64

/** `SHA256:<base64 without padding>`, the form `ssh-keygen -l` prints. */
fun sshFingerprint(keyBlob: ByteArray): String =
    "SHA256:" + Base64.getEncoder().withoutPadding().encodeToString(MessageDigest.getInstance("SHA-256").digest(keyBlob))

@Serializable
data class KnownHost(val host: String, val port: Int, val keyType: String, val key: String) {
    val fingerprint: String get() = sshFingerprint(Base64.getDecoder().decode(key))
}

/** The host key a server presented. */
data class PresentedKey(val host: String, val port: Int, val keyType: String, val blob: ByteArray) {
    val fingerprint: String get() = sshFingerprint(blob)
    fun toKnown() = KnownHost(host.lowercase(), port, keyType, Base64.getEncoder().encodeToString(blob))

    override fun equals(other: Any?) = other is PresentedKey && host == other.host && port == other.port &&
        keyType == other.keyType && blob.contentEquals(other.blob)
    override fun hashCode() = (host.hashCode() * 31 + port) * 31 + blob.contentHashCode()
}

sealed interface HostKeyCheck {
    /** Same key as last time. */
    data object Trusted : HostKeyCheck

    /** First contact: show the fingerprint and ask. */
    data class Unknown(val presented: PresentedKey) : HostKeyCheck

    /** The server's key is not the one trusted before. Refuse unless the user explicitly replaces it. */
    data class Changed(val presented: PresentedKey, val trusted: KnownHost) : HostKeyCheck
}

/** Trust on first use, one key per host and port. */
data class KnownHosts(val entries: List<KnownHost> = emptyList()) {
    fun find(host: String, port: Int): KnownHost? = entries.firstOrNull { it.host == host.lowercase() && it.port == port }

    fun check(presented: PresentedKey): HostKeyCheck {
        val known = find(presented.host, presented.port) ?: return HostKeyCheck.Unknown(presented)
        val same = known.keyType == presented.keyType &&
            Base64.getDecoder().decode(known.key).contentEquals(presented.blob)
        return if (same) HostKeyCheck.Trusted else HostKeyCheck.Changed(presented, known)
    }

    /** Key types to ask the server for first, so a host with several keys offers the trusted one. */
    fun algorithmsFor(host: String, port: Int): List<String> = listOfNotNull(find(host, port)?.keyType)

    fun trust(presented: PresentedKey): KnownHosts =
        KnownHosts(entries.filterNot { it.host == presented.host.lowercase() && it.port == presented.port } + presented.toKnown())

    fun forget(host: String, port: Int): KnownHosts =
        KnownHosts(entries.filterNot { it.host == host.lowercase() && it.port == port })
}

/** [KnownHosts] kept in a JSON file in app-private storage. */
class KnownHostsStore(private val file: File) {
    private val json = Json { ignoreUnknownKeys = true }
    private val serializer = ListSerializer(KnownHost.serializer())

    @Synchronized
    fun load(): KnownHosts = if (!file.exists()) {
        KnownHosts()
    } else {
        runCatching { KnownHosts(json.decodeFromString(serializer, file.readText())) }.getOrElse { KnownHosts() }
    }

    @Synchronized
    fun update(change: (KnownHosts) -> KnownHosts): KnownHosts {
        val next = change(load())
        file.parentFile?.mkdirs()
        val tmp = File(file.parentFile, file.name + ".tmp")
        tmp.writeText(json.encodeToString(serializer, next.entries))
        if (!tmp.renameTo(file)) {
            file.writeText(tmp.readText())
            tmp.delete()
        }
        return next
    }
}
