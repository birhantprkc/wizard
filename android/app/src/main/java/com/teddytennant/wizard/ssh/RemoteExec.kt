package com.teddytennant.wizard.ssh

import com.teddytennant.wizard.acp.AcpTransport

/** A process started on a machine: stdio plus what it said on stderr. */
interface RemoteProcess : AcpTransport {
    /** The last few KB of stderr. */
    val stderrTail: String
}

/** Runs commands on a machine. [SshLink] over SSH; a local shell in tests. */
interface RemoteExec {
    /** False once the link to the machine is gone. */
    val isConnected: Boolean get() = true
    fun run(command: String, timeoutSeconds: Long = 30): ExecResult
    fun start(command: String): RemoteProcess
}
