package com.teddytennant.wizard.testing

import com.teddytennant.wizard.ssh.ExecResult
import com.teddytennant.wizard.ssh.RemoteExec
import com.teddytennant.wizard.ssh.RemoteProcess
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.TimeUnit

/** Runs the app's remote commands in a local shell, the way an SSH login shell would. */
class LocalExec(private val home: File? = null) : RemoteExec {
    private fun builder(command: String) = ProcessBuilder("/bin/sh", "-c", command).apply {
        if (home != null) environment()["HOME"] = home.path
    }

    override fun run(command: String, timeoutSeconds: Long): ExecResult {
        val p = builder(command).start()
        val err = StringBuilder()
        val t = Thread { err.append(p.errorStream.bufferedReader().readText()) }.apply { start() }
        val out = p.inputStream.bufferedReader().readText()
        p.waitFor(timeoutSeconds, TimeUnit.SECONDS)
        t.join(2_000)
        return ExecResult(p.exitValue(), out, err.toString())
    }

    override fun start(command: String): RemoteProcess {
        val p = builder(command).start()
        val err = StringBuilder()
        Thread { runCatching { p.errorStream.bufferedReader().forEachLine { synchronized(err) { err.appendLine(it) } } } }.apply { isDaemon = true; start() }
        return object : RemoteProcess {
            override val input: InputStream = p.inputStream
            override val output: OutputStream = p.outputStream
            override val stderrTail: String get() = synchronized(err) { err.toString().takeLast(2000) }
            override fun close() {
                runCatching { p.outputStream.close() }
                if (!p.waitFor(5, TimeUnit.SECONDS)) p.destroyForcibly()
            }
        }
    }
}
