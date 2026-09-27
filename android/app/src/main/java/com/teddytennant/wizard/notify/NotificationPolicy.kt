package com.teddytennant.wizard.notify

import com.teddytennant.wizard.data.Settings

enum class TurnOutcome { Finished, TurnLimit, Cancelled, Failed }

/** What the notification says. */
data class NotificationText(val title: String, val body: String)

/** A turn in progress, for the ongoing notification. */
data class RunningTurn(
    val machineId: String,
    val machineName: String,
    val sessionId: String,
    val title: String?,
    val cwd: String,
    val agent: com.teddytennant.wizard.agent.Agent = com.teddytennant.wizard.agent.Agent.Wizard,
    /** Background tasks the session is running. */
    val tasks: Int = 0,
    /** The turn itself ended and only its background tasks are left. */
    val waiting: Boolean = false,
)

/** When to notify and what to say. No Android types, so it's tested on the JVM. */
object NotificationPolicy {
    data class Situation(
        val appInForeground: Boolean,
        /** The chat this is about is on screen right now. */
        val viewingChat: Boolean,
        val settings: Settings,
        val permissionGranted: Boolean,
    )

    fun outcome(stopReason: String?, failed: Boolean): TurnOutcome = when {
        failed -> TurnOutcome.Failed
        stopReason == "cancelled" -> TurnOutcome.Cancelled
        stopReason == "max_turn_requests" || stopReason == "max_tokens" -> TurnOutcome.TurnLimit
        else -> TurnOutcome.Finished
    }

    /** A finished turn is news only when nobody is looking at the app, and a stop the user asked for is never news. */
    fun notifyTurnEnd(outcome: TurnOutcome, stoppedByUser: Boolean, s: Situation): Boolean =
        s.permissionGranted && s.settings.notifyFinished && !s.appInForeground &&
            !(outcome == TurnOutcome.Cancelled && stoppedByUser)

    /** A question waits on the user unless they're already looking at that chat. */
    fun notifyNeedsInput(s: Situation): Boolean =
        s.permissionGranted && s.settings.notifyInput && !(s.appInForeground && s.viewingChat)

    /** Ask once, the first time a turn starts, on Android 13 and later. */
    fun shouldAskPermission(sdkInt: Int, granted: Boolean, askedBefore: Boolean): Boolean =
        sdkInt >= 33 && !granted && !askedBefore

    fun turnEnded(machine: String, outcome: TurnOutcome, firstLine: String?, error: String?, agent: String = "Wizard"): NotificationText {
        val line = firstLine?.let(::clip)
        return when (outcome) {
            TurnOutcome.Finished -> NotificationText("$agent finished on $machine", line ?: "The turn is done.")
            TurnOutcome.TurnLimit -> NotificationText("$agent stopped on $machine", "It reached the turn limit." + (line?.let { " $it" } ?: ""))
            TurnOutcome.Cancelled -> NotificationText("$agent stopped on $machine", line ?: "The turn was cancelled.")
            TurnOutcome.Failed -> NotificationText("$agent lost $machine", error?.let(::clip) ?: "The connection dropped mid-turn.")
        }
    }

    fun needsInput(machine: String, what: String, agent: String = "Wizard"): NotificationText =
        NotificationText("$agent needs you on $machine", clip(what))

    fun ongoing(turns: List<RunningTurn>): NotificationText = when {
        turns.isEmpty() -> NotificationText("Wizard", "Connecting")
        turns.size == 1 -> {
            val t = turns.single()
            val what = t.title?.let(::clip) ?: projectName(t.cwd)
            when {
                !t.waiting -> NotificationText("Working on ${t.machineName}", what)
                t.tasks == 0 -> NotificationText("Finishing up on ${t.machineName}", what)
                else -> NotificationText("Background tasks on ${t.machineName}", "${tasks(t.tasks)} running: $what")
            }
        }
        else -> {
            val machines = turns.map { it.machineName }.distinct()
            NotificationText(
                if (turns.all { it.waiting }) "Background tasks in ${turns.size} sessions" else "Working in ${turns.size} sessions",
                if (machines.size == 1) "On ${machines.single()}" else machines.joinToString(", "),
            )
        }
    }

    fun tasks(n: Int) = if (n == 1) "1 task" else "$n tasks"

    fun projectName(cwd: String): String = cwd.trimEnd('/').substringAfterLast('/').ifEmpty { cwd }

    private fun clip(text: String, max: Int = 160): String {
        val one = text.replace(Regex("\\s+"), " ").trim()
        return if (one.length <= max) one else one.take(max - 1).trimEnd() + "…"
    }
}
