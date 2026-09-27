package com.teddytennant.wizard.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.session.AgentSession
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.session.Reach
import com.teddytennant.wizard.session.SessionHub
import com.teddytennant.wizard.ui.components.AgentTile
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.Hairline
import com.teddytennant.wizard.ui.components.IconAction
import com.teddytennant.wizard.ui.components.Panel
import com.teddytennant.wizard.ui.components.SectionLabel
import com.teddytennant.wizard.ui.components.Spinner
import com.teddytennant.wizard.ui.components.StatusDot
import com.teddytennant.wizard.ui.components.TextAction
import com.teddytennant.wizard.ui.components.TopBar
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.theme.WizardTheme
import java.time.Instant

data class MachineScreenState(
    val machine: Machine,
    val status: MachineStatus,
    val sessions: List<AgentSession> = emptyList(),
    val sessionsLoading: Boolean = false,
    val sessionsError: String? = null,
    val showAllSessions: Boolean = false,
)

@Composable
fun MachineContent(
    state: MachineScreenState,
    onBack: () -> Unit,
    onEdit: () -> Unit,
    onRetry: () -> Unit,
    onInstall: (Agent) -> Unit,
    onNewChat: () -> Unit,
    onSession: (AgentSession) -> Unit,
    onShowAll: () -> Unit,
    now: Instant = Instant.now(),
) {
    val colors = WizardTheme.colors
    val status = state.status
    Column(Modifier.fillMaxSize().background(colors.background)) {
        TopBar(state.machine.name, onBack, subtitle = state.machine.address) {
            IconAction(R.drawable.ic_refresh, "Refresh", onRetry)
            IconAction(R.drawable.ic_pen, "Edit machine", onEdit)
        }
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = PaddingValues(start = 20.dp, end = 20.dp, top = 8.dp, bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item { ConnectionBanner(status, onRetry) }
            if (status.reach == Reach.Online) {
                item {
                    WizardButton("New chat here", onNewChat, Modifier.fillMaxWidth().padding(top = 4.dp), icon = R.drawable.ic_plus, enabled = status.readyAgents.isNotEmpty())
                }
                item { SectionLabel("Agents", Modifier.padding(top = 16.dp)) }
                item {
                    Panel(padding = PaddingValues(0.dp)) {
                        Agent.entries.forEachIndexed { i, agent ->
                            if (i > 0) Hairline(inset = 66.dp)
                            val a = status.agent(agent)
                            Row(Modifier.fillMaxWidth().heightIn(min = 64.dp).padding(start = 14.dp, end = 6.dp, top = 10.dp, bottom = 10.dp), verticalAlignment = Alignment.CenterVertically) {
                                AgentTile(agent)
                                Column(Modifier.weight(1f).padding(start = 14.dp)) {
                                    Text(agent.displayName, style = WizardTheme.type.body, color = colors.text)
                                    Text(
                                        when {
                                            a.ready -> a.version ?: "Ready"
                                            a.missing != null -> a.missing
                                            else -> "Not installed"
                                        },
                                        style = WizardTheme.type.small,
                                        color = colors.faint,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                }
                                if (!a.ready) TextAction("Install", { onInstall(agent) })
                            }
                        }
                    }
                }
                item {
                    SectionLabel("Sessions", Modifier.padding(top = 16.dp)) {
                        if (state.sessionsLoading) Spinner(size = 14.dp)
                    }
                }
                item {
                    val shown = if (state.showAllSessions) state.sessions else state.sessions.take(15)
                    when {
                        state.sessionsError != null -> Text(state.sessionsError, style = WizardTheme.type.small, color = colors.danger)
                        shown.isEmpty() && !state.sessionsLoading -> Text(
                            "No saved sessions yet.",
                            style = WizardTheme.type.small,
                            color = colors.faint,
                            modifier = Modifier.padding(vertical = 4.dp),
                        )
                        shown.isNotEmpty() -> Panel(padding = PaddingValues(0.dp)) {
                            shown.forEachIndexed { i, s ->
                                if (i > 0) Hairline(inset = 66.dp)
                                val chat = com.teddytennant.wizard.data.RecentChat(
                                    state.machine.id, s.agent.id, s.info.sessionId, s.info.cwd,
                                    s.info.title?.takeIf { it != "(no prompt)" },
                                    SessionHub.parseTime(s.info.updatedAt) ?: SessionHub.parseTime(s.info.sessionId) ?: 0,
                                )
                                RecentChatRow(RecentRow(chat, "", false), now) { onSession(s) }
                            }
                        }
                    }
                }
                if (!state.showAllSessions && state.sessions.size > 15) {
                    item { WizardButton("Show all ${state.sessions.size}", onShowAll, Modifier.fillMaxWidth(), kind = ButtonKind.Quiet) }
                }
            }
            item { Spacer(Modifier.navigationBarsPadding()) }
        }
    }
}

@Composable
private fun ConnectionBanner(status: MachineStatus, onRetry: () -> Unit) {
    val colors = WizardTheme.colors
    val (dot, words, pulsing) = statusLine(status)
    when (status.reach) {
        Reach.Online -> Row(Modifier.fillMaxWidth().padding(vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            StatusDot(dot)
            Text(words, style = WizardTheme.type.small, color = colors.muted, modifier = Modifier.padding(start = 9.dp))
        }
        Reach.Checking, Reach.Unknown -> Row(
            Modifier.fillMaxWidth().padding(vertical = 24.dp),
            horizontalArrangement = Arrangement.Center,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Spinner()
            Text("Connecting", style = WizardTheme.type.small, color = colors.muted, modifier = Modifier.padding(start = 10.dp))
        }
        else -> Panel {
            Row(verticalAlignment = Alignment.CenterVertically) {
                StatusDot(dot, pulsing = pulsing)
                Text(words, style = WizardTheme.type.heading, color = colors.text, modifier = Modifier.padding(start = 10.dp))
            }
            status.message?.let {
                Spacer(Modifier.height(6.dp))
                Text(it, style = WizardTheme.type.small, color = colors.muted)
            }
            Spacer(Modifier.height(16.dp))
            WizardButton("Try again", onRetry, kind = ButtonKind.Quiet)
        }
    }
}

