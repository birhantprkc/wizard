package com.teddytennant.wizard

import android.app.Application
import android.content.Context
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import com.teddytennant.wizard.data.MachineStore
import com.teddytennant.wizard.data.SettingsStore
import com.teddytennant.wizard.notify.NotificationText
import com.teddytennant.wizard.notify.Notifier
import com.teddytennant.wizard.notify.RunningTurn
import com.teddytennant.wizard.notify.TurnService
import com.teddytennant.wizard.session.HubEnvironment
import com.teddytennant.wizard.session.SessionHub
import com.teddytennant.wizard.ssh.AesGcmBox
import com.teddytennant.wizard.ssh.KeyVault
import com.teddytennant.wizard.ssh.KeystoreKey
import com.teddytennant.wizard.ssh.KnownHostsStore
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import org.bouncycastle.jce.provider.BouncyCastleProvider
import java.io.File
import java.security.Security

/** Everything the screens share, built once. */
class AppGraph(context: Context) {
    private val app = context.applicationContext
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val dataStore = SettingsStore.dataStore(app)
    val machines = MachineStore(dataStore)
    val settings = SettingsStore(dataStore)
    private val vaultKey by lazy { KeystoreKey.get() }
    val vault = KeyVault(File(app.filesDir, "vault"), AesGcmBox { vaultKey })
    val knownHosts = KnownHostsStore(File(app.filesDir, "known_hosts.json"))
    val notifier = Notifier(app)
    val version: String = runCatching { app.packageManager.getPackageInfo(app.packageName, 0).versionName }.getOrNull() ?: "dev"

    val hub = SessionHub(
        machines = machines,
        vault = vault,
        knownHosts = knownHosts,
        settings = settings,
        env = object : HubEnvironment {
            override val appInForeground: Boolean
                get() = ProcessLifecycleOwner.get().lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)
            override fun startTurnService() = TurnService.start(app)
            override fun permissionGranted() = notifier.permissionGranted()
            override fun postTurnEnded(turn: RunningTurn, text: NotificationText) = notifier.turnEnded(turn, text)
            override fun postNeedsInput(turn: RunningTurn, text: NotificationText) = notifier.needsInput(turn, text)
        },
        scope = scope,
        clientVersion = version,
    )
}

class WizardApp : Application() {
    lateinit var graph: AppGraph
        private set

    override fun onCreate() {
        super.onCreate()
        // Android ships a cut-down provider named "BC"; sshj needs the full one for Ed25519 and friends.
        Security.removeProvider("BC")
        Security.insertProviderAt(BouncyCastleProvider(), 1)
        graph = AppGraph(this)
        graph.notifier.createChannels()
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = graph.hub.onForeground()
            override fun onStop(owner: LifecycleOwner) = graph.hub.onBackground()
        })
    }
}
