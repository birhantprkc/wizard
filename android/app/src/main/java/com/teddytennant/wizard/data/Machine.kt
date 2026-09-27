package com.teddytennant.wizard.data

import kotlinx.serialization.Serializable

@Serializable
enum class AuthKind { Key, Password }

/** An SSH machine the user added. Passwords live in the key vault, never here. */
@Serializable
data class Machine(
    val id: String,
    val name: String,
    val host: String,
    val port: Int = 22,
    val user: String,
    val auth: AuthKind = AuthKind.Key,
    val keyId: String? = null,
    /** Working directories sessions were started in, newest first. */
    val recentDirs: List<String> = emptyList(),
) {
    val address: String get() = if (port == 22) "$user@$host" else "$user@$host:$port"

    fun withRecentDir(dir: String): Machine = copy(recentDirs = (listOf(dir) + recentDirs.filter { it != dir }).take(8))

    companion object {
        fun passwordSecret(machineId: String) = "password:$machineId"
    }
}

/** Checks the add-machine form. Returns a message per invalid field. */
object MachineForm {
    private val hostPattern = Regex("^[A-Za-z0-9._:%\\[\\]-]+$")
    private val userPattern = Regex("^[A-Za-z0-9._@-]+$")

    data class Errors(val name: String? = null, val host: String? = null, val port: String? = null, val user: String? = null, val auth: String? = null) {
        val ok get() = listOf(name, host, port, user, auth).all { it == null }
    }

    fun validate(name: String, host: String, port: String, user: String, auth: AuthKind, keyId: String?, password: String, hasSavedPassword: Boolean): Errors = Errors(
        name = if (name.isBlank()) "Give it a name" else null,
        host = when {
            host.isBlank() -> "Enter a host name or IP"
            host.startsWith("-") || !hostPattern.matches(host.trim()) -> "That isn't a host name"
            else -> null
        },
        port = port.trim().toIntOrNull().let { if (it == null || it !in 1..65535) "Port is 1 to 65535" else null },
        user = when {
            user.isBlank() -> "Enter the user to log in as"
            !userPattern.matches(user.trim()) -> "That isn't a user name"
            else -> null
        },
        auth = when {
            auth == AuthKind.Key && keyId == null -> "Pick or generate a key"
            auth == AuthKind.Password && password.isEmpty() && !hasSavedPassword -> "Enter the password"
            else -> null
        },
    )

    /** `user@host:port` pasted into the host field fills the other fields. */
    fun splitAddress(input: String): Triple<String?, String, Int?> {
        val trimmed = input.trim()
        val user = trimmed.substringBefore('@', "").ifEmpty { null }
        val rest = trimmed.substringAfter('@')
        val port = if (!rest.startsWith("[") && rest.count { it == ':' } == 1) rest.substringAfter(':').toIntOrNull() else null
        val host = if (port != null) rest.substringBefore(':') else rest
        return Triple(user, host, port)
    }
}
