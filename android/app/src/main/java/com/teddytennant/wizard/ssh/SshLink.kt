package com.teddytennant.wizard.ssh

import net.schmizz.sshj.DefaultConfig
import net.schmizz.sshj.SSHClient
import net.schmizz.sshj.common.KeyType
import net.schmizz.sshj.transport.TransportException
import net.schmizz.sshj.transport.verification.HostKeyVerifier
import net.schmizz.sshj.userauth.UserAuthException
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.security.PublicKey
import java.util.concurrent.TimeUnit

sealed interface SshAuth {
    data class Key(val material: KeyMaterial) : SshAuth
    data class Password(val password: String) : SshAuth
}

data class SshTarget(val host: String, val port: Int, val user: String)

/** The server's key isn't trusted yet; ask the user about [presented]. */
class HostKeyUnknownException(val presented: PresentedKey) : IOException("unknown host key ${presented.fingerprint}")

/** The server's key differs from the trusted one. */
class HostKeyChangedException(val presented: PresentedKey, val trusted: KnownHost) :
    IOException("host key changed: was ${trusted.fingerprint}, now ${presented.fingerprint}")

class SshAuthException(message: String, cause: Throwable?) : IOException(message, cause)

data class ExecResult(val exitStatus: Int?, val stdout: String, val stderr: String)

/** One authenticated SSH connection (sshj). */
class SshLink private constructor(private val client: SSHClient) : RemoteExec, AutoCloseable {
    override val isConnected: Boolean get() = client.isConnected && client.isAuthenticated

    override fun run(command: String, timeoutSeconds: Long): ExecResult {
        client.startSession().use { session ->
            val cmd = session.exec(command)
            val errors = drain(cmd.errorStream)
            val out = cmd.inputStream.readBytes()
            cmd.join(timeoutSeconds, TimeUnit.SECONDS)
            errors.join(2_000)
            return ExecResult(cmd.exitStatus, String(out), errors.text())
        }
    }

    /** Runs [command] and hands its output over line by line, stdout and stderr merged by the script. */
    fun stream(command: String, onLine: (String) -> Unit): Int? {
        client.startSession().use { session ->
            val cmd = session.exec(command)
            cmd.inputStream.bufferedReader().forEachLine { onLine(it.trimEnd('\r')) }
            cmd.join(10, TimeUnit.SECONDS)
            return cmd.exitStatus
        }
    }

    override fun start(command: String): RemoteProcess {
        val session = client.startSession()
        val cmd = session.exec(command)
        val errors = drain(cmd.errorStream)
        return object : RemoteProcess {
            override val input: InputStream = cmd.inputStream
            override val output: OutputStream = cmd.outputStream
            override val stderrTail: String get() = errors.text().takeLast(2000)
            override fun close() {
                runCatching { cmd.outputStream.close() }
                runCatching { cmd.join(3, TimeUnit.SECONDS) }
                runCatching { cmd.close() }
                runCatching { session.close() }
                errors.interrupt()
            }
        }
    }

    override fun close() {
        runCatching { client.disconnect() }
    }

    /** Keeps the last few KB of a stream, and keeps the channel window open. */
    private class Drain(private val stream: InputStream) : Thread("ssh-stderr") {
        private val tail = ByteArrayOutputStream()
        init { isDaemon = true }
        override fun run() {
            val buf = ByteArray(4096)
            try {
                while (true) {
                    val n = stream.read(buf)
                    if (n < 0) break
                    synchronized(tail) {
                        tail.write(buf, 0, n)
                        if (tail.size() > 16_384) {
                            val keep = tail.toByteArray().takeLast(8_192).toByteArray()
                            tail.reset()
                            tail.write(keep)
                        }
                    }
                }
            } catch (_: IOException) {
            }
        }
        fun text(): String = synchronized(tail) { String(tail.toByteArray()) }
    }

    private fun drain(stream: InputStream) = Drain(stream).also { it.start() }

    companion object {
        /**
         * Connects and authenticates. Throws [HostKeyUnknownException] or
         * [HostKeyChangedException] before any credential is sent when the
         * server's key isn't the trusted one.
         */
        fun connect(target: SshTarget, auth: SshAuth, knownHosts: KnownHosts, timeoutMs: Int = 15_000): SshLink {
            val client = SSHClient(DefaultConfig())
            var verdict: HostKeyCheck? = null
            client.addHostKeyVerifier(object : HostKeyVerifier {
                override fun verify(hostname: String, port: Int, key: PublicKey): Boolean {
                    val presented = PresentedKey(target.host, target.port, KeyType.fromKey(key).toString(), SshKeys.publicKeyBlob(key))
                    val check = knownHosts.check(presented)
                    verdict = check
                    return check == HostKeyCheck.Trusted
                }

                override fun findExistingAlgorithms(hostname: String, port: Int): List<String> =
                    knownHosts.algorithmsFor(target.host, target.port)
            })
            client.connectTimeout = timeoutMs
            client.timeout = 0
            try {
                client.connect(target.host, target.port)
            } catch (e: TransportException) {
                runCatching { client.disconnect() }
                when (val v = verdict) {
                    is HostKeyCheck.Unknown -> throw HostKeyUnknownException(v.presented)
                    is HostKeyCheck.Changed -> throw HostKeyChangedException(v.presented, v.trusted)
                    else -> throw e
                }
            } catch (e: IOException) {
                runCatching { client.disconnect() }
                throw e
            }
            client.connection.keepAlive.keepAliveInterval = 20
            try {
                when (auth) {
                    is SshAuth.Key -> client.authPublickey(target.user, SshKeys.provider(auth.material.privateKey, auth.material.passphrase))
                    is SshAuth.Password -> client.authPassword(target.user, auth.password)
                }
            } catch (e: UserAuthException) {
                runCatching { client.disconnect() }
                throw SshAuthException(
                    when (auth) {
                        is SshAuth.Key -> "${target.user}@${target.host} didn't accept the key. Add its public key to ~/.ssh/authorized_keys there."
                        is SshAuth.Password -> "${target.user}@${target.host} didn't accept the password."
                    },
                    e,
                )
            } catch (e: Exception) {
                runCatching { client.disconnect() }
                throw e
            }
            return SshLink(client)
        }
    }
}
