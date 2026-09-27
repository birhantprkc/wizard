package com.teddytennant.wizard.session

import com.teddytennant.wizard.acp.AcpClient
import com.teddytennant.wizard.acp.AgentInfo
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.acp.ConnectionClosedException
import com.teddytennant.wizard.acp.JsonRpcException
import com.teddytennant.wizard.acp.PermissionAnswer
import com.teddytennant.wizard.acp.PermissionRequest
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.acp.SessionNotification
import com.teddytennant.wizard.acp.SessionUpdate
import com.teddytennant.wizard.data.AuthKind
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.data.MachineStore
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
import java.util.concurrent.ConcurrentHashMap

enum class Reach { Unknown, Checking, Online, Offline, NeedsTrust, KeyChanged, AuthFailed }

data class MachineStatus(
    val reach: Reach = Reach.Unknown,
    val wizardVersion: String? = null,
    val running: Int = 0,
    val message: String? = null,
    val presented: PresentedKey? = null,
    val trusted: KnownHost? = null,
    val home: String? = null,
) {
    val wizardMissing: Boolean get() = reach == Reach.Online && wizardVersion == null
}

data class PendingPermission(val request: PermissionRequest)

data class ChatState(
    val machineId: String,
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
)

/** Something to tell the user about while they're in the app. */
class WizardMissingException(machine: String) : IOException("Wizard isn't installed on $machine")

/** Hooks the hub needs from Android; a fake in tests. */
interface HubEnvironment {
    val appInForeground: Boolean
    fun startTurnService()
    fun permissionGranted(): Boolean
    fun postTurnEnded(turn: RunningTurn, text: NotificationText)
    fun postNeedsInput(turn: RunningTurn, text: NotificationText)
}

/**
 * Owns every SSH connection and `wizard acp` process, and the state of every
 * open chat. One `wizard acp` per machine serves all of that machine's chats.
 */
class SessionHub(
    private val machines: MachineStore,
    private val vault: KeyVault,
    private val knownHosts: KnownHostsStore,
    private val settings: SettingsStore,
    private val env: HubEnvironment,
    private val scope: CoroutineScope,
    private val clientVersion: String,
) {
    private val _statuses = MutableStateFlow<Map<String, MachineStatus>>(emptyMap())
    val statuses: StateFlow<Map<String, MachineStatus>> = _statuses.asStateFlow()

    private val _running = MutableStateFlow<List<RunningTurn>>(emptyList())
    val running: StateFlow<List<RunningTurn>> = _running.asStateFlow()

    /** `machineId/sessionId` of the chat on screen, if any. */
    @Volatile var viewing: String? = null

    private class Live(val link: SshLink) {
        var acp: AcpClient? = null
        var agent: AgentInfo? = null
        var wizardVersion: String? = null
        val loaded = ConcurrentHashMap.newKeySet<String>()
    }

    private val lives = ConcurrentHashMap<String, Live>()
    private val locks = ConcurrentHashMap<String, Mutex>()
    private val chats = ConcurrentHashMap<String, MutableStateFlow<ChatState>>()
    private val permissionAnswers = ConcurrentHashMap<String, CompletableDeferred<String?>>()
    private val stoppedByUser = ConcurrentHashMap.newKeySet<String>()
    private var idleJob: Job? = null

    private fun key(machineId: String, sessionId: String) = "$machineId/$sessionId"
    private fun lock(machineId: String) = locks.getOrPut(machineId) { Mutex() }

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
        val probe = withContext(Dispatchers.IO) { runCatching { RemoteScripts.parseProbe(link.exec(RemoteScripts.probe, 15).stdout) }.getOrNull() }
        live.wizardVersion = probe?.wizardVersion
        setStatus(machine.id) {
            MachineStatus(Reach.Online, probe?.wizardVersion, probe?.running ?: 0, home = probe?.home)
        }
        lives[machine.id] = live
        live
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

    private suspend fun acp(machine: Machine, live: Live): AcpClient = lock(machine.id + "#acp").withLock {
        live.acp?.takeIf { !it.closed.isCompleted }?.let { return it }
        live.loaded.clear()
        if (live.wizardVersion == null) {
            val probe = withContext(Dispatchers.IO) { RemoteScripts.parseProbe(live.link.exec(RemoteScripts.probe, 15).stdout) }
            live.wizardVersion = probe.wizardVersion
            setStatus(machine.id) { it.copy(wizardVersion = probe.wizardVersion, running = probe.running) }
            if (probe.wizardVersion == null) throw WizardMissingException(machine.name)
        }
        val transport = withContext(Dispatchers.IO) { live.link.openAcp() }
        val client = AcpClient(
            transport = transport,
            scope = scope,
            onUpdate = { onUpdate(machine.id, it) },
            onPermission = { onPermission(machine, it) },
        )
        try {
            live.agent = withTimeout(30_000) { client.initialize(clientVersion) }
        } catch (e: Exception) {
            client.close()
            throw if (e is CancellationException) e else IOException("wizard acp didn't start on ${machine.name}: ${describe(e)}", e)
        }
        live.acp = client
        client
    }

    /** Connects if needed and reads the machine's state. Never throws. */
    suspend fun refresh(machineId: String) {
        val machine = machines.get(machineId) ?: return
        val existing = lives[machineId]
        if (existing != null && existing.link.isConnected) {
            val probe = withContext(Dispatchers.IO) { runCatching { RemoteScripts.parseProbe(existing.link.exec(RemoteScripts.probe, 15).stdout) }.getOrNull() }
            if (probe != null) {
                existing.wizardVersion = probe.wizardVersion
                setStatus(machineId) { MachineStatus(Reach.Online, probe.wizardVersion, probe.running, home = probe.home) }
                return
            }
        }
        runCatching { withTimeout(20_000) { live(machine) } }.onFailure { e ->
            if (e is kotlinx.coroutines.TimeoutCancellationException) setStatus(machineId) { it.copy(reach = Reach.Offline, message = "${machine.host} didn't answer in time.") }
        }
    }

    suspend fun refreshAll() {
        val list = machines.machines.first()
        coroutineScopeAll(list.map { m -> suspend { refresh(m.id) } })
    }

    private suspend fun coroutineScopeAll(jobs: List<suspend () -> Unit>) =
        kotlinx.coroutines.coroutineScope { jobs.map { async { it() } }.awaitAll() }

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
        live.acp?.let { runCatching { it.close() } }
        runCatching { live.link.close() }
    }

    // Machine overview

    suspend fun sessions(machineId: String, cwd: String? = null, pages: Int = 3): List<SessionInfo> {
        val machine = machine(machineId)
        val client = acp(machine, live(machine))
        val all = mutableListOf<SessionInfo>()
        var cursor: String? = null
        repeat(pages) {
            val page = client.listSessions(cwd, cursor)
            all += page.sessions
            cursor = page.nextCursor ?: return all
        }
        return all
    }

    suspend fun listDirs(machineId: String, path: String): RemoteScripts.Listing {
        val machine = machine(machineId)
        val live = live(machine)
        val result = withContext(Dispatchers.IO) { live.link.exec(RemoteScripts.listDirs(path), 20) }
        if (result.exitStatus != 0) throw IOException(result.stderr.trim().ifEmpty { "Couldn't open $path" })
        return RemoteScripts.parseListing(result.stdout)
    }

    /** Runs the Wizard installer on the machine, handing over each line of output. Returns true on success. */
    suspend fun install(machineId: String, onLine: (String) -> Unit): Boolean {
        val machine = machine(machineId)
        val live = live(machine)
        val status = withContext(Dispatchers.IO) { live.link.stream(RemoteScripts.install, onLine) }
        live.wizardVersion = null
        refresh(machineId)
        return status == 0 && status(machineId).wizardVersion != null
    }

    // Chats

    fun chat(machineId: String, sessionId: String): StateFlow<ChatState>? = chats[key(machineId, sessionId)]

    /** Opens a new session in [cwd] and returns its id. */
    suspend fun startChat(machineId: String, cwd: String): String {
        val machine = machine(machineId)
        val live = live(machine)
        val client = acp(machine, live)
        val opened = client.newSession(cwd)
        live.loaded += opened.sessionId
        chats[key(machineId, opened.sessionId)] = MutableStateFlow(
            ChatState(machineId, opened.sessionId, cwd, options = opened.configOptions),
        )
        machines.update(machineId) { it.withRecentDir(cwd) }
        return opened.sessionId
    }

    /** The state for a saved session, created empty if this app hasn't seen it. Call [openChat] to fill it. */
    fun prepareChat(machineId: String, sessionId: String, cwd: String, title: String?): StateFlow<ChatState> =
        chats.getOrPut(key(machineId, sessionId)) {
            MutableStateFlow(ChatState(machineId, sessionId, cwd, title = title, loading = true))
        }

    /** Opens a saved session, replaying its transcript if this app hasn't shown it yet. */
    suspend fun openChat(machineId: String, sessionId: String, cwd: String, title: String?): StateFlow<ChatState> {
        prepareChat(machineId, sessionId, cwd, title)
        val flow = chats.getValue(key(machineId, sessionId))
        try {
            ensureLoaded(machineId, sessionId, flow)
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            flow.update { it.copy(loading = false, error = describe(e)) }
        }
        return flow
    }

    private suspend fun ensureLoaded(machineId: String, sessionId: String, flow: MutableStateFlow<ChatState>): AcpClient {
        val machine = machine(machineId)
        val live = live(machine)
        val client = acp(machine, live)
        if (sessionId in live.loaded) {
            flow.update { it.copy(loading = false) }
            return client
        }
        flow.update { it.copy(acceptReplay = it.items.isEmpty(), loading = it.items.isEmpty(), error = null) }
        try {
            val opened = client.loadSession(sessionId, flow.value.cwd)
            var options = opened.configOptions
            for ((id, value) in flow.value.chosen) {
                if (options.firstOrNull { it.id == id }?.currentValue != value) {
                    options = runCatching { client.setConfigOption(sessionId, id, value) }.getOrDefault(options)
                }
            }
            live.loaded += sessionId
            flow.update { it.copy(options = options, loading = false) }
        } finally {
            flow.update { it.copy(acceptReplay = false) }
        }
        return client
    }

    private fun onUpdate(machineId: String, note: SessionNotification) {
        val flow = chats[key(machineId, note.sessionId)] ?: return
        flow.update { state ->
            when {
                note.isReplay && !state.acceptReplay -> state
                note.update is SessionUpdate.ConfigOptionsChanged -> state.copy(options = note.update.options)
                else -> {
                    val items = Transcript.apply(state.items, note.update)
                    val title = state.title ?: (note.update as? SessionUpdate.UserText)?.text?.let(::titleFrom)
                    state.copy(items = items, title = title)
                }
            }
        }
    }

    fun send(machineId: String, sessionId: String, text: String) {
        val chatKey = key(machineId, sessionId)
        val flow = chats[chatKey] ?: return
        if (flow.value.running || text.isBlank()) return
        flow.update {
            it.copy(items = Transcript.userMessage(it.items, text), running = true, error = null, title = it.title ?: titleFrom(text))
        }
        stoppedByUser.remove(chatKey)
        scope.launch {
            val machine = machines.get(machineId)
            val turn = RunningTurn(machineId, machine?.name ?: "your machine", sessionId, flow.value.title, flow.value.cwd)
            _running.update { it + turn }
            env.startTurnService()
            var stopReason: String? = null
            var error: String? = null
            try {
                val client = ensureLoaded(machineId, sessionId, flow)
                stopReason = client.prompt(sessionId, text)
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                error = describe(e)
                if (e is ConnectionClosedException) lives[machineId]?.let { it.acp = null }
            }
            val outcome = NotificationPolicy.outcome(stopReason, failed = error != null)
            flow.update { state ->
                var items = state.items
                if (outcome != TurnOutcome.Finished) items = Transcript.settle(items)
                when (outcome) {
                    TurnOutcome.Cancelled -> items = Transcript.notice(items, "Stopped")
                    TurnOutcome.TurnLimit -> items = Transcript.notice(items, "Stopped at the turn limit")
                    TurnOutcome.Failed -> items = Transcript.notice(items, error ?: "The turn failed", isError = true)
                    TurnOutcome.Finished -> Unit
                }
                state.copy(items = items, running = false, permission = null)
            }
            _running.update { list -> list.filterNot { it.machineId == machineId && it.sessionId == sessionId } }
            val situation = situation(chatKey)
            if (NotificationPolicy.notifyTurnEnd(outcome, chatKey in stoppedByUser, situation)) {
                env.postTurnEnded(
                    turn.copy(title = flow.value.title),
                    NotificationPolicy.turnEnded(turn.machineName, outcome, Transcript.lastReplyFirstLine(flow.value.items), error),
                )
            }
            if (!env.appInForeground) scheduleIdleClose()
        }
    }

    fun stop(machineId: String, sessionId: String) {
        val chatKey = key(machineId, sessionId)
        stoppedByUser += chatKey
        permissionAnswers.remove(chatKey)?.complete(null)
        val client = lives[machineId]?.acp ?: return
        scope.launch { runCatching { client.cancel(sessionId) } }
    }

    fun stopAll() = _running.value.forEach { stop(it.machineId, it.sessionId) }

    suspend fun setOption(machineId: String, sessionId: String, configId: String, value: String) {
        val flow = chats[key(machineId, sessionId)] ?: return
        if (flow.value.running) return
        flow.update { it.copy(chosen = it.chosen + (configId to value)) }
        try {
            val client = ensureLoaded(machineId, sessionId, flow)
            val options = client.setConfigOption(sessionId, configId, value)
            flow.update { it.copy(options = options, error = null) }
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            flow.update { it.copy(error = describe(e)) }
        }
    }

    fun answerPermission(machineId: String, sessionId: String, optionId: String?) {
        permissionAnswers.remove(key(machineId, sessionId))?.complete(optionId)
    }

    private suspend fun onPermission(machine: Machine, request: PermissionRequest): PermissionAnswer {
        val chatKey = key(machine.id, request.sessionId)
        val flow = chats[chatKey] ?: return PermissionAnswer(null)
        val answer = CompletableDeferred<String?>()
        permissionAnswers[chatKey] = answer
        flow.update { it.copy(permission = PendingPermission(request)) }
        if (NotificationPolicy.notifyNeedsInput(situation(chatKey))) {
            val state = flow.value
            env.postNeedsInput(
                RunningTurn(machine.id, machine.name, request.sessionId, state.title, state.cwd),
                NotificationPolicy.needsInput(machine.name, request.title),
            )
        }
        val choice = try {
            answer.await()
        } finally {
            flow.update { it.copy(permission = null) }
        }
        return PermissionAnswer(choice)
    }

    private suspend fun situation(chatKey: String) = NotificationPolicy.Situation(
        appInForeground = env.appInForeground,
        viewingChat = viewing == chatKey,
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
        fun titleFrom(text: String): String {
            val line = text.lineSequence().firstOrNull { it.isNotBlank() }?.trim().orEmpty()
            return if (line.length <= 80) line else line.take(79).trimEnd() + "…"
        }

        fun describe(e: Throwable): String = when (e) {
            is UnknownHostException -> "Can't find ${e.message ?: "that host"}."
            is ConnectException, is NoRouteToHostException -> "Nothing answered. Check the host, port and network."
            is SocketTimeoutException -> "The machine didn't answer in time."
            is HostKeyUnknownException -> "The host key needs confirming."
            is HostKeyChangedException -> "The host key changed."
            is ConnectionClosedException -> "The connection to the machine dropped."
            is JsonRpcException -> e.message ?: "Wizard returned an error."
            is kotlinx.coroutines.TimeoutCancellationException -> "The machine didn't answer in time."
            else -> e.message?.takeIf { it.isNotBlank() } ?: e.javaClass.simpleName
        }
    }
}
