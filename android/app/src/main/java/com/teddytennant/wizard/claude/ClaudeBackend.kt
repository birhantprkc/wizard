package com.teddytennant.wizard.claude

import com.teddytennant.wizard.acp.ConfigChoice
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.OpenedSession
import com.teddytennant.wizard.acp.PermissionOption
import com.teddytennant.wizard.acp.PermissionRequest
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionNotification
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.session.AgentBackend
import com.teddytennant.wizard.session.PermissionSink
import com.teddytennant.wizard.session.UpdateSink
import com.teddytennant.wizard.ssh.RemoteExec
import com.teddytennant.wizard.ssh.RemoteProcess
import com.teddytennant.wizard.ssh.RemoteScripts
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runInterruptible
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.put
import java.io.IOException
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/**
 * Claude Code over its stream-json protocol, one `claude -p` process per
 * turn, as Wizard GUI runs it. A new chat gets its id up front
 * (`--session-id`), later turns `--resume` it, and model and effort are
 * flags on the next turn. Saved sessions are read from ~/.claude/projects.
 */
class ClaudeBackend(
    private val exec: RemoteExec,
    private val onUpdate: UpdateSink,
    private val onPermission: PermissionSink,
) : AgentBackend {
    override val agent = Agent.ClaudeCode
    override val isClosed: Boolean get() = closed

    internal class Session(val cwd: String, var started: Boolean, var model: String = "default", var effort: String = "default")

    private val sessions = ConcurrentHashMap<String, Session>()
    private val running = ConcurrentHashMap<String, RemoteProcess>()
    private val interrupted = ConcurrentHashMap.newKeySet<String>()
    private val modelsLock = Mutex()
    private var models: List<ConfigChoice>? = null
    @Volatile private var closed = false

    /** The model list from an `initialize` control request: no prompt, no model call. */
    suspend fun models(): List<ConfigChoice> = modelsLock.withLock {
        models?.let { return it }
        val found = withContext(Dispatchers.IO) {
            val process = exec.start(RemoteScripts.claude(".", listOf("-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose")))
            try {
                process.output.write((ClaudeNormalizer.controlRequest("wizard-android-init", "initialize") + "\n").toByteArray())
                process.output.flush()
                withTimeoutOrNull(30_000) {
                    runInterruptible {
                        process.input.bufferedReader().lineSequence().firstNotNullOfOrNull { ClaudeNormalizer.initializeModels(it) }
                    }
                }
            } finally {
                process.close()
            }
        }
        (found ?: emptyList()).also { if (found != null) models = it }
    }

    private suspend fun optionsFor(session: Session) = ClaudeNormalizer.options(models(), session.model, session.effort)

    override suspend fun newSession(cwd: String): OpenedSession {
        val id = UUID.randomUUID().toString()
        val session = Session(cwd, started = false)
        sessions[id] = session
        return OpenedSession(id, optionsFor(session))
    }

    override suspend fun loadSession(sessionId: String, cwd: String): OpenedSession {
        val result = withContext(Dispatchers.IO) { exec.run(RemoteScripts.claudeTranscript(sessionId), 60) }
        if (result.exitStatus != 0) throw IOException(result.stderr.trim().ifEmpty { "Couldn't read that Claude Code session." })
        val normalizer = ClaudeNormalizer(live = false)
        for (line in result.stdout.lineSequence()) {
            for (event in normalizer.line(line)) {
                if (event is ClaudeEvent.Update) onUpdate(SessionNotification(sessionId, event.update, isReplay = true))
            }
        }
        val session = sessions.getOrPut(sessionId) { Session(cwd, started = true) }
        session.started = true
        return OpenedSession(sessionId, optionsFor(session))
    }

    override suspend fun listSessions(cwd: String?): List<SessionInfo> {
        val result = withContext(Dispatchers.IO) { exec.run(RemoteScripts.claudeSessions(), 30) }
        return result.stdout.lineSequence().mapNotNull { ClaudeNormalizer.sessionLine(it) }
            .filter { cwd == null || it.cwd == cwd }
            .toList()
    }

    override suspend fun setOption(sessionId: String, configId: String, value: String): List<ConfigOption> {
        val session = sessions[sessionId] ?: throw IOException("That session isn't open.")
        when (configId) {
            "model" -> session.model = value
            "thought_level" -> session.effort = value
        }
        return optionsFor(session)
    }

    internal fun flags(sessionId: String, session: Session): List<String> = buildList {
        addAll(listOf("-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"))
        addAll(listOf("--thinking-display", "summarized", "--permission-prompt-tool", "stdio", "--permission-mode", "default"))
        if (session.started) add("--resume=$sessionId") else addAll(listOf("--session-id", sessionId))
        if (session.model != "default") addAll(listOf("--model", session.model))
        if (session.effort != "default") addAll(listOf("--effort", session.effort))
    }

    override suspend fun prompt(sessionId: String, cwd: String, text: String): String {
        val session = sessions.getOrPut(sessionId) { Session(cwd, started = true) }
        interrupted.remove(sessionId)
        val process = withContext(Dispatchers.IO) { exec.start(RemoteScripts.claude(session.cwd, flags(sessionId, session))) }
        running[sessionId] = process
        val writeLock = Mutex()
        suspend fun write(line: String) = writeLock.withLock {
            withContext(Dispatchers.IO) {
                process.output.write((line + "\n").toByteArray())
                process.output.flush()
            }
        }
        try {
            write(ClaudeNormalizer.userLine(text))
            val normalizer = ClaudeNormalizer(live = true)
            val reader = process.input.bufferedReader()
            while (true) {
                val line = withContext(Dispatchers.IO) { runInterruptible { reader.readLine() } } ?: break
                for (event in normalizer.line(line)) {
                    when (event) {
                        is ClaudeEvent.Update -> onUpdate(SessionNotification(sessionId, event.update, isReplay = false))
                        is ClaudeEvent.ToolPermission -> write(ClaudeNormalizer.allow(event.requestId, answer(sessionId, event)))
                        is ClaudeEvent.Result -> {
                            session.started = true
                            return when {
                                sessionId in interrupted -> "cancelled"
                                event.isError -> throw IOException(event.error ?: "Claude Code stopped with ${event.subtype}.")
                                else -> "end_turn"
                            }
                        }
                    }
                }
            }
            if (sessionId in interrupted) return "cancelled"
            val detail = process.stderrTail.trim().lines().lastOrNull { it.isNotBlank() }
            throw IOException(detail ?: "Claude Code exited without finishing the turn.")
        } finally {
            running.remove(sessionId)
            withContext(Dispatchers.IO) { process.close() }
        }
    }

    /**
     * Tools are allowed without asking, as on the desktop; `AskUserQuestion`
     * goes to the user, one question at a time, answers keyed by question text.
     */
    private suspend fun answer(sessionId: String, request: ClaudeEvent.ToolPermission): JsonObject {
        if (request.toolName != "AskUserQuestion") return request.input
        val questions = (request.input["questions"] as? JsonArray).orEmpty().mapNotNull { it as? JsonObject }
        val answers = buildJsonObject {
            for (q in questions) {
                val text = (q["question"] as? JsonPrimitive)?.contentOrNull ?: (q["prompt"] as? JsonPrimitive)?.contentOrNull ?: continue
                val labels = (q["options"] as? JsonArray).orEmpty().mapNotNull { o ->
                    (o as? JsonPrimitive)?.contentOrNull ?: ((o as? JsonObject)?.get("label") as? JsonPrimitive)?.contentOrNull
                }
                val picked = onPermission(
                    PermissionRequest(sessionId, text, null, labels.map { PermissionOption(it, it, "allow_once") }),
                ).optionId
                put(text, picked ?: "")
            }
        }
        return JsonObject(request.input + ("answers" to answers))
    }

    override suspend fun cancel(sessionId: String) {
        val process = running[sessionId] ?: return
        interrupted += sessionId
        withContext(Dispatchers.IO) {
            runCatching {
                process.output.write((ClaudeNormalizer.controlRequest("wizard-android-interrupt", "interrupt") + "\n").toByteArray())
                process.output.flush()
            }
        }
    }

    override fun close() {
        closed = true
        running.values.forEach { runCatching { it.close() } }
    }
}
