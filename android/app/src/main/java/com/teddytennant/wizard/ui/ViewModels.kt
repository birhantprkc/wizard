package com.teddytennant.wizard.ui

import android.os.Build
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.teddytennant.wizard.AppGraph
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.data.AuthKind
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.data.MachineForm
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

    val sessions = MutableStateFlow<List<SessionInfo>>(emptyList())
    val sessionsLoading = MutableStateFlow(false)
    val sessionsError = MutableStateFlow<String?>(null)
    val browse = MutableStateFlow<BrowseState?>(null)
    val install = MutableStateFlow<InstallState?>(null)

    fun load() = viewModelScope.launch {
        graph.hub.refresh(machineId)
        loadSessions()
    }

    private suspend fun loadSessions() {
        val s = graph.hub.status(machineId)
        if (s.reach != Reach.Online || s.wizardVersion == null) return
        sessionsLoading.value = true
        try {
            sessions.value = graph.hub.sessions(machineId)
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

    fun openBrowser(start: String = "~") {
        browse.value = BrowseState(path = start)
        navigate(start)
    }

    fun navigate(path: String) = viewModelScope.launch {
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

    fun openInstall() {
        install.value = InstallState()
    }

    fun runInstall() = viewModelScope.launch {
        install.value = InstallState(running = true)
        val ok = try {
            graph.hub.install(machineId) { line -> install.update { s -> s?.copy(lines = (s.lines + line).takeLast(400)) } }
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            install.update { s -> s?.copy(lines = s.lines + SessionHub.describe(e)) }
            false
        }
        install.update { it?.copy(running = false, done = ok) }
        if (ok) loadSessions()
    }

    fun closeInstall() {
        if (install.value?.running == true) return
        install.value = null
    }
}

class ChatViewModel(private val graph: AppGraph, private val route: ChatRoute) : ViewModel() {
    val chat = MutableStateFlow<StateFlow<ChatState>?>(null)
    val starting = MutableStateFlow(route.sessionId == null)
    val startError = MutableStateFlow<String?>(null)
    val draft = MutableStateFlow("")
    val machineName: StateFlow<String> = graph.machines.machines.map { list -> list.firstOrNull { it.id == route.machineId }?.name ?: "" }
        .stateIn(viewModelScope, SharingStarted.Eagerly, "")

    val sessionId get() = chat.value?.value?.sessionId ?: route.sessionId

    init {
        viewModelScope.launch {
            if (route.sessionId == null) {
                try {
                    val id = graph.hub.startChat(route.machineId, route.cwd)
                    chat.value = graph.hub.chat(route.machineId, id)
                } catch (e: Exception) {
                    if (e is CancellationException) throw e
                    startError.value = "Couldn't start a session: " + SessionHub.describe(e)
                } finally {
                    starting.value = false
                }
            } else {
                chat.value = graph.hub.prepareChat(route.machineId, route.sessionId, route.cwd, route.title)
                graph.hub.openChat(route.machineId, route.sessionId, route.cwd, route.title)
            }
        }
    }

    fun send() {
        val id = sessionId ?: return
        val text = draft.value.trim()
        if (text.isEmpty()) return
        graph.hub.send(route.machineId, id, text)
        draft.value = ""
    }

    fun stop() {
        sessionId?.let { graph.hub.stop(route.machineId, it) }
    }

    fun setOption(configId: String, value: String) = viewModelScope.launch {
        sessionId?.let { graph.hub.setOption(route.machineId, it, configId, value) }
    }

    fun answer(optionId: String?) {
        sessionId?.let { graph.hub.answerPermission(route.machineId, it, optionId) }
    }

    fun viewing(on: Boolean) {
        val id = sessionId ?: return
        val key = "${route.machineId}/$id"
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
