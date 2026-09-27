package com.teddytennant.wizard.ui

import android.os.Build
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.teddytennant.wizard.AppGraph
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.session.AgentSession
import com.teddytennant.wizard.data.AuthKind
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.data.MachineForm
import com.teddytennant.wizard.data.RecentChat
import com.teddytennant.wizard.session.ChatState
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.session.Reach
import com.teddytennant.wizard.session.SessionHub
import com.teddytennant.wizard.ssh.KeyImportException
import com.teddytennant.wizard.ssh.PresentedKey
import com.teddytennant.wizard.ssh.StoredKey
import com.teddytennant.wizard.ui.screens.BrowseState
import com.teddytennant.wizard.ui.screens.InstallState
import com.teddytennant.wizard.ui.screens.MachineCardModel
import com.teddytennant.wizard.ui.screens.MachineFormState
import com.teddytennant.wizard.ui.screens.OnboardingMachine
import com.teddytennant.wizard.ui.screens.OnboardingStep
import com.teddytennant.wizard.ui.screens.HomeState
import com.teddytennant.wizard.ui.screens.RecentRow
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.util.UUID

class MachinesViewModel(private val graph: AppGraph) : ViewModel() {
    val cards: StateFlow<List<MachineCardModel>?> = combine(graph.machines.machines, graph.hub.statuses) { machines, statuses ->
        machines.map { MachineCardModel(it, statuses[it.id] ?: MachineStatus()) }
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), null)

    val refreshing = MutableStateFlow(false)

    fun refresh(showSpinner: Boolean = true) {
        if (refreshing.value) return
        viewModelScope.launch {
            if (showSpinner) refreshing.value = true
            try {
                graph.hub.refreshAll()
            } finally {
                refreshing.value = false
            }
        }
    }

    fun delete(id: String) = viewModelScope.launch {
        graph.hub.disconnect(id)
        withContext(Dispatchers.IO) { graph.vault.deleteSecret(Machine.passwordSecret(id)) }
        graph.machines.delete(id)
        graph.recents.removeMachine(id)
    }
}

class KeysViewModel(private val graph: AppGraph) : ViewModel() {
    val keys = MutableStateFlow<List<StoredKey>>(emptyList())
    val importError = MutableStateFlow<String?>(null)
    val importing = MutableStateFlow(false)

    init {
        reload()
    }

    fun reload() = viewModelScope.launch { keys.value = withContext(Dispatchers.IO) { graph.vault.list() } }

    suspend fun generate(): StoredKey {
        val key = withContext(Dispatchers.Default) { graph.vault.generate(defaultKeyLabel()) }
        reload()
        return key
    }

    /** Returns the new key, or null with [importError] set. */
    suspend fun import(label: String, text: String, passphrase: String): StoredKey? {
        importing.value = true
        importError.value = null
        return try {
            withContext(Dispatchers.Default) { graph.vault.import(label.ifBlank { "Imported key" }, text, passphrase.ifEmpty { null }) }.also { reload() }
        } catch (e: KeyImportException) {
            importError.value = e.message
            null
        } finally {
            importing.value = false
        }
    }

    fun delete(key: StoredKey) = viewModelScope.launch {
        withContext(Dispatchers.IO) { graph.vault.delete(key.id) }
        reload()
    }

    companion object {
        fun defaultKeyLabel() = "wizard@" + Build.MODEL.lowercase().replace(Regex("[^a-z0-9]+"), "-").trim('-').ifEmpty { "android" }
    }
}

class EditMachineViewModel(private val graph: AppGraph, private val machineId: String?) : ViewModel() {
    val form = MutableStateFlow(MachineFormState())
    private var original: Machine? = null

    init {
        viewModelScope.launch {
            val keys = withContext(Dispatchers.IO) { graph.vault.list() }
            val machine = machineId?.let { graph.machines.get(it) }
            original = machine
            form.value = if (machine != null) {
                MachineFormState(
                    name = machine.name,
                    host = machine.host,
                    port = machine.port.toString(),
                    user = machine.user,
                    auth = machine.auth,
                    keyId = machine.keyId ?: keys.firstOrNull()?.id,
                    hasSavedPassword = withContext(Dispatchers.IO) { graph.vault.secret(Machine.passwordSecret(machine.id)) != null },
                )
            } else {
                MachineFormState(keyId = keys.lastOrNull()?.id)
            }
        }
    }

    fun update(next: MachineFormState) {
        // Re-check fields the user has already been told about, so errors clear as they fix them.
        val errors = if (form.value.errors.ok) next.errors else validate(next)
        form.value = next.copy(errors = errors)
    }

    private fun validate(f: MachineFormState) =
        MachineForm.validate(f.name, f.host, f.port, f.user, f.auth, f.keyId, f.password, f.hasSavedPassword)

    fun save(onSaved: (String) -> Unit) {
        val f = form.value
        val errors = validate(f)
        if (!errors.ok) {
            form.value = f.copy(errors = errors)
            return
        }
        form.value = f.copy(saving = true, errors = errors)
        viewModelScope.launch {
            val id = original?.id ?: UUID.randomUUID().toString()
            val machine = (original ?: Machine(id = id, name = "", host = "", user = "")).copy(
                name = f.name.trim(),
                host = f.host.trim(),
                port = f.port.trim().toInt(),
                user = f.user.trim(),
                auth = f.auth,
                keyId = if (f.auth == AuthKind.Key) f.keyId else null,
            )
            withContext(Dispatchers.IO) {
                if (f.auth == AuthKind.Password && f.password.isNotEmpty()) graph.vault.putSecret(Machine.passwordSecret(id), f.password)
                if (f.auth == AuthKind.Key) graph.vault.deleteSecret(Machine.passwordSecret(id))
            }
            graph.hub.disconnect(id)
            graph.machines.upsert(machine)
            if (original == null) graph.selection.setMachine(id)
            form.update { it.copy(saving = false) }
            onSaved(id)
        }
    }
}

class MachineViewModel(private val graph: AppGraph, val machineId: String) : ViewModel() {
    val machine: StateFlow<Machine?> = graph.machines.machines.map { list -> list.firstOrNull { it.id == machineId } }
        .stateIn(viewModelScope, SharingStarted.Eagerly, null)
    val status: StateFlow<MachineStatus> = graph.hub.statuses.map { it[machineId] ?: MachineStatus() }
        .stateIn(viewModelScope, SharingStarted.Eagerly, graph.hub.status(machineId))

    val sessions = MutableStateFlow<List<AgentSession>>(emptyList())
    val sessionsLoading = MutableStateFlow(false)
    val sessionsError = MutableStateFlow<String?>(null)
    val install = MutableStateFlow<InstallState?>(null)

    fun load() = viewModelScope.launch {
        graph.hub.refresh(machineId)
        loadSessions()
    }

    private suspend fun loadSessions() {
        if (graph.hub.status(machineId).reach != Reach.Online) return
        sessionsLoading.value = true
        try {
            val found = graph.hub.sessions(machineId)
            sessions.value = found
            sessionsError.value = null
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            sessionsError.value = SessionHub.describe(e)
        } finally {
            sessionsLoading.value = false
        }
    }

    fun trust(presented: PresentedKey) = viewModelScope.launch {
        graph.hub.trustHostKey(machineId, presented)
        loadSessions()
    }

    fun openInstall(agent: Agent) {
        install.value = InstallState(agent)
    }

    fun runInstall() = viewModelScope.launch {
        val agent = install.value?.agent ?: return@launch
        install.value = InstallState(agent, running = true)
        val ok = runInstallOn(graph, machineId, agent) { line -> install.update { s -> s?.copy(lines = (s.lines + line).takeLast(400)) } }
        install.update { it?.copy(running = false, done = ok) }
        if (ok) loadSessions()
    }

    fun closeInstall() {
        if (install.value?.running == true) return
        install.value = null
    }

    /** Points the home composer at this machine. */
    fun useForNewChat() = viewModelScope.launch { graph.selection.setMachine(machineId) }
}

private suspend fun runInstallOn(graph: AppGraph, machineId: String, agent: Agent, onLine: (String) -> Unit): Boolean = try {
    graph.hub.install(machineId, agent, onLine)
} catch (e: Exception) {
    if (e is CancellationException) throw e
    onLine(SessionHub.describe(e))
    false
}

class HomeViewModel(private val graph: AppGraph) : ViewModel() {
    val draft = MutableStateFlow("")
    val refreshing = MutableStateFlow(false)
    val browse = MutableStateFlow<BrowseState?>(null)
    val install = MutableStateFlow<InstallState?>(null)

    private val base = combine(graph.machines.machines, graph.hub.statuses, graph.selection.selection) { machines, statuses, selection ->
        Triple(machines, statuses, selection)
    }

    val state: StateFlow<HomeState?> = combine(base, graph.recents.recents, graph.hub.running, graph.settings.settings, refreshing) { b, recents, running, settings, refreshing ->
        val (machines, statuses, selection) = b
        val machine = machines.firstOrNull { it.id == selection.machineId } ?: machines.firstOrNull()
        val status = machine?.let { statuses[it.id] } ?: MachineStatus()
        val names = machines.associate { it.id to it.name }
        HomeState(
            machine = machine,
            status = status,
            agent = selection.agent,
            cwd = machine?.let { selection.cwd(it.id) ?: it.recentDirs.firstOrNull() ?: status.home },
            recents = recents.filter { it.machineId in names }.map { chat ->
                RecentRow(chat, names.getValue(chat.machineId), running.any { r -> r.machineId == chat.machineId && r.sessionId == chat.sessionId })
            },
            refreshing = refreshing,
            hasMachines = machines.isNotEmpty(),
            artwork = settings.artwork,
        )
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), null)

    val machinesWithStatus: StateFlow<List<Pair<Machine, MachineStatus>>> = base.map { (machines, statuses, _) ->
        machines.map { it to (statuses[it.id] ?: MachineStatus()) }
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    fun refresh() {
        if (refreshing.value) return
        viewModelScope.launch {
            refreshing.value = true
            try {
                graph.hub.refreshRecents()
            } finally {
                refreshing.value = false
            }
        }
    }

    fun setAgent(agent: Agent) = viewModelScope.launch { graph.selection.setAgent(agent) }

    fun setMachine(id: String) = viewModelScope.launch {
        graph.selection.setMachine(id)
        graph.hub.refresh(id)
    }

    fun setCwd(cwd: String) = viewModelScope.launch {
        state.value?.machine?.id?.let { graph.selection.setCwd(it, cwd) }
    }

    /** The route for a new chat with the draft, or null when there's no machine yet. */
    fun send(): ChatRoute? {
        val s = state.value ?: return null
        val machine = s.machine ?: return null
        val text = draft.value.trim().ifEmpty { return null }
        draft.value = ""
        return ChatRoute(machine.id, s.agent.id, s.cwd ?: "~", prompt = text)
    }

    fun openBrowser() {
        val start = state.value?.cwd ?: "~"
        browse.value = BrowseState(path = start)
        navigate(start)
    }

    fun navigate(path: String) = viewModelScope.launch {
        val machineId = state.value?.machine?.id ?: return@launch
        browse.update { it?.copy(path = path, loading = true, error = null) }
        try {
            val listing = graph.hub.listDirs(machineId, path)
            browse.update { it?.copy(listing = listing, loading = false) }
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            browse.update { it?.copy(loading = false, error = SessionHub.describe(e)) }
        }
    }

    fun up() {
        val current = browse.value?.listing?.path ?: return
        navigate(current.trimEnd('/').substringBeforeLast('/').ifEmpty { "/" })
    }

    fun closeBrowser() {
        browse.value = null
    }

    fun openInstall(agent: Agent) {
        install.value = InstallState(agent)
    }

    fun runInstall() = viewModelScope.launch {
        val agent = install.value?.agent ?: return@launch
        val machineId = state.value?.machine?.id ?: return@launch
        install.value = InstallState(agent, running = true)
        val ok = runInstallOn(graph, machineId, agent) { line -> install.update { s -> s?.copy(lines = (s.lines + line).takeLast(400)) } }
        install.update { it?.copy(running = false, done = ok) }
        if (ok) graph.selection.setAgent(agent)
    }

    fun closeInstall() {
        if (install.value?.running == true) return
        install.value = null
    }
}

class OnboardingViewModel(private val graph: AppGraph) : ViewModel() {
    val step = MutableStateFlow(OnboardingStep.Welcome)
    val machine = MutableStateFlow(OnboardingMachine())
    val notificationsAsked = MutableStateFlow(false)

    fun generateKey() = viewModelScope.launch {
        val key = withContext(Dispatchers.Default) { graph.vault.generate(KeysViewModel.defaultKeyLabel()) }
        machine.update { it.copy(key = key) }
    }

    fun saveMachine() = viewModelScope.launch {
        val m = machine.value
        val key = m.key ?: return@launch
        val errors = MachineForm.validate(m.host, m.host, m.port, m.user, AuthKind.Key, key.id, "", false)
        if (!errors.ok) {
            machine.update { it.copy(error = listOfNotNull(errors.host, errors.user, errors.port).first()) }
            return@launch
        }
        val id = UUID.randomUUID().toString()
        graph.machines.upsert(Machine(id, MachineForm.defaultName(m.host), m.host, m.port.toInt(), m.user, AuthKind.Key, key.id))
        graph.selection.setMachine(id)
        machine.update { it.copy(saved = true, error = null) }
    }

    suspend fun finish() = graph.settings.markOnboarded()
    suspend fun markAsked() {
        graph.settings.markAskedForNotifications()
        notificationsAsked.value = true
    }
}

class ChatViewModel(private val graph: AppGraph, private val route: ChatRoute) : ViewModel() {
    val agent: Agent = Agent.fromId(route.agent)
    val chat = MutableStateFlow<StateFlow<ChatState>?>(null)
    val starting = MutableStateFlow(route.sessionId == null)
    val startError = MutableStateFlow<String?>(null)
    val draft = MutableStateFlow("")
    val cwd = MutableStateFlow(route.cwd)
    val machineName: StateFlow<String> = graph.machines.machines.map { list -> list.firstOrNull { it.id == route.machineId }?.name ?: "" }
        .stateIn(viewModelScope, SharingStarted.Eagerly, "")

    val sessionId get() = chat.value?.value?.sessionId ?: route.sessionId

    init {
        viewModelScope.launch {
            if (route.sessionId == null) {
                try {
                    var dir = route.cwd
                    if (dir == "~" || dir.isBlank()) {
                        // A machine that was never probed has no home yet: connect first.
                        graph.hub.refresh(route.machineId)
                        dir = graph.hub.status(route.machineId).home ?: throw java.io.IOException(graph.hub.status(route.machineId).message ?: "Couldn't reach the machine.")
                        cwd.value = dir
                    }
                    graph.selection.setCwd(route.machineId, dir)
                    val id = graph.hub.startChat(route.machineId, agent, dir)
                    chat.value = graph.hub.chat(route.machineId, agent, id)
                    route.prompt?.let { graph.hub.send(route.machineId, agent, id, it) }
                } catch (e: Exception) {
                    if (e is CancellationException) throw e
                    startError.value = "Couldn't start a chat: " + SessionHub.describe(e)
                } finally {
                    starting.value = false
                }
            } else {
                chat.value = graph.hub.prepareChat(route.machineId, agent, route.sessionId, route.cwd, route.title)
                graph.hub.openChat(route.machineId, agent, route.sessionId, route.cwd, route.title)
            }
        }
    }

    fun send() {
        val id = sessionId ?: return
        val text = draft.value.trim()
        if (text.isEmpty()) return
        graph.hub.send(route.machineId, agent, id, text)
        draft.value = ""
    }

    fun stop() {
        sessionId?.let { graph.hub.stop(route.machineId, agent, it) }
    }

    fun setOption(configId: String, value: String) = viewModelScope.launch {
        sessionId?.let { graph.hub.setOption(route.machineId, agent, it, configId, value) }
    }

    fun answer(optionId: String?) {
        sessionId?.let { graph.hub.answerPermission(route.machineId, agent, it, optionId) }
    }

    fun viewing(on: Boolean) {
        val id = sessionId ?: return
        val key = graph.hub.chatKey(route.machineId, agent, id)
        if (on) {
            graph.hub.viewing = key
            graph.notifier.cancelFor(route.machineId, id)
        } else if (graph.hub.viewing == key) {
            graph.hub.viewing = null
        }
    }

    suspend fun shouldAskForNotifications(): Boolean =
        com.teddytennant.wizard.notify.NotificationPolicy.shouldAskPermission(
            Build.VERSION.SDK_INT,
            graph.notifier.permissionGranted(),
            graph.settings.settings.first().askedForNotifications,
        )

    suspend fun markAsked() = graph.settings.markAskedForNotifications()
}
