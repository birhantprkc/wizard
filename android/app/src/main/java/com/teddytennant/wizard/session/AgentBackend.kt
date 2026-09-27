package com.teddytennant.wizard.session

import com.teddytennant.wizard.acp.AcpClient
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.OpenedSession
import com.teddytennant.wizard.acp.PermissionAnswer
import com.teddytennant.wizard.acp.PermissionRequest
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionNotification
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.ssh.RemoteExec
import com.teddytennant.wizard.ssh.RemoteScripts
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import java.io.IOException

/**
 * One agent on one machine, whatever protocol it speaks. Updates come back
 * through the callbacks it was built with, as [SessionNotification]s, so every
 * agent lands in the same transcript model.
 */
interface AgentBackend {
    val agent: Agent
    val isClosed: Boolean
    suspend fun newSession(cwd: String): OpenedSession
    /** Replays the saved transcript (marked as replay) before returning. */
    suspend fun loadSession(sessionId: String, cwd: String): OpenedSession
    suspend fun listSessions(cwd: String? = null): List<SessionInfo>
    suspend fun setOption(sessionId: String, configId: String, value: String): List<ConfigOption>
    /** Runs one turn and returns ACP's stop reason. */
    suspend fun prompt(sessionId: String, cwd: String, text: String): String
    /** Background tasks the session's last turn left running, which will report back through an [ActivitySink]. */
    fun backgroundTasks(sessionId: String): Int = 0
    suspend fun cancel(sessionId: String)
    fun close()
}

typealias UpdateSink = suspend (SessionNotification) -> Unit
typealias PermissionSink = suspend (PermissionRequest) -> PermissionAnswer
typealias ActivitySink = suspend (Activity) -> Unit

/**
 * What a session does after [AgentBackend.prompt] returned: Claude Code
 * keeps background tasks running and wakes up on its own when one finishes.
 */
sealed interface Activity {
    val sessionId: String

    /** [count] background tasks are running. */
    data class Tasks(override val sessionId: String, val count: Int) : Activity

    /** A turn nobody sent started, the wake turn after a background task. */
    data class WakeStarted(override val sessionId: String) : Activity

    data class WakeEnded(override val sessionId: String, val error: String?) : Activity

    /** Background work or a wake turn happened, and now nothing is left running. Sent once each time. */
    data class Idle(override val sessionId: String) : Activity

    /**
     * The agent exited with work outstanding. [lost] means the connection
     * went; otherwise [detail] is what it said, null when it was asked to stop.
     */
    data class Stopped(override val sessionId: String, val lost: Boolean, val detail: String?) : Activity
}

/** Wizard (`wizard acp`) and Pi (`pi-acp`): one ACP process serves every session. */
class AcpBackend private constructor(override val agent: Agent, private val client: AcpClient) : AgentBackend {
    override val isClosed: Boolean get() = client.closed.isCompleted

    override suspend fun newSession(cwd: String) = client.newSession(cwd)
    override suspend fun loadSession(sessionId: String, cwd: String) = client.loadSession(sessionId, cwd)

    override suspend fun listSessions(cwd: String?): List<SessionInfo> {
        val all = mutableListOf<SessionInfo>()
        var cursor: String? = null
        repeat(3) {
            val page = client.listSessions(cwd, cursor)
            all += page.sessions
            cursor = page.nextCursor ?: return all
        }
        return all
    }

    override suspend fun setOption(sessionId: String, configId: String, value: String) = client.setConfigOption(sessionId, configId, value)
    override suspend fun prompt(sessionId: String, cwd: String, text: String) = client.prompt(sessionId, text)
    override suspend fun cancel(sessionId: String) = client.cancel(sessionId)
    override fun close() = client.close()

    companion object {
        suspend fun start(
            agent: Agent,
            exec: RemoteExec,
            scope: CoroutineScope,
            clientVersion: String,
            onUpdate: UpdateSink,
            onPermission: PermissionSink,
        ): AcpBackend {
            val process = withContext(Dispatchers.IO) { exec.start(RemoteScripts.acp(agent)) }
            val client = AcpClient(process, scope, onUpdate, onPermission)
            try {
                withTimeout(45_000) { client.initialize(clientVersion) }
            } catch (e: Exception) {
                client.close()
                if (e is kotlinx.coroutines.CancellationException && e !is kotlinx.coroutines.TimeoutCancellationException) throw e
                val detail = process.stderrTail.trim().lines().lastOrNull { it.isNotBlank() }
                throw IOException("${agent.displayName} didn't start" + (detail?.let { ": $it" } ?: ""), e)
            }
            return AcpBackend(agent, client)
        }
    }
}
