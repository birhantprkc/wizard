package com.teddytennant.wizard.ui

import android.Manifest
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.os.Build
import android.provider.Settings as AndroidSettings
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.NavHostController
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import androidx.navigation.toRoute
import com.teddytennant.wizard.AppGraph
import com.teddytennant.wizard.data.Settings
import com.teddytennant.wizard.notify.TurnService
import com.teddytennant.wizard.session.ChatState
import com.teddytennant.wizard.session.Reach
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.ui.screens.AgentSheetContent
import com.teddytennant.wizard.ui.screens.FolderSheetContent
import com.teddytennant.wizard.ui.screens.HomeContent
import com.teddytennant.wizard.ui.screens.MachineSheetContent
import com.teddytennant.wizard.ui.screens.OnboardingContent
import com.teddytennant.wizard.ui.screens.OnboardingStep
import com.teddytennant.wizard.ssh.StoredKey
import com.teddytennant.wizard.ui.screens.AboutContent
import com.teddytennant.wizard.ui.screens.BrowseSheetContent
import com.teddytennant.wizard.ui.screens.ChatContent
import com.teddytennant.wizard.ui.screens.ConfirmContent
import com.teddytennant.wizard.ui.screens.EditMachineContent
import com.teddytennant.wizard.ui.screens.HostKeyChangedContent
import com.teddytennant.wizard.ui.screens.HostKeyTrustContent
import com.teddytennant.wizard.ui.screens.ImportKeyContent
import com.teddytennant.wizard.ui.screens.InstallSheetContent
import com.teddytennant.wizard.ui.screens.KeysContent
import com.teddytennant.wizard.ui.screens.LicenseEntry
import com.teddytennant.wizard.ui.screens.MachineContent
import com.teddytennant.wizard.ui.screens.MachineScreenState
import com.teddytennant.wizard.ui.screens.MachinesContent
import com.teddytennant.wizard.ui.screens.OptionsSheetContent
import com.teddytennant.wizard.ui.screens.SettingsContent
import com.teddytennant.wizard.ui.screens.TextDocumentContent
import com.teddytennant.wizard.ui.screens.WizardDialog
import com.teddytennant.wizard.ui.screens.WizardSheet
import com.teddytennant.wizard.ui.theme.WizardTheme
import kotlinx.coroutines.launch

private const val NAV_MS = 240

@Composable
fun WizardNavHost(graph: AppGraph, settings: Settings, startWithOnboarding: Boolean, pendingChat: ChatRoute?, onChatOpened: () -> Unit) {
    val nav = rememberNavController()
    LaunchedEffect(pendingChat) {
        if (pendingChat != null) {
            // Replace any open chat so back goes to the machine, not to a second copy of this chat.
            nav.navigate(pendingChat) {
                popUpTo<ChatRoute> { inclusive = true }
                launchSingleTop = true
            }
            onChatOpened()
        }
    }
    NavHost(
        navController = nav,
        startDestination = if (startWithOnboarding) OnboardingRoute else HomeRoute,
        modifier = Modifier.fillMaxSize().background(WizardTheme.colors.background),
        // Short tweens rather than the default springs: with predictive back these
        // track the finger, and on release they finish in a quarter second.
        enterTransition = { slideInHorizontally(tween(NAV_MS)) { it / 8 } + fadeIn(tween(NAV_MS)) },
        exitTransition = { fadeOut(tween(NAV_MS)) },
        popEnterTransition = { fadeIn(tween(NAV_MS)) },
        popExitTransition = { slideOutHorizontally(tween(NAV_MS)) { it / 8 } + fadeOut(tween(NAV_MS)) },
    ) {
        composable<OnboardingRoute> { OnboardingRouteScreen(graph, nav, settings) }
        composable<HomeRoute> { HomeRouteScreen(graph, nav) }
        composable<MachinesRoute> { MachinesRouteScreen(graph, nav) }
        composable<EditMachineRoute> { entry -> EditMachineRouteScreen(graph, nav, entry.toRoute<EditMachineRoute>().machineId) }
        composable<MachineRoute> { entry -> MachineRouteScreen(graph, nav, entry.toRoute<MachineRoute>().machineId) }
        composable<ChatRoute> { entry -> ChatRouteScreen(graph, nav, entry.toRoute(), settings.compact) }
        composable<SettingsRoute> { SettingsRouteScreen(graph, nav) }
        composable<KeysRoute> { KeysRouteScreen(graph, nav) }
        composable<AboutRoute> { AboutRouteScreen(graph, nav) }
    }
}

private fun copy(context: Context, label: String, text: String) {
    context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText(label, text))
    if (Build.VERSION.SDK_INT < 33) Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
}

private fun share(context: Context, key: StoredKey) {
    val send = Intent(Intent.ACTION_SEND).apply {
        type = "text/plain"
        putExtra(Intent.EXTRA_SUBJECT, "SSH public key: ${key.label}")
        putExtra(Intent.EXTRA_TEXT, key.publicKey)
    }
    context.startActivity(Intent.createChooser(send, "Share public key"))
}

@Composable
private fun MachinesRouteScreen(graph: AppGraph, nav: NavHostController) {
    val vm = viewModel { MachinesViewModel(graph) }
    val cards by vm.cards.collectAsStateWithLifecycle()
    val refreshing by vm.refreshing.collectAsStateWithLifecycle()
    var deleting by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) { vm.refresh(showSpinner = false) }
    val list = cards ?: return
    MachinesContent(
        cards = list,
        refreshing = refreshing,
        onRefresh = { vm.refresh() },
        onOpen = { nav.navigate(MachineRoute(it)) },
        onAdd = { nav.navigate(EditMachineRoute()) },
        onEdit = { nav.navigate(EditMachineRoute(it)) },
        onDelete = { deleting = it },
        onBack = { nav.popBackStack() },
    )
    deleting?.let { id ->
        val name = list.firstOrNull { it.machine.id == id }?.machine?.name ?: "this machine"
        WizardDialog({ deleting = null }) {
            ConfirmContent(
                "Remove $name?",
                "Its saved password goes with it. Keys and trusted host keys stay. Sessions on the machine aren't touched.",
                "Remove",
                danger = true,
                onConfirm = { vm.delete(id); deleting = null },
                onCancel = { deleting = null },
            )
        }
    }
}

@Composable
private fun EditMachineRouteScreen(graph: AppGraph, nav: NavHostController, machineId: String?) {
    val vm = viewModel(key = "edit-$machineId") { EditMachineViewModel(graph, machineId) }
    val keysVm = viewModel { KeysViewModel(graph) }
    val form by vm.form.collectAsStateWithLifecycle()
    val keys by keysVm.keys.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var importing by remember { mutableStateOf(false) }
    EditMachineContent(
        editing = machineId != null,
        form = form,
        keys = keys,
        onChange = vm::update,
        onGenerateKey = { scope.launch { val key = keysVm.generate(); vm.update(vm.form.value.copy(keyId = key.id)) } },
        onImportKey = { importing = true },
        onCopyKey = { copy(context, "SSH public key", it.publicKey) },
        onShareKey = { share(context, it) },
        onSave = {
            vm.save { id ->
                if (machineId == null) {
                    nav.navigate(MachineRoute(id)) { popUpTo(EditMachineRoute()) { inclusive = true } }
                } else {
                    nav.popBackStack()
                }
            }
        },
        onBack = { nav.popBackStack() },
    )
    if (importing) {
        ImportKeyDialog(keysVm, onDone = { key -> importing = false; if (key != null) vm.update(vm.form.value.copy(keyId = key.id)) })
    }
}

@Composable
private fun ImportKeyDialog(vm: KeysViewModel, onDone: (StoredKey?) -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var text by remember { mutableStateOf("") }
    var passphrase by remember { mutableStateOf("") }
    var label by remember { mutableStateOf("") }
    val error by vm.importError.collectAsStateWithLifecycle()
    val busy by vm.importing.collectAsStateWithLifecycle()
    val picker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) {
            runCatching {
                context.contentResolver.openInputStream(uri)?.use { stream -> stream.readBytes().take(64 * 1024).toByteArray().decodeToString() }
            }.getOrNull()?.let { text = it }
            if (label.isBlank()) label = uri.lastPathSegment?.substringAfterLast('/')?.substringAfterLast(':') ?: ""
        }
    }
    WizardDialog({ onDone(null) }) {
        ImportKeyContent(
            text = text,
            passphrase = passphrase,
            label = label,
            error = error,
            busy = busy,
            onText = { text = it },
            onPassphrase = { passphrase = it },
            onLabel = { label = it },
            onPickFile = { picker.launch(arrayOf("*/*")) },
            onImport = { scope.launch { vm.import(label, text, passphrase)?.let(onDone) } },
        )
    }
}

@Composable
private fun MachineRouteScreen(graph: AppGraph, nav: NavHostController, machineId: String) {
    val vm = viewModel(key = "machine-$machineId") { MachineViewModel(graph, machineId) }
    val machine by vm.machine.collectAsStateWithLifecycle()
    val status by vm.status.collectAsStateWithLifecycle()
    val sessions by vm.sessions.collectAsStateWithLifecycle()
    val loading by vm.sessionsLoading.collectAsStateWithLifecycle()
    val error by vm.sessionsError.collectAsStateWithLifecycle()
    val install by vm.install.collectAsStateWithLifecycle()
    var showAll by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    LaunchedEffect(vm) { vm.loadOnce() }
    val m = machine ?: return
    MachineContent(
        state = MachineScreenState(m, status, sessions, loading, error, showAll),
        onBack = { nav.popBackStack() },
        onEdit = { nav.navigate(EditMachineRoute(machineId)) },
        onRetry = { vm.load() },
        onInstall = vm::openInstall,
        onNewChat = {
            scope.launch {
                vm.useForNewChat().join()
                nav.navigate(HomeRoute) { popUpTo(HomeRoute) { inclusive = true } }
            }
        },
        onSession = { nav.navigate(ChatRoute(machineId, it.agent.id, it.info.cwd, it.info.sessionId, it.info.title)) },
        onShowAll = { showAll = true },
    )
    HostKeyPrompts(status, onTrust = vm::trust)
    install?.let { state ->
        WizardSheet(onDismiss = vm::closeInstall) {
            InstallSheetContent(m.name, state, onInstall = vm::runInstall, onClose = vm::closeInstall)
        }
    }
}

/** First-contact and changed host keys, wherever a machine gets connected. */
@Composable
private fun HostKeyPrompts(status: MachineStatus, onTrust: (com.teddytennant.wizard.ssh.PresentedKey) -> Unit) {
    var dismissedKey by remember { mutableStateOf<String?>(null) }
    val presented = status.presented
    if (presented != null && dismissedKey != presented.fingerprint) {
        when (status.reach) {
            Reach.NeedsTrust -> WizardDialog({ dismissedKey = presented.fingerprint }) {
                HostKeyTrustContent(presented, onTrust = { onTrust(presented) }, onCancel = { dismissedKey = presented.fingerprint })
            }
            Reach.KeyChanged -> WizardDialog({ dismissedKey = presented.fingerprint }, dismissible = false) {
                HostKeyChangedContent(presented, status.trusted!!, onReplace = { onTrust(presented) }, onCancel = { dismissedKey = presented.fingerprint })
            }
            else -> Unit
        }
    }
}

private enum class HomeSheet { Agent, Machine, Folder }

@Composable
private fun HomeRouteScreen(graph: AppGraph, nav: NavHostController) {
    val vm = viewModel { HomeViewModel(graph) }
    val state by vm.state.collectAsStateWithLifecycle()
    val draft by vm.draft.collectAsStateWithLifecycle()
    val browse by vm.browse.collectAsStateWithLifecycle()
    val install by vm.install.collectAsStateWithLifecycle()
    val machines by vm.machinesWithStatus.collectAsStateWithLifecycle()
    var sheet by remember { mutableStateOf<HomeSheet?>(null) }
    val scope = rememberCoroutineScope()
    // Coming back from another screen doesn't redo the SSH round trips; only a stale list does.
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(lifecycle) {
        val observer = LifecycleEventObserver { _, event -> if (event == Lifecycle.Event.ON_START) vm.refreshIfStale() }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer) }
    }
    val s = state ?: return
    HomeContent(
        state = s,
        draft = draft,
        onDraft = { vm.draft.value = it },
        onSend = {
            if (s.machine == null) nav.navigate(EditMachineRoute()) else vm.send()?.let { nav.navigate(it) }
        },
        onAgent = { sheet = HomeSheet.Agent },
        onMachine = { if (s.machine == null) nav.navigate(EditMachineRoute()) else sheet = HomeSheet.Machine },
        onFolder = { sheet = HomeSheet.Folder },
        onRecent = { nav.navigate(ChatRoute(it.machineId, it.agentId, it.cwd, it.sessionId, it.title)) },
        onSettings = { nav.navigate(SettingsRoute) },
    )
    s.machine?.let { m ->
        if (s.status.presented != null) {
            HostKeyPrompts(s.status) { key ->
                scope.launch {
                    graph.hub.trustHostKey(m.id, key)
                    vm.refresh()
                }
            }
        }
    }
    when (sheet) {
        HomeSheet.Agent -> WizardSheet(onDismiss = { sheet = null }) {
            AgentSheetContent(s.agent, s.machine?.let { s.status }, s.machine?.name, onPick = { agent ->
                vm.setAgent(agent)
                sheet = null
            }, onInstall = { agent ->
                sheet = null
                vm.openInstall(agent)
            })
        }
        HomeSheet.Machine -> WizardSheet(onDismiss = { sheet = null }) {
            MachineSheetContent(machines, s.machine?.id, onPick = { vm.setMachine(it); sheet = null }, onAdd = {
                sheet = null
                nav.navigate(EditMachineRoute())
            }, onManage = {
                sheet = null
                nav.navigate(MachinesRoute)
            })
        }
        HomeSheet.Folder -> WizardSheet(onDismiss = { sheet = null }) {
            FolderSheetContent(s.machine?.recentDirs.orEmpty(), s.cwd, s.status.home, onPick = { vm.setCwd(it); sheet = null }, onBrowse = {
                sheet = null
                vm.openBrowser()
            })
        }
        null -> Unit
    }
    browse?.let { b ->
        WizardSheet(onDismiss = vm::closeBrowser) {
            BrowseSheetContent(b, s.status.home, onOpen = { vm.navigate(it) }, onUp = vm::up, onPick = { dir ->
                vm.setCwd(dir)
                vm.closeBrowser()
            })
        }
    }
    install?.let { i ->
        WizardSheet(onDismiss = vm::closeInstall) {
            InstallSheetContent(s.machine?.name ?: "", i, onInstall = vm::runInstall, onClose = vm::closeInstall)
        }
    }
}

@Composable
private fun OnboardingRouteScreen(graph: AppGraph, nav: NavHostController, settings: Settings) {
    val vm = viewModel { OnboardingViewModel(graph) }
    val step by vm.step.collectAsStateWithLifecycle()
    val machine by vm.machine.collectAsStateWithLifecycle()
    val asked by vm.notificationsAsked.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val askPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        vm.step.value = OnboardingStep.Done
    }
    OnboardingContent(
        step = step,
        settings = settings,
        machine = machine,
        notificationsAsked = asked || graph.notifier.permissionGranted(),
        onStep = { vm.step.value = it },
        onTheme = { scope.launch { graph.settings.setTheme(it) } },
        onCompact = { scope.launch { graph.settings.setCompact(it) } },
        onArtwork = { scope.launch { graph.settings.setArtwork(it) } },
        onMachine = { vm.machine.value = it },
        onGenerateKey = { vm.generateKey() },
        onCopyKey = { machine.key?.let { copy(context, "SSH public key", it.publicKey) } },
        onShareKey = { machine.key?.let { share(context, it) } },
        onSaveMachine = { vm.saveMachine() },
        onAllowNotifications = {
            scope.launch {
                vm.markAsked()
                if (Build.VERSION.SDK_INT >= 33) askPermission.launch(Manifest.permission.POST_NOTIFICATIONS) else vm.step.value = OnboardingStep.Done
            }
        },
        onFinish = {
            scope.launch {
                vm.finish()
                nav.navigate(HomeRoute) { popUpTo(OnboardingRoute) { inclusive = true } }
            }
        },
    )
}

@Composable
private fun ChatRouteScreen(graph: AppGraph, nav: NavHostController, route: ChatRoute, compact: Boolean) {
    val vm = viewModel(key = "chat-${route.machineId}-${route.agent}-${route.sessionId}-${route.cwd}-${route.prompt.hashCode()}") { ChatViewModel(graph, route) }
    val chatFlow by vm.chat.collectAsStateWithLifecycle()
    val state: ChatState? = chatFlow?.collectAsStateWithLifecycle()?.value
    val starting by vm.starting.collectAsStateWithLifecycle()
    val startError by vm.startError.collectAsStateWithLifecycle()
    val draft by vm.draft.collectAsStateWithLifecycle()
    val machineName by vm.machineName.collectAsStateWithLifecycle()
    val cwd by vm.cwd.collectAsStateWithLifecycle()
    var options by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val askPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        // The ongoing notification was posted before this answer; post it again so it shows.
        if (granted && graph.hub.running.value.isNotEmpty()) TurnService.start(context)
    }

    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val sessionId = state?.sessionId
    DisposableEffect(lifecycle, sessionId) {
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_RESUME -> vm.viewing(true)
                Lifecycle.Event.ON_PAUSE -> vm.viewing(false)
                else -> Unit
            }
        }
        lifecycle.addObserver(observer)
        onDispose {
            lifecycle.removeObserver(observer)
            vm.viewing(false)
        }
    }

    // The first message goes out on its own, so ask about notifications as the chat opens.
    LaunchedEffect(Unit) {
        if (route.prompt != null && vm.shouldAskForNotifications()) {
            vm.markAsked()
            if (Build.VERSION.SDK_INT >= 33) askPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }
    ChatContent(
        state = state,
        agent = vm.agent,
        machineName = machineName,
        cwd = cwd,
        starting = starting,
        startError = startError,
        draft = draft,
        onDraft = { vm.draft.value = it },
        onSend = {
            vm.send()
            // Ask for notifications the first time a turn starts, when the reason is obvious.
            scope.launch {
                if (vm.shouldAskForNotifications()) {
                    vm.markAsked()
                    if (Build.VERSION.SDK_INT >= 33) askPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
                }
            }
        },
        onStop = vm::stop,
        onOptions = { options = true },
        onPermission = vm::answer,
        onBack = { nav.popBackStack() },
        compact = compact,
    )
    if (options && state != null) {
        WizardSheet(onDismiss = { options = false }) {
            OptionsSheetContent(state.options, enabled = !state.running, onPick = { id, value -> vm.setOption(id, value) })
        }
    }
}

@Composable
private fun SettingsRouteScreen(graph: AppGraph, nav: NavHostController) {
    val settings by graph.settings.settings.collectAsStateWithLifecycle(initialValue = Settings())
    val keysVm = viewModel { KeysViewModel(graph) }
    val keys by keysVm.keys.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var allowed by remember { mutableStateOf(graph.notifier.permissionGranted()) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(lifecycle) {
        val observer = LifecycleEventObserver { _, e ->
            if (e == Lifecycle.Event.ON_RESUME) {
                allowed = graph.notifier.permissionGranted()
                keysVm.reload()
            }
        }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer) }
    }
    val machineList by graph.machines.machines.collectAsState(initial = emptyList())
    SettingsContent(
        settings = settings,
        machineCount = machineList.size,
        keyCount = keys.size,
        trustedHosts = remember(keys) { graph.knownHosts.load().entries.size },
        notificationsAllowed = allowed,
        version = graph.version,
        onMachines = { nav.navigate(MachinesRoute) },
        onTheme = { scope.launch { graph.settings.setTheme(it) } },
        onCompact = { scope.launch { graph.settings.setCompact(it) } },
        onArtwork = { scope.launch { graph.settings.setArtwork(it) } },
        onNotifyFinished = { scope.launch { graph.settings.setNotifyFinished(it) } },
        onNotifyInput = { scope.launch { graph.settings.setNotifyInput(it) } },
        onSystemNotifications = {
            context.startActivity(
                Intent(AndroidSettings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(AndroidSettings.EXTRA_APP_PACKAGE, context.packageName),
            )
        },
        onKeys = { nav.navigate(KeysRoute) },
        onAbout = { nav.navigate(AboutRoute) },
        onBack = { nav.popBackStack() },
    )
}

@Composable
private fun KeysRouteScreen(graph: AppGraph, nav: NavHostController) {
    val vm = viewModel { KeysViewModel(graph) }
    val keys by vm.keys.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var importing by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf<StoredKey?>(null) }
    val machines by graph.machines.machines.collectAsState(initial = emptyList())
    KeysContent(
        keys = keys,
        onGenerate = { scope.launch { vm.generate() } },
        onImport = { importing = true },
        onCopy = { copy(context, "SSH public key", it.publicKey) },
        onShare = { share(context, it) },
        onDelete = { deleting = it },
        onBack = { nav.popBackStack() },
    )
    if (importing) ImportKeyDialog(vm) { importing = false }
    deleting?.let { key ->
        val users = machines.filter { it.keyId == key.id }.map { it.name }
        WizardDialog({ deleting = null }) {
            ConfirmContent(
                "Delete ${key.label}?",
                (if (users.isEmpty()) "" else "${users.joinToString(", ")} sign in with it. ") +
                    "The private key is gone for good; remove its line from authorized_keys on your machines too.",
                "Delete",
                danger = true,
                onConfirm = { vm.delete(key); deleting = null },
                onCancel = { deleting = null },
            )
        }
    }
}

@Composable
private fun AboutRouteScreen(graph: AppGraph, nav: NavHostController) {
    val context = LocalContext.current
    var open by remember { mutableStateOf<Pair<LicenseEntry, String>?>(null) }
    AboutContent(
        version = graph.version,
        onLicense = { entry ->
            val raw = runCatching { context.assets.open(entry.asset!!).bufferedReader().use { it.readText() } }.getOrDefault("")
            // License files are hard-wrapped at 80 columns; let paragraphs flow to the screen instead.
            val text = raw.replace("\r\n", "\n").split(Regex("\n\\s*\n")).joinToString("\n\n") { it.trim().replace(Regex("\\s*\n\\s*"), " ") }
            open = entry to text
        },
        onBack = { nav.popBackStack() },
    )
    open?.let { (entry, text) ->
        WizardDialog({ open = null }) { TextDocumentContent(entry.name, text) { open = null } }
    }
}

