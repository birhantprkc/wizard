package com.teddytennant.wizard.testing

import com.teddytennant.wizard.ssh.ExecResult
import com.teddytennant.wizard.ssh.RemoteExec
import com.teddytennant.wizard.ssh.RemoteProcess
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * A machine whose `claude` is played by the test: every turn process the
 * backend starts shows up in [processes], stdout is whatever the test
 * [FakeProcess.emit]s, and stdin lines land in [FakeProcess.written]. The
 * `initialize` probe for the model list gets an empty answer.
 */
class FakeClaude : RemoteExec {
    val processes = LinkedBlockingQueue<FakeProcess>()
    val started = CopyOnWriteArrayList<String>()
    @Volatile override var isConnected = true

    override fun run(command: String, timeoutSeconds: Long) = ExecResult(0, "", "")

    override fun start(command: String): RemoteProcess {
        val process = FakeProcess(command)
        if ("--include-partial-messages" !in command) {
            process.emit("""{"type":"control_response","response":{"subtype":"success","request_id":"wizard-android-init","response":{"models":[]}}}""")
            process.exit()
            return process
        }
        started += command
        processes += process
        return process
    }

    fun next(): FakeProcess = processes.poll(10, TimeUnit.SECONDS) ?: error("no claude process started")

    class FakeProcess(val command: String) : RemoteProcess {
        private val frames = LinkedBlockingQueue<Any>()
        val written = LinkedBlockingQueue<String>()
        @Volatile var closed = false
            private set

        fun emit(vararg lines: String) = lines.forEach { frames += (it + "\n").toByteArray() }
        fun emit(lines: List<String>) = emit(*lines.toTypedArray())

        /** The CLI exits: stdout ends. */
        fun exit() {
            frames += EOF
        }

        /** The link drops under the process. */
        fun drop() {
            frames += IOException("Broken transport")
        }

        /** The next line the backend writes that matches. */
        fun awaitWrite(match: (String) -> Boolean): String {
            while (true) {
                val line = written.poll(10, TimeUnit.SECONDS) ?: error("nothing matching was written")
                if (match(line)) return line
            }
        }

        override val stderrTail: String = ""

        override val input: InputStream = object : InputStream() {
            private var current: ByteArray = ByteArray(0)
            private var pos = 0
            private var done = false

            override fun read(): Int {
                val b = ByteArray(1)
                return if (read(b, 0, 1) < 0) -1 else b[0].toInt() and 0xff
            }

            override fun read(b: ByteArray, off: Int, len: Int): Int {
                if (done) return -1
                while (pos >= current.size) {
                    when (val next = frames.take()) {
                        EOF -> {
                            done = true
                            return -1
                        }
                        is IOException -> throw next
                        else -> {
                            current = next as ByteArray
                            pos = 0
                        }
                    }
                }
                val n = minOf(len, current.size - pos)
                System.arraycopy(current, pos, b, off, n)
                pos += n
                return n
            }
        }

        override val output: OutputStream = object : OutputStream() {
            private val buffer = StringBuilder()

            override fun write(b: Int) {
                if (closed) throw IOException("Stream closed")
                val c = b.toChar()
                if (c == '\n') {
                    written += buffer.toString()
                    buffer.clear()
                } else {
                    buffer.append(c)
                }
            }
        }

        /** Closing stdin ends the CLI, as it does the real one. */
        override fun close() {
            if (closed) return
            closed = true
            exit()
        }

        private companion object {
            val EOF = Any()
        }
    }
}
