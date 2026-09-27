package com.teddytennant.wizard.claude

import com.teddytennant.wizard.acp.ConfigChoice
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.ConnectionClosedException
import com.teddytennant.wizard.acp.OpenedSession
import com.teddytennant.wizard.acp.PermissionOption
import com.teddytennant.wizard.acp.PermissionRequest
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionNotification
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.session.Activity
import com.teddytennant.wizard.session.ActivitySink
import com.teddytennant.wizard.session.AgentBackend
import com.teddytennant.wizard.session.PermissionSink
import com.teddytennant.wizard.session.UpdateSink
import com.teddytennant.wizard.ssh.RemoteExec
import com.teddytennant.wizard.ssh.RemoteProcess
import com.teddytennant.wizard.ssh.RemoteScripts
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
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
 * Claude Code over its stream-json protocol, as Wizard GUI runs it
 * (gui/crates/harness/src/claude): one `claude -p` per open session, kept
 * running between turns. A prompt is a user line on its stdin and returns at
 * that turn's `result`, but the process stays up, because a turn can leave
 * background tasks behind. The CLI runs a wake turn on its own when one
 * finishes (a fresh `init`, output, another `result`), and the reader here
 * forwards it and reports it through [onActivity]. Tools run with
 * `bypassPermissions`, so they never depend on this channel; `AskUserQuestion`
 * still comes over stdio and is answered whenever it arrives.
 *
 * A new chat gets its id up front (`--session-id`), a restarted process
 * `--resume`s it, and model and effort are flags at start. The process closes
 * on [cancel], [close], or after [Timing.idleCloseMs] with nothing running.
 * Saved sessions are read from ~/.claude/projects.
 */
class ClaudeBackend(
    private val exec: RemoteExec,
    private val onUpdate: UpdateSink,
    private val onPermission: PermissionSink,
    private val onActivity: ActivitySink = {},
    private val scope: CoroutineScope = CoroutineScope(SupervisorJob() + Dispatchers.IO),
    private val timing: Timing = Timing(),
) : AgentBackend {
    override val agent = Agent.ClaudeCode
    override val isClosed: Boolean get() = closed

    /**
     * [settleMs]: how long a session has to stay quiet after its last task or
     * wake turn before it counts as done; tasks finishing together wake it
     * more than once. [idleCloseMs]: when a quiet process is closed.
     * [interruptGraceMs]: how long an interrupt gets before the process is closed.
     */
    data class Timing(val settleMs: Long = 3_000, val idleCloseMs: Long = 10 * 60_000, val interruptGraceMs: Long = 5_000)

    internal class Session(val cwd: String, var started: Boolean, var model: String = "default", var effort: String = "default")

    /** One running `claude -p`. Fields are guarded by the object's monitor. */
    private class Proc(val sessionId: String, val process: RemoteProcess, val model: String, val effort: String) {
        val writeLock = Mutex()
        /** A model turn is running, the user's or a wake. */
        var turn = false
        /** The prompt waiting for this turn's result; null during a wake turn. */
        var pending: CompletableDeferred<String>? = null
        val tasks = mutableSetOf<String>()
        /** Once the CLI lists its background tasks, single start and end frames are ignored. */
        var listed = false
        /** Background work or a wake happened since the last prompt, so an [Activity.Idle] is due. */
        var owed = false
        var interrupted = false
        var closing = false
        var quiet: Job? = null

        val busy: Boolean get() = turn || tasks.isNotEmpty()
    }

    private val sessions = ConcurrentHashMap<String, Session>()
    private val procs = ConcurrentHashMap<String, Proc>()
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
        // Tools are allowed without asking, as on the desktop with auto-approve on. The stdio
        // prompt tool stays for AskUserQuestion. As root the launch script falls back to
        // `default`, since the CLI refuses to bypass permissions there.
        addAll(listOf("--thinking-display", "summarized", "--permission-prompt-tool", "stdio"))
        addAll(listOf("--permission-mode", "bypassPermissions", "--dangerously-skip-permissions"))
        if (session.started) add("--resume=$sessionId") else addAll(listOf("--session-id", sessionId))
        if (session.model != "default") addAll(listOf("--model", session.model))
        if (session.effort != "default") addAll(listOf("--effort", session.effort))
    }

    override suspend fun prompt(sessionId: String, cwd: String, text: String): String {
        if (closed) throw ConnectionClosedException("The connection to the machine was closed.")
        val session = sessions.getOrPut(sessionId) { Session(cwd, started = true) }
        // A process that died or closed while idle is replaced once, with --resume.
        for (attempt in 1..2) {
            val proc = process(sessionId, session)
            val result = CompletableDeferred<String>()
            val open = synchronized(proc) {
                if (!proc.closing) {
                    proc.pending = result
                    proc.turn = true
                    proc.owed = false
                    proc.interrupted = false
                    proc.quiet?.cancel()
                }
                !proc.closing
            }
            if (!open) continue
            try {
                write(proc, ClaudeNormalizer.userLine(text))
            } catch (e: IOException) {
                synchronized(proc) { proc.pending = null }
                shut(proc)
                if (!exec.isConnected) throw ConnectionClosedException("The connection to the machine dropped.", e)
                if (attempt == 2) throw e
                continue
            }
            return result.await()
        }
        throw IOException("Claude Code didn't start.")
    }

    override fun backgroundTasks(sessionId: String): Int = procs[sessionId]?.let { synchronized(it) { it.tasks.size } } ?: 0

    /** The session's process, started if there is none. One started with other flags is replaced once it is quiet. */
    private suspend fun process(sessionId: String, session: Session): Proc {
        procs[sessionId]?.let { proc ->
            val stale = proc.model != session.model || proc.effort != session.effort
            if (!stale || synchronized(proc) { proc.busy }) return proc
            shut(proc)
        }
        val process = withContext(Dispatchers.IO) { exec.start(RemoteScripts.claude(session.cwd, flags(sessionId, session))) }
        val proc = Proc(sessionId, process, session.model, session.effort)
        procs[sessionId] = proc
        scope.launch(Dispatchers.IO) { read(proc, session) }
        return proc
    }

    private suspend fun write(proc: Proc, line: String) = proc.writeLock.withLock {
        withContext(Dispatchers.IO) {
            proc.process.output.write((line + "\n").toByteArray())
            proc.process.output.flush()
        }
    }

    /** Reads the process until it exits, whether or not a prompt is waiting. */
    private suspend fun read(proc: Proc, session: Session) {
        val normalizer = ClaudeNormalizer(live = true)
        val reader = proc.process.input.bufferedReader()
        var failure: IOException? = null
        try {
            while (true) {
                val line = runInterruptible { reader.readLine() } ?: break
                for (event in normalizer.line(line)) handle(proc, session, event)
            }
        } catch (e: IOException) {
            failure = e
        }
        exited(proc, failure)
    }

    private suspend fun handle(proc: Proc, session: Session, event: ClaudeEvent) {
        val id = proc.sessionId
        when (event) {
            is ClaudeEvent.Update -> onUpdate(SessionNotification(id, event.update, isReplay = false))
            // Answered off the reader: a question waits on the user while other frames keep coming.
            is ClaudeEvent.ToolPermission -> scope.launch {
                runCatching { write(proc, ClaudeNormalizer.allow(event.requestId, answer(id, event))) }
            }
            ClaudeEvent.TurnStarted -> {
                val wake = synchronized(proc) {
                    proc.quiet?.cancel()
                    (!proc.turn).also {
                        if (it) {
                            proc.turn = true
                            proc.owed = true
                        }
                    }
                }
                if (wake) onActivity(Activity.WakeStarted(id))
            }
            is ClaudeEvent.Tasks -> {
                val (before, after) = synchronized(proc) {
                    val before = proc.tasks.size
                    if (event.all != null) {
                        proc.listed = true
                        proc.tasks.clear()
                        proc.tasks += event.all
                    } else if (!proc.listed) {
                        event.started?.let { proc.tasks += it }
                        event.ended?.let { proc.tasks -= it }
                    }
                    if (proc.tasks.isNotEmpty() && !proc.turn) proc.owed = true
                    before to proc.tasks.size
                }
                if (before != after) onActivity(Activity.Tasks(id, after))
                settle(proc)
            }
            is ClaudeEvent.Result -> {
                session.started = true
                val (waiter, interrupted) = synchronized(proc) {
                    proc.turn = false
                    val waiter = proc.pending
                    proc.pending = null
                    if (waiter != null && proc.tasks.isNotEmpty()) proc.owed = true
                    waiter to proc.interrupted
                }
                val error = if (event.isError) event.error ?: "Claude Code stopped with ${event.subtype}." else null
                when {
                    waiter == null -> onActivity(Activity.WakeEnded(id, error.takeUnless { interrupted }))
                    interrupted -> waiter.complete("cancelled")
                    error != null -> waiter.completeExceptionally(IOException(error))
                    else -> waiter.complete("end_turn")
                }
                // Stop ends the process, background tasks and all, as on the desktop.
                if (interrupted) shut(proc) else settle(proc)
            }
        }
    }

    /** Once nothing runs: [Activity.Idle] after [Timing.settleMs] if it's owed, then close after [Timing.idleCloseMs]. */
    private fun settle(proc: Proc) = synchronized(proc) {
        proc.quiet?.cancel()
        proc.quiet = null
        if (proc.busy || proc.closing) return@synchronized
        proc.quiet = scope.launch {
            val owed = synchronized(proc) { proc.owed }
            if (owed) {
                delay(timing.settleMs)
                synchronized(proc) { proc.owed = false }
                onActivity(Activity.Idle(proc.sessionId))
            }
            delay(timing.idleCloseMs)
            shut(proc, onlyIfQuiet = true)
        }
    }

    private suspend fun exited(proc: Proc, failure: IOException?) {
        procs.remove(proc.sessionId, proc)
        val waiter: CompletableDeferred<String>?
        val busy: Boolean
        val owed: Boolean
        val asked: Boolean
        synchronized(proc) {
            proc.quiet?.cancel()
            waiter = proc.pending
            proc.pending = null
            busy = proc.busy
            owed = proc.owed
            asked = proc.closing || proc.interrupted
        }
        withContext(NonCancellable + Dispatchers.IO) { runCatching { proc.process.close() } }
        val lost = !asked && (failure != null || !exec.isConnected)
        val detail = proc.process.stderrTail.trim().lines().lastOrNull { it.isNotBlank() }
        when {
            waiter != null && asked -> waiter.complete("cancelled")
            waiter != null && lost -> waiter.completeExceptionally(ConnectionClosedException("The connection to the machine dropped.", failure))
            waiter != null -> waiter.completeExceptionally(IOException(detail ?: "Claude Code exited without finishing the turn."))
            asked && (busy || owed) -> onActivity(Activity.Stopped(proc.sessionId, lost = false, detail = null))
            busy -> onActivity(Activity.Stopped(proc.sessionId, lost, if (lost) null else detail ?: "Claude Code exited."))
            owed -> onActivity(Activity.Idle(proc.sessionId))
        }
    }

    /** Closes a process on purpose; its reader reports the exit. With [onlyIfQuiet], not while anything runs. */
    private suspend fun shut(proc: Proc, onlyIfQuiet: Boolean = false) {
        synchronized(proc) {
            if (proc.closing || (onlyIfQuiet && proc.busy)) return
            proc.closing = true
        }
        procs.remove(proc.sessionId, proc)
        withContext(NonCancellable + Dispatchers.IO) { runCatching { proc.process.close() } }
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

    /** The interrupt control request, then the process closes; a turn that ignores it is cut off after the grace. */
    override suspend fun cancel(sessionId: String) {
        val proc = procs[sessionId] ?: return
        val turn = synchronized(proc) {
            proc.interrupted = true
            proc.turn
        }
        if (!turn) return shut(proc)
        runCatching { write(proc, ClaudeNormalizer.controlRequest("wizard-android-interrupt", "interrupt")) }
        scope.launch {
            delay(timing.interruptGraceMs)
            shut(proc)
        }
    }

    override fun close() {
        closed = true
        procs.values.toList().forEach { proc ->
            synchronized(proc) {
                proc.closing = true
                proc.quiet?.cancel()
            }
            runCatching { proc.process.close() }
        }
        procs.clear()
    }
}
