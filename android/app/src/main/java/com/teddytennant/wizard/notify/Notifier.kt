package com.teddytennant.wizard.notify

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import com.teddytennant.wizard.MainActivity
import com.teddytennant.wizard.R

/** Channels and notifications. The decisions are in [NotificationPolicy]. */
class Notifier(private val context: Context) {
    companion object {
        const val CHANNEL_RUNNING = "running"
        const val CHANNEL_FINISHED = "finished"
        const val CHANNEL_INPUT = "input"
        const val ONGOING_ID = 1
        const val EXTRA_MACHINE = "machineId"
        const val EXTRA_SESSION = "sessionId"
        const val EXTRA_CWD = "cwd"
        private const val ACCENT = 0xFF8B7CF6.toInt()
    }

    fun createChannels() {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannels(
            listOf(
                NotificationChannel(CHANNEL_RUNNING, context.getString(R.string.channel_running), NotificationManager.IMPORTANCE_LOW).apply {
                    description = context.getString(R.string.channel_running_desc)
                    setShowBadge(false)
                },
                NotificationChannel(CHANNEL_FINISHED, context.getString(R.string.channel_finished), NotificationManager.IMPORTANCE_DEFAULT).apply {
                    description = context.getString(R.string.channel_finished_desc)
                },
                NotificationChannel(CHANNEL_INPUT, context.getString(R.string.channel_input), NotificationManager.IMPORTANCE_HIGH).apply {
                    description = context.getString(R.string.channel_input_desc)
                },
            ),
        )
    }

    fun permissionGranted(): Boolean =
        Build.VERSION.SDK_INT < 33 ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED

    fun openChatIntent(machineId: String, sessionId: String, cwd: String, requestCode: Int): PendingIntent {
        val intent = Intent(context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
            putExtra(EXTRA_MACHINE, machineId)
            putExtra(EXTRA_SESSION, sessionId)
            putExtra(EXTRA_CWD, cwd)
        }
        return PendingIntent.getActivity(context, requestCode, intent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
    }

    fun ongoing(turns: List<RunningTurn>): Notification {
        val text = NotificationPolicy.ongoing(turns)
        val builder = NotificationCompat.Builder(context, CHANNEL_RUNNING)
            .setSmallIcon(R.drawable.ic_wizard_mark)
            .setColor(ACCENT)
            .setContentTitle(text.title)
            .setContentText(text.body)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_PROGRESS)
            .setProgress(0, 0, true)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
        turns.firstOrNull()?.let { t ->
            builder.setContentIntent(openChatIntent(t.machineId, t.sessionId, t.cwd, ONGOING_ID))
            builder.addAction(0, if (turns.size == 1) "Stop" else "Stop all", TurnService.stopIntent(context))
        }
        return builder.build()
    }

    fun turnEnded(turn: RunningTurn, text: NotificationText) = post(CHANNEL_FINISHED, turn, text, NotificationCompat.PRIORITY_DEFAULT)

    fun needsInput(turn: RunningTurn, text: NotificationText) = post(CHANNEL_INPUT, turn, text, NotificationCompat.PRIORITY_HIGH)

    fun cancelFor(machineId: String, sessionId: String) {
        NotificationManagerCompat.from(context).cancel(idFor(machineId, sessionId))
    }

    private fun post(channel: String, turn: RunningTurn, text: NotificationText, priority: Int) {
        if (!permissionGranted()) return
        val id = idFor(turn.machineId, turn.sessionId)
        val notification = NotificationCompat.Builder(context, channel)
            .setSmallIcon(R.drawable.ic_wizard_mark)
            .setColor(ACCENT)
            .setContentTitle(text.title)
            .setContentText(text.body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text.body))
            .setSubText(turn.title ?: NotificationPolicy.projectName(turn.cwd))
            .setPriority(priority)
            .setAutoCancel(true)
            .setContentIntent(openChatIntent(turn.machineId, turn.sessionId, turn.cwd, id))
            .build()
        try {
            NotificationManagerCompat.from(context).notify(id, notification)
        } catch (_: SecurityException) {
        }
    }

    private fun idFor(machineId: String, sessionId: String) = 1000 + ("$machineId/$sessionId".hashCode() and 0x3fffffff)
}
