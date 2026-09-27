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
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.acp.SessionInfo
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.session.Reach
import com.teddytennant.wizard.ui.Format
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.Hairline
import com.teddytennant.wizard.ui.components.IconAction
import com.teddytennant.wizard.ui.components.ListRow
import com.teddytennant.wizard.ui.components.Panel
import com.teddytennant.wizard.ui.components.SectionLabel
import com.teddytennant.wizard.ui.components.Spinner
import com.teddytennant.wizard.ui.components.StatusDot
import com.teddytennant.wizard.ui.components.TopBar
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

data class MachineScreenState(
    val machine: Machine,
    val status: MachineStatus,
    val sessions: List<SessionInfo> = emptyList(),
    val sessionsLoading: Boolean = false,
    val sessionsError: String? = null,
    val showAllSessions: Boolean = false,
) {
    /** Recent working directories: ones used from this phone first, then the ones sessions ran in. */
    val projects: List<String>
        get() = (machine.recentDirs + sessions.map { it.cwd }).filter { it.isNotBlank() }.distinct().take(6)
}

@Composable
fun MachineContent(
    state: MachineScreenState,
    onBack: () -> Unit,
    onEdit: () -> Unit,
    onRetry: () -> Unit,
    onInstall: () -> Unit,
    onNewSession: () -> Unit,
    onProject: (String) -> Unit,
    onBrowse: () -> Unit,
    onSession: (SessionInfo) -> Unit,
    onShowAll: () -> Unit,
) {
    val colors = WizardTheme.colors
    val status = state.status
    val home = status.home
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
            item { ConnectionBanner(status, onRetry, onInstall) }
            val ready = status.reach == Reach.Online && status.wizardVersion != null
            if (ready) {
                item {
                    WizardButton("New session", onNewSession, Modifier.fillMaxWidth().padding(top = 4.dp), icon = R.drawable.ic_plus)
                }
                item { SectionLabel("Projects", Modifier.padding(top = 16.dp)) }
                item {
                    Panel(padding = PaddingValues(0.dp)) {
                        state.projects.forEachIndexed { i, dir ->
                            if (i > 0) Hairline(inset = 50.dp)
                            ListRow(
                                Format.project(dir),
                                subtitle = Format.path(dir, home),
                                icon = R.drawable.ic_folder,
                                monoSubtitle = true,
                                onClick = { onProject(dir) },
                            ) { WizardIcon(R.drawable.ic_plus, "New session in ${Format.project(dir)}", tint = colors.faint, size = 18.dp) }
                        }
                        if (state.projects.isNotEmpty()) Hairline(inset = 50.dp)
                        ListRow("Browse folders", icon = R.drawable.ic_magnifer, onClick = onBrowse) {
                            WizardIcon(R.drawable.ic_alt_arrow_right, null, tint = colors.faint, size = 18.dp)
                        }
                    }
                }
                item {
                    SectionLabel("Sessions", Modifier.padding(top = 16.dp)) {
                        if (state.sessionsLoading) Spinner(size = 14.dp)
                    }
                }
                item {
                    val shown = if (state.showAllSessions) state.sessions else state.sessions.take(12)
                    when {
                        state.sessionsError != null -> Text(state.sessionsError, style = WizardTheme.type.small, color = colors.danger)
                        shown.isEmpty() && !state.sessionsLoading -> Text(
                            "No sessions yet. Start one above, or from Wizard on the machine.",
                            style = WizardTheme.type.small,
                            color = colors.faint,
                            modifier = Modifier.padding(vertical = 4.dp),
                        )
                        shown.isNotEmpty() -> Panel(padding = PaddingValues(0.dp)) {
                            shown.forEachIndexed { i, s ->
                                if (i > 0) Hairline(inset = 50.dp)
                                ListRow(
                                    s.title?.takeIf { it != "(no prompt)" } ?: "Untitled session",
                                    subtitle = Format.project(s.cwd) + "  ·  " + Format.ago(Format.instant(s.updatedAt) ?: Format.instant(s.sessionId)),
                                    icon = R.drawable.ic_chat_round_line,
                                    onClick = { onSession(s) },
                                )
                            }
                        }
                    }
                }
                if (!state.showAllSessions && state.sessions.size > 12) {
                    item { WizardButton("Show all ${state.sessions.size}", onShowAll, Modifier.fillMaxWidth(), kind = ButtonKind.Quiet) }
                }
            }
            item { Spacer(Modifier.navigationBarsPadding()) }
        }
    }
}

@Composable
private fun ConnectionBanner(status: MachineStatus, onRetry: () -> Unit, onInstall: () -> Unit) {
    val colors = WizardTheme.colors
    val (dot, words, pulsing) = statusLine(status)
    when {
        status.reach == Reach.Online && status.wizardVersion != null -> Row(
            Modifier.fillMaxWidth().padding(vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            StatusDot(dot)
            Text(words, style = WizardTheme.type.small, color = colors.muted, modifier = Modifier.padding(start = 9.dp))
        }
        status.reach == Reach.Online -> Panel {
            Text("Wizard isn't installed here", style = WizardTheme.type.heading, color = colors.text)
            Spacer(Modifier.height(6.dp))
            Text(
                "Install it into ~/.local/bin with Wizard's installer. You'll see its output as it runs.",
                style = WizardTheme.type.small,
                color = colors.muted,
            )
            Spacer(Modifier.height(16.dp))
            WizardButton("Install Wizard", onInstall)
        }
        status.reach == Reach.Checking || status.reach == Reach.Unknown -> Row(
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
