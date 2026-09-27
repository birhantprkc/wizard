package com.teddytennant.wizard.notify

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import com.teddytennant.wizard.WizardApp
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

/**
 * Holds the process in the foreground while any turn runs, so Android keeps
 * the SSH connection (and with it the remote `wizard acp`) alive with the
 * screen off. Stops itself when the last turn ends.
 */
class TurnService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var watcher: Job? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val graph = (application as WizardApp).graph
        val hub = graph.hub
        val notification = graph.notifier.ongoing(hub.running.value)
        ServiceCompat.startForeground(
            this,
            Notifier.ONGOING_ID,
            notification,
            if (Build.VERSION.SDK_INT >= 34) ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE else 0,
        )
        if (intent?.action == ACTION_STOP) hub.stopAll()
        if (watcher == null) {
            watcher = scope.launch {
                hub.running.collect { turns ->
                    if (turns.isEmpty()) {
                        ServiceCompat.stopForeground(this@TurnService, ServiceCompat.STOP_FOREGROUND_REMOVE)
                        stopSelf()
                    } else {
                        graph.notifier.ongoing(turns).let {
                            getSystemService(android.app.NotificationManager::class.java).notify(Notifier.ONGOING_ID, it)
                        }
                    }
                }
            }
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    companion object {
        private const val ACTION_STOP = "com.teddytennant.wizard.STOP_TURNS"

        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, TurnService::class.java))
        }

        fun stopIntent(context: Context): PendingIntent = PendingIntent.getService(
            context,
            2,
            Intent(context, TurnService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }
}
