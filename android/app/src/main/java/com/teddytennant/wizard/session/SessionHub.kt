package com.teddytennant.wizard.session

import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.ConnectionClosedException
import com.teddytennant.wizard.acp.JsonRpcException
import com.teddytennant.wizard.acp.PermissionAnswer
import com.teddytennant.wizard.acp.PermissionRequest
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionNotification
import com.teddytennant.wizard.acp.SessionUpdate
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.agent.AgentAvailability
import com.teddytennant.wizard.claude.ClaudeBackend
import com.teddytennant.wizard.data.AuthKind
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.data.MachineStore
import com.teddytennant.wizard.data.RecentChat
import com.teddytennant.wizard.data.RecentStore
import com.teddytennant.wizard.data.SettingsStore
import com.teddytennant.wizard.notify.NotificationPolicy
import com.teddytennant.wizard.notify.NotificationText
import com.teddytennant.wizard.notify.RunningTurn
import com.teddytennant.wizard.notify.TurnOutcome
import com.teddytennant.wizard.ssh.HostKeyChangedException
import com.teddytennant.wizard.ssh.HostKeyUnknownException
import com.teddytennant.wizard.ssh.KeyVault
import com.teddytennant.wizard.ssh.KnownHost
import com.teddytennant.wizard.ssh.KnownHostsStore
import com.teddytennant.wizard.ssh.PresentedKey
import com.teddytennant.wizard.ssh.RemoteScripts
import com.teddytennant.wizard.ssh.SshAuth
import com.teddytennant.wizard.ssh.SshAuthException
import com.teddytennant.wizard.ssh.SshLink
import com.teddytennant.wizard.ssh.SshTarget
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import java.io.IOException
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.time.Instant
import java.util.concurrent.ConcurrentHashMap

enum class Reach { Unknown, Checking, Online, Offline, NeedsTrust, KeyChanged, AuthFailed }

data class MachineStatus(
    val reach: Reach = Reach.Unknown,
    val agents: Map<Agent, AgentAvailability> = emptyMap(),
    val running: Int = 0,
    val message: String? = null,
    val presented: PresentedKey? = null,
    val trusted: KnownHost? = null,
    val home: String? = null,
) {
    val wizardVersion: String? get() = agents[Agent.Wizard]?.version
    fun agent(agent: Agent): AgentAvailability = agents[agent] ?: AgentAvailability.Unknown
    val readyAgents: List<Agent> get() = Agent.entries.filter { agent(it).ready }
}

data class PendingPermission(val request: PermissionRequest)

data class ChatState(
    val machineId: String,
    val agent: Agent,
    val sessionId: String,
    val cwd: String,
    val title: String? = null,
    val items: List<TranscriptItem> = emptyList(),
    val running: Boolean = false,
    val loading: Boolean = false,
    val options: List<ConfigOption> = emptyList(),
    /** Option values the user picked, re-applied if the session is reloaded. */
    val chosen: Map<String, String> = emptyMap(),
    val error: String? = null,
    val permission: PendingPermission? = null,
    val acceptReplay: Boolean = false,
    /** Background tasks still running after the turn ended. */
    val background: Int = 0,
)

/** A saved session on a machine, for lists that mix agents. */
data class AgentSession(val agent: Agent, val info: SessionInfo)

class AgentMissingException(agent: Agent, machine: String) : IOException("${agent.displayName} isn't installed on $machine")

/** Hooks the hub needs from Android; a fake in tests. */
interface HubEnvironment {
    val appInForeground: Boolean
    fun startTurnService()
    fun permissionGranted(): Boolean
    fun postTurnEnded(turn: RunningTurn, text: NotificationText)
    fun postNeedsInput(turn: RunningTurn, text: NotificationText)
}

/**
 * Owns every SSH connection and agent process, and the state of every open
 * chat. Per machine: one SSH connection, one ACP process each for Wizard and
 * Pi serving all their chats, and a `claude -p` per open Claude Code chat.
 *
 * A turn stays in [running] after it returns while it has background tasks,
 * so the service and the connection stay up; the wake turns those tasks start
 * run in the chat like any other, and "finished" is posted once, when the
 * session has nothing left running.
 */
class SessionHub(
    private val machines: MachineStore,
    private val vault: KeyVault,
    private val knownHosts: KnownHostsStore,
    private val settings: SettingsStore,
    private val recents: RecentStore,
    private val env: HubEnvironment,
    private val scope: CoroutineScope,
    private val clientVersion: String,
) {
    private val _statuses = MutableStateFlow<Map<String, MachineStatus>>(emptyMap())
    val statuses: StateFlow<Map<String, MachineStatus>> = _statuses.asStateFlow()

    private val _running = MutableStateFlow<List<RunningTurn>>(emptyList())
    val running: StateFlow<List<RunningTurn>> = _running.asStateFlow()

    /** Key of the chat on screen, if any. */
    @Volatile var viewing: String? = null

    private class Live(val link: SshLink) {
        val backends = ConcurrentHashMap<Agent, AgentBackend>()
        val loaded: MutableSet<String> = ConcurrentHashMap.newKeySet()
    }

    private val lives = ConcurrentHashMap<String, Live>()
    private val locks = ConcurrentHashMap<String, Mutex>()
    private val chats = ConcurrentHashMap<String, MutableStateFlow<ChatState>>()
    private val permissionAnswers = ConcurrentHashMap<String, CompletableDeferred<String?>>()
    private val stoppedByUser: MutableSet<String> = ConcurrentHashMap.newKeySet()
    private var idleJob: Job? = null

    private fun lock(name: String) = locks.getOrPut(name) { Mutex() }

    fun status(machineId: String): MachineStatus = _statuses.value[machineId] ?: MachineStatus()

    private fun setStatus(machineId: String, change: (MachineStatus) -> MachineStatus) =
        _statuses.update { it + (machineId to change(it[machineId] ?: MachineStatus())) }

    // Connections

    private suspend fun machine(machineId: String): Machine =
        machines.get(machineId) ?: throw IOException("That machine was removed.")

    private suspend fun live(machine: Machine): Live = lock(machine.id).withLock {
        lives[machine.id]?.takeIf { it.link.isConnected }?.let { return it }
        lives.remove(machine.id)?.let { dispose(it) }
        setStatus(machine.id) { it.copy(reach = Reach.Checking, message = null) }
        val link = try {
            withContext(Dispatchers.IO) { SshLink.connect(SshTarget(machine.host, machine.port, machine.user), auth(machine), knownHosts.load()) }
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            setStatus(machine.id) { failedStatus(it, e) }
            throw e
        }
        val live = Live(link)
        lives[machine.id] = live
        probe(machine.id, live)
        live
    }

    private suspend fun probe(machineId: String, live: Live): RemoteScripts.Probe? {
        val probe = withContext(Dispatchers.IO) { runCatching { RemoteScripts.parseProbe(live.link.run(RemoteScripts.probe, 30).stdout) }.getOrNull() }
        setStatus(machineId) { MachineStatus(Reach.Online, probe?.agents.orEmpty(), probe?.running ?: 0, home = probe?.home) }
        return probe
    }

    private fun auth(machine: Machine): SshAuth = when (machine.auth) {
        AuthKind.Key -> SshAuth.Key(vault.material(machine.keyId ?: throw IOException("No key is set for ${machine.name}.")))
        AuthKind.Password -> SshAuth.Password(vault.secret(Machine.passwordSecret(machine.id)) ?: throw IOException("No password is saved for ${machine.name}."))
    }

    private fun failedStatus(previous: MachineStatus, e: Throwable): MachineStatus = when (e) {
        is HostKeyUnknownException -> previous.copy(reach = Reach.NeedsTrust, presented = e.presented, trusted = null, message = null)
        is HostKeyChangedException -> previous.copy(reach = Reach.KeyChanged, presented = e.presented, trusted = e.trusted, message = null)
        is SshAuthException -> previous.copy(reach = Reach.AuthFailed, message = e.message)
        else -> previous.copy(reach = Reach.Offline, message = describe(e))
    }

    private suspend fun backend(machine: Machine, agent: Agent): AgentBackend {
        val live = live(machine)
        return lock("${machine.id}#${agent.id}").withLock {
            live.backends[agent]?.takeIf { !it.isClosed }?.let { return it }
            live.loaded.removeAll { it.startsWith("${agent.id}/") }
            if (!status(machine.id).agent(agent).ready) {
                probe(machine.id, live)
                if (!status(machine.id).agent(agent).ready) throw AgentMissingException(agent, machine.name)
            }
            val onUpdate: UpdateSink = { onUpdate(machine.id, agent, it) }
            val onPermission: PermissionSink = { onPermission(machine, agent, it) }
            val backend = when (agent) {
                Agent.Wizard, Agent.Pi -> AcpBackend.start(agent, live.link, scope, clientVersion, onUpdate, onPermission)
                Agent.ClaudeCode -> ClaudeBackend(live.link, onUpdate, onPermission, { onActivity(machine, agent, it) }, scope)
            }
            live.backends[agent] = backend
            backend
        }
    }

    /** Connects if needed and reads the machine's state. Never throws. */
    suspend fun refresh(machineId: String) {
        val machine = machines.get(machineId) ?: return
        val existing = lives[machineId]
        if (existing != null && existing.link.isConnected && probe(machineId, existing) != null) return
        runCatching { withTimeout(25_000) { live(machine) } }.onFailure { e ->
            if (e is kotlinx.coroutines.TimeoutCancellationException) setStatus(machineId) { it.copy(reach = Reach.Offline, message = "${machine.host} didn't answer in time.") }
        }
    }

    suspend fun refreshAll() {
        val list = machines.machines.first()
        coroutineScope { list.map { m -> async { refresh(m.id) } }.awaitAll() }
    }

    /** Refreshes every machine, then pulls each ready agent's saved sessions into the recent list. */
    suspend fun refreshRecents() {
        refreshAll()
        val list = machines.machines.first()
        coroutineScope {
            list.filter { status(it.id).reach == Reach.Online }.map { m ->
                async {
                    val found = runCatching { sessions(m.id) }.getOrDefault(emptyList())
                    recents.upsert(found.map { recentOf(m.id, it) })
                }
            }.awaitAll()
        }
    }

    private fun recentOf(machineId: String, s: AgentSession) = RecentChat(
        machineId = machineId,
        agentId = s.agent.id,
        sessionId = s.info.sessionId,
        cwd = s.info.cwd,
        title = s.info.title?.takeIf { it.isNotBlank() && it != "(no prompt)" },
        updatedAt = parseTime(s.info.updatedAt) ?: 0,
    )

    /** Trusts [presented] for its host (replacing any old key) and connects again. */
    suspend fun trustHostKey(machineId: String, presented: PresentedKey) {
        withContext(Dispatchers.IO) { knownHosts.update { it.forget(presented.host, presented.port).trust(presented) } }
        setStatus(machineId) { it.copy(reach = Reach.Unknown, presented = null, trusted = null) }
        refresh(machineId)
    }

    /** Drops the connection (after an edit, or on delete). */
    fun disconnect(machineId: String) {
        lives.remove(machineId)?.let { dispose(it) }
        _statuses.update { it - machineId }
    }

    private fun dispose(live: Live) {
        live.backends.values.forEach { runCatching { it.close() } }
        runCatching { live.link.close() }
    }

    // Machine overview

    /** Saved sessions of every ready agent on the machine, newest first. */
    suspend fun sessions(machineId: String): List<AgentSession> {
        val machine = machine(machineId)
        live(machine)
        return coroutineScope {
            status(machineId).readyAgents.map { agent ->
                async {
                    runCatching { backend(machine, agent).listSessions(null).map { AgentSession(agent, it) } }.getOrDefault(emptyList())
                }
            }.awaitAll().flatten().sortedByDescending { parseTime(it.info.updatedAt) ?: parseTime(it.info.sessionId) ?: 0 }
        }
    }

    suspend fun listDirs(machineId: String, path: String): RemoteScripts.Listing {
        val machine = machine(machineId)
        val live = live(machine)
        val result = withContext(Dispatchers.IO) { live.link.run(RemoteScripts.listDirs(path), 20) }
        if (result.exitStatus != 0) throw IOException(result.stderr.trim().ifEmpty { "Couldn't open $path" })
        return RemoteScripts.parseListing(result.stdout)
    }

    /** Runs an agent's installer on the machine, handing over each line of output. Returns true on success. */
    suspend fun install(machineId: String, agent: Agent, onLine: (String) -> Unit): Boolean {
        val machine = machine(machineId)
        val live = live(machine)
        val status = withContext(Dispatchers.IO) { live.link.stream(RemoteScripts.install(agent), onLine) }
        live.backends.remove(agent)?.close()
        probe(machineId, live)
        return status == 0 && status(machineId).agent(agent).ready
    }

    // Chats

    fun chatKey(machineId: String, agent: Agent, sessionId: String) = "$machineId/${agent.id}/$sessionId"

    fun chat(machineId: String, agent: Agent, sessionId: String): StateFlow<ChatState>? = chats[chatKey(machineId, agent, sessionId)]

    /** Opens a new session in [cwd] and returns its id. */
    suspend fun startChat(machineId: String, agent: Agent, cwd: String): String {
        val machine = machine(machineId)
        val backend = backend(machine, agent)
        val opened = backend.newSession(cwd)
        lives[machineId]?.loaded?.add("${agent.id}/${opened.sessionId}")
        chats[chatKey(machineId, agent, opened.sessionId)] = MutableStateFlow(
            ChatState(machineId, agent, opened.sessionId, cwd, options = opened.configOptions),
        )
        machines.update(machineId) { it.withRecentDir(cwd) }
        return opened.sessionId
    }

    /** The state for a saved session, created empty if this app hasn't seen it. Call [openChat] to fill it. */
    fun prepareChat(machineId: String, agent: Agent, sessionId: String, cwd: String, title: String?): StateFlow<ChatState> =
        chats.getOrPut(chatKey(machineId, agent, sessionId)) {
            MutableStateFlow(ChatState(machineId, agent, sessionId, cwd, title = title, loading = true))
        }

    /** Opens a saved session, replaying its transcript if this app hasn't shown it yet. */
    suspend fun openChat(machineId: String, agent: Agent, sessionId: String, cwd: String, title: String?): StateFlow<ChatState> {
        prepareChat(machineId, agent, sessionId, cwd, title)
        val flow = chats.getValue(chatKey(machineId, agent, sessionId))
        try {
            ensureLoaded(flow)
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            flow.update { it.copy(loading = false, error = describe(e)) }
        }
        return flow
    }

    private suspend fun ensureLoaded(flow: MutableStateFlow<ChatState>): AgentBackend {
        val state = flow.value
        val machine = machine(state.machineId)
        val backend = backend(machine, state.agent)
        val live = lives[state.machineId] ?: throw IOException("The connection dropped.")
        val loadedKey = "${state.agent.id}/${state.sessionId}"
        if (loadedKey in live.loaded) {
            flow.update { it.copy(loading = false) }
            return backend
        }
        flow.update { it.copy(acceptReplay = it.items.isEmpty(), loading = it.items.isEmpty(), error = null) }
        try {
            val opened = backend.loadSession(state.sessionId, state.cwd)
            var options = opened.configOptions
            for ((id, value) in flow.value.chosen) {
                if (options.firstOrNull { it.id == id }?.currentValue != value) {
                    options = runCatching { backend.setOption(state.sessionId, id, value) }.getOrDefault(options)
                }
            }
            live.loaded += loadedKey
            flow.update { it.copy(options = options, loading = false) }
        } finally {
            flow.update { it.copy(acceptReplay = false) }
        }
        return backend
    }

    private fun onUpdate(machineId: String, agent: Agent, note: SessionNotification) {
        val flow = chats[chatKey(machineId, agent, note.sessionId)] ?: return
        flow.update { state ->
            when {
                note.isReplay && !state.acceptReplay -> state
                note.update is SessionUpdate.ConfigOptionsChanged -> state.copy(options = note.update.options)
                // Outside a turn, only a replay belongs in the transcript: Pi greets a new session with a banner.
                !note.isReplay && !state.running -> state
                else -> {
                    val items = Transcript.apply(state.items, note.update)
                    val title = state.title ?: (note.update as? SessionUpdate.UserText)?.text?.let(::titleFrom)
                    state.copy(items = items, title = title)
                }
            }
        }
    }

    fun send(machineId: String, agent: Agent, sessionId: String, text: String) {
        val key = chatKey(machineId, agent, sessionId)
        val flow = chats[key] ?: return
        if (flow.value.running || text.isBlank()) return
        flow.update {
            it.copy(items = Transcript.userMessage(it.items, text), running = true, error = null, title = it.title ?: titleFrom(text))
        }
        stoppedByUser.remove(key)
        scope.launch {
            val machine = machines.get(machineId)
            val turn = RunningTurn(machineId, machine?.name ?: "your machine", sessionId, flow.value.title, flow.value.cwd, agent)
            _running.update { list -> list.filterNot { it.machineId == machineId && it.sessionId == sessionId } + turn }
            remember(flow.value)
            env.startTurnService()
            var stopReason: String? = null
            var error: String? = null
            var backend: AgentBackend? = null
            try {
                backend = ensureLoaded(flow)
                stopReason = backend.prompt(sessionId, flow.value.cwd, text)
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                error = if (e is ConnectionClosedException) lostMessage(turn.machineName) else describe(e)
                if (e is ConnectionClosedException) lives[machineId]?.backends?.remove(agent)
            }
            val outcome = NotificationPolicy.outcome(stopReason, failed = error != null)
            val tasks = if (outcome == TurnOutcome.Finished) backend?.backgroundTasks(sessionId) ?: 0 else 0
            flow.update { state ->
                var items = state.items
                if (outcome != TurnOutcome.Finished) items = Transcript.settle(items)
                when (outcome) {
                    TurnOutcome.Cancelled -> items = Transcript.notice(items, "Stopped")
                    TurnOutcome.TurnLimit -> items = Transcript.notice(items, "Stopped at the turn limit")
                    TurnOutcome.Failed -> items = Transcript.notice(items, error ?: "The turn failed", isError = true)
                    TurnOutcome.Finished -> Unit
                }
                // A background task's question can still be waiting on the user.
                state.copy(items = items, running = false, background = tasks, permission = state.permission.takeIf { tasks > 0 })
            }
            if (tasks > 0) {
                // Background tasks keep the turn: it ends when the session is idle.
                hold(machineId, sessionId) { it.copy(title = flow.value.title, tasks = tasks, waiting = true) }
                remember(flow.value)
                return@launch
            }
            endTurn(key, flow, outcome, error)
        }
    }

    /** Lets the turn go and says so, if nobody is looking. */
    private suspend fun endTurn(key: String, flow: MutableStateFlow<ChatState>, outcome: TurnOutcome, error: String?) {
        val state = flow.value
        val turn = _running.value.firstOrNull { it.machineId == state.machineId && it.sessionId == state.sessionId }
            ?: RunningTurn(state.machineId, machines.get(state.machineId)?.name ?: "your machine", state.sessionId, state.title, state.cwd, state.agent)
        _running.update { list -> list.filterNot { it.machineId == state.machineId && it.sessionId == state.sessionId } }
        remember(state)
        if (NotificationPolicy.notifyTurnEnd(outcome, key in stoppedByUser, situation(key))) {
            env.postTurnEnded(
                turn.copy(title = state.title),
                NotificationPolicy.turnEnded(turn.machineName, outcome, Transcript.lastReplyFirstLine(state.items), error, state.agent.displayName),
            )
        }
        if (!env.appInForeground) scheduleIdleClose()
    }

    private fun hold(machineId: String, sessionId: String, change: (RunningTurn) -> RunningTurn) =
        _running.update { list -> list.map { if (it.machineId == machineId && it.sessionId == sessionId) change(it) else it } }

    private fun holding(machineId: String, sessionId: String) = _running.value.any { it.machineId == machineId && it.sessionId == sessionId }

    /** Background tasks and the wake turns they start, after [AgentBackend.prompt] returned. */
    private suspend fun onActivity(machine: Machine, agent: Agent, activity: Activity) {
        val key = chatKey(machine.id, agent, activity.sessionId)
        val flow = chats[key] ?: return
        val id = activity.sessionId
        when (activity) {
            is Activity.Tasks -> {
                flow.update { it.copy(background = activity.count) }
                hold(machine.id, id) { it.copy(tasks = activity.count) }
            }
            is Activity.WakeStarted -> {
                // The notice keeps the wake turn's reply apart from the last one.
                flow.update { it.copy(items = Transcript.notice(it.items, "A background task finished"), running = true, error = null) }
                if (holding(machine.id, id)) {
                    hold(machine.id, id) { it.copy(waiting = false) }
                } else {
                    val state = flow.value
                    _running.update { it + RunningTurn(machine.id, machine.name, id, state.title, state.cwd, agent, tasks = state.background) }
                    // Android can refuse a foreground service started from the background.
                    runCatching { env.startTurnService() }
                }
            }
            is Activity.WakeEnded -> {
                flow.update { state ->
                    val items = activity.error?.let { Transcript.notice(Transcript.settle(state.items), it, isError = true) } ?: state.items
                    state.copy(items = items, running = false)
                }
                hold(machine.id, id) { it.copy(waiting = true) }
            }
            is Activity.Idle -> {
                if (!holding(machine.id, id) || flow.value.running) return
                flow.update { it.copy(background = 0) }
                endTurn(key, flow, TurnOutcome.Finished, null)
            }
            is Activity.Stopped -> {
                if (!holding(machine.id, id)) return
                val error = if (activity.lost) lostMessage(machine.name) else activity.detail
                flow.update { state ->
                    val items = Transcript.notice(Transcript.settle(state.items), error ?: "Stopped", isError = error != null)
                    state.copy(items = items, running = false, background = 0)
                }
                endTurn(key, flow, if (error == null) TurnOutcome.Cancelled else TurnOutcome.Failed, error)
            }
        }
    }

    /** Puts a chat at the top of the recent list. */
    fun remember(state: ChatState) {
        scope.launch {
            recents.upsert(listOf(RecentChat(state.machineId, state.agent.id, state.sessionId, state.cwd, state.title, System.currentTimeMillis())))
        }
    }

    fun isRunning(machineId: String, agent: Agent, sessionId: String) =
        chats[chatKey(machineId, agent, sessionId)]?.value?.running == true

    fun stop(machineId: String, agent: Agent, sessionId: String) {
        val key = chatKey(machineId, agent, sessionId)
        stoppedByUser += key
        permissionAnswers.remove(key)?.complete(null)
        val backend = lives[machineId]?.backends?.get(agent) ?: return
        scope.launch { runCatching { backend.cancel(sessionId) } }
    }

    fun stopAll() = _running.value.forEach { stop(it.machineId, it.agent, it.sessionId) }

    suspend fun setOption(machineId: String, agent: Agent, sessionId: String, configId: String, value: String) {
        val flow = chats[chatKey(machineId, agent, sessionId)] ?: return
        if (flow.value.running) return
        flow.update { it.copy(chosen = it.chosen + (configId to value)) }
        try {
            val backend = ensureLoaded(flow)
            val options = backend.setOption(sessionId, configId, value)
            flow.update { it.copy(options = options, error = null) }
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            flow.update { it.copy(error = describe(e)) }
        }
    }

    fun answerPermission(machineId: String, agent: Agent, sessionId: String, optionId: String?) {
        permissionAnswers.remove(chatKey(machineId, agent, sessionId))?.complete(optionId)
    }

    private suspend fun onPermission(machine: Machine, agent: Agent, request: PermissionRequest): PermissionAnswer {
        val key = chatKey(machine.id, agent, request.sessionId)
        val flow = chats[key] ?: return PermissionAnswer(null)
        val answer = CompletableDeferred<String?>()
        permissionAnswers[key] = answer
        flow.update { it.copy(permission = PendingPermission(request)) }
        if (NotificationPolicy.notifyNeedsInput(situation(key))) {
            val state = flow.value
            env.postNeedsInput(
                RunningTurn(machine.id, machine.name, request.sessionId, state.title, state.cwd, agent),
                NotificationPolicy.needsInput(machine.name, request.title, agent.displayName),
            )
        }
        val choice = try {
            answer.await()
        } finally {
            flow.update { it.copy(permission = null) }
        }
        return PermissionAnswer(choice)
    }

    private suspend fun situation(key: String) = NotificationPolicy.Situation(
        appInForeground = env.appInForeground,
        viewingChat = viewing == key,
        settings = settings.current(),
        permissionGranted = env.permissionGranted(),
    )

    // Lifecycle

    fun onForeground() {
        idleJob?.cancel()
        idleJob = null
    }

    fun onBackground() = scheduleIdleClose()

    /** Nothing running and nobody looking: let the machines go after a minute. */
    private fun scheduleIdleClose() {
        idleJob?.cancel()
        idleJob = scope.launch {
            delay(60_000)
            if (!env.appInForeground && _running.value.isEmpty()) {
                lives.keys.toList().forEach { id -> lives.remove(id)?.let { dispose(it) } }
            }
        }
    }

    companion object {
        fun lostMessage(machine: String) = "Lost the connection to $machine. Send again to reconnect and pick up where the session left off."

        fun titleFrom(text: String): String {
            val line = text.lineSequence().firstOrNull { it.isNotBlank() }?.trim().orEmpty()
            return if (line.length <= 80) line else line.take(79).trimEnd() + "…"
        }

        /** An RFC 3339 time, or a Wizard session id (`2026-08-31T15-39-52`, local time), as epoch millis. */
        fun parseTime(value: String?): Long? {
            if (value.isNullOrBlank()) return null
            runCatching { return java.time.OffsetDateTime.parse(value).toInstant().toEpochMilli() }
            runCatching { return Instant.parse(value).toEpochMilli() }
            runCatching {
                return java.time.LocalDateTime.parse(value, java.time.format.DateTimeFormatter.ofPattern("yyyy-MM-dd'T'HH-mm-ss"))
                    .atZone(java.time.ZoneId.systemDefault()).toInstant().toEpochMilli()
            }
            return null
        }

        fun describe(e: Throwable): String = when (e) {
            is UnknownHostException -> "Can't find ${e.message ?: "that host"}."
            is ConnectException, is NoRouteToHostException -> "Nothing answered. Check the host, port and network."
            is SocketTimeoutException -> "The machine didn't answer in time."
            is HostKeyUnknownException -> "The host key needs confirming."
            is HostKeyChangedException -> "The host key changed."
            is ConnectionClosedException -> "The connection to the machine dropped."
            is JsonRpcException -> e.message ?: "The agent returned an error."
            is kotlinx.coroutines.TimeoutCancellationException -> "The machine didn't answer in time."
            else -> e.message?.takeIf { it.isNotBlank() } ?: e.javaClass.simpleName
        }
    }
}
