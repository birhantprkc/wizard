package com.teddytennant.wizard.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.acp.ConfigOption
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.session.Reach
import com.teddytennant.wizard.ui.components.AgentTile
import com.teddytennant.wizard.ui.components.StatusDot
import com.teddytennant.wizard.ui.components.TextAction
import com.teddytennant.wizard.ssh.RemoteScripts
import com.teddytennant.wizard.ui.Format
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.FieldShape
import com.teddytennant.wizard.ui.components.Hairline
import com.teddytennant.wizard.ui.components.ListRow
import com.teddytennant.wizard.ui.components.PillShape
import com.teddytennant.wizard.ui.components.SectionLabel
import com.teddytennant.wizard.ui.components.Segmented
import com.teddytennant.wizard.ui.components.Spinner
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun WizardSheet(onDismiss: () -> Unit, skipPartial: Boolean = true, content: @Composable () -> Unit) {
    val colors = WizardTheme.colors
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = skipPartial),
        containerColor = colors.card,
        contentColor = colors.text,
        scrimColor = colors.scrim,
        shape = RoundedCornerShape(topStart = 24.dp, topEnd = 24.dp),
        dragHandle = {
            Box(Modifier.padding(top = 10.dp, bottom = 6.dp).size(width = 36.dp, height = 4.dp).clip(PillShape).background(colors.borderStrong))
        },
    ) {
        Box(Modifier.navigationBarsPadding()) { content() }
    }
}

/** Model, reasoning effort and mode, from the session's ACP config options. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun OptionsSheetContent(options: List<ConfigOption>, enabled: Boolean, onPick: (String, String) -> Unit) {
    val colors = WizardTheme.colors
    Column(
        Modifier.verticalScroll(rememberScrollState()).padding(start = 20.dp, end = 20.dp, bottom = 24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Session", style = WizardTheme.type.title, color = colors.text, modifier = Modifier.padding(top = 8.dp))
        if (!enabled) Text("Options can change once this turn ends.", style = WizardTheme.type.small, color = colors.faint)
        if (options.isEmpty()) Text("Wizard didn't offer any options.", style = WizardTheme.type.small, color = colors.faint)
        // Model last: it's the long list.
        val ordered = options.sortedBy { if (it.category == "model" || it.id == "model") 1 else 0 }
        ordered.forEach { option ->
            SectionLabel(option.name, Modifier.padding(top = 12.dp))
            when {
                option.category == "model" || option.id == "model" || option.choices.size > 5 -> Column(
                    Modifier.fillMaxWidth().clip(RoundedCornerShape(16.dp)).border(1.dp, colors.border, RoundedCornerShape(16.dp)),
                ) {
                    option.choices.forEachIndexed { i, choice ->
                        if (i > 0) Hairline(inset = 50.dp)
                        val on = choice.value == option.currentValue
                        Row(
                            Modifier
                                .fillMaxWidth()
                                .clickable(enabled = enabled, role = Role.RadioButton) { onPick(option.id, choice.value) }
                                .semantics { selected = on }
                                .heightIn(min = 56.dp)
                                .padding(horizontal = 16.dp, vertical = 10.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Radio(on)
                            Column(Modifier.weight(1f).padding(start = 14.dp)) {
                                Text(choice.name, style = WizardTheme.type.body, color = colors.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
                                choice.description?.let { Text(it, style = WizardTheme.type.small, color = colors.faint, maxLines = 1, overflow = TextOverflow.Ellipsis) }
                            }
                        }
                    }
                }
                option.choices.size <= 3 -> {
                    Segmented(option.choices.map { it.value to it.name }, option.currentValue, { if (enabled) onPick(option.id, it) })
                    option.currentChoice?.description?.let { Text(it, style = WizardTheme.type.small, color = colors.faint) }
                }
                else -> {
                    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        option.choices.forEach { choice ->
                            val on = choice.value == option.currentValue
                            Box(
                                Modifier
                                    .heightIn(min = 40.dp)
                                    .clip(PillShape)
                                    .background(if (on) colors.solid else colors.input)
                                    .border(1.dp, if (on) colors.solid else colors.border, PillShape)
                                    .clickable(enabled = enabled, role = Role.RadioButton) { onPick(option.id, choice.value) }
                                    .semantics { selected = on }
                                    .padding(horizontal = 16.dp),
                                contentAlignment = Alignment.Center,
                            ) {
                                Text(choice.name, style = WizardTheme.type.label, color = if (on) colors.onSolid else colors.muted)
                            }
                        }
                    }
                    option.currentChoice?.description?.let { Text(it, style = WizardTheme.type.small, color = colors.faint) }
                }
            }
        }
    }
}

/** The short summary on the composer: `grok-4.6 · High · Genie`. */
fun optionsSummary(options: List<ConfigOption>): String? {
    if (options.isEmpty()) return null
    val model = options.firstOrNull { it.category == "model" || it.id == "model" }
    val parts = mutableListOf<String>()
    model?.let { m ->
        // Wizard and Pi ids are `provider/model`; Claude's are aliases with a display name.
        parts += if ('/' in m.currentValue) m.currentValue.substringAfterLast('/') else (m.currentChoice?.name ?: m.currentValue).substringBefore(" (")
    }
    options.filter { it !== model }.forEach { o ->
        val name = o.currentChoice?.name ?: o.currentValue
        if (o.currentValue != "default") parts += name
    }
    return parts.joinToString("  ·  ")
}

data class BrowseState(
    val path: String = "~",
    val listing: RemoteScripts.Listing? = null,
    val loading: Boolean = true,
    val error: String? = null,
)

@Composable
fun BrowseSheetContent(state: BrowseState, home: String?, onOpen: (String) -> Unit, onUp: () -> Unit, onPick: (String) -> Unit, pickLabel: String = "Use this folder") {
    val colors = WizardTheme.colors
    val listState = rememberLazyListState()
    LaunchedEffect(state.listing?.path) { listState.scrollToItem(0) }
    Column(Modifier.padding(bottom = 16.dp)) {
        Text("Pick a folder", style = WizardTheme.type.title, color = colors.text, modifier = Modifier.padding(start = 20.dp, end = 20.dp, top = 8.dp))
        Text(
            state.listing?.path?.let { Format.path(it, home) } ?: state.path,
            style = WizardTheme.type.monoSmall,
            color = colors.faint,
            modifier = Modifier.padding(start = 20.dp, end = 20.dp, top = 4.dp, bottom = 12.dp),
        )
        Box(Modifier.fillMaxWidth().heightIn(min = 200.dp, max = 420.dp)) {
            when {
                state.loading -> Box(Modifier.fillMaxWidth().height(200.dp), contentAlignment = Alignment.Center) { com.teddytennant.wizard.ui.life.LifeIndicator(size = 32.dp, cells = 10, contentDescription = "Loading folders") }
                state.error != null -> Text(state.error, style = WizardTheme.type.small, color = colors.danger, modifier = Modifier.padding(20.dp))
                else -> LazyColumn(state = listState) {
                    if (state.listing?.path != "/") {
                        item { ListRow("..", icon = R.drawable.ic_alt_arrow_left, onClick = onUp) }
                    }
                    items(state.listing?.dirs.orEmpty(), key = { it.name }) { dir ->
                        ListRow(
                            dir.name,
                            icon = if (dir.isRepo) R.drawable.ic_git_branch else R.drawable.ic_folder,
                            iconTint = if (dir.isRepo) colors.text else colors.faint,
                            onClick = { onOpen(state.listing!!.path.trimEnd('/') + "/" + dir.name) },
                        ) { WizardIcon(R.drawable.ic_alt_arrow_right, null, size = 16.dp, tint = colors.faint) }
                    }
                    if (state.listing?.dirs?.isEmpty() == true) {
                        item { Text("No folders here.", style = WizardTheme.type.small, color = colors.faint, modifier = Modifier.padding(20.dp)) }
                    }
                }
            }
        }
        Spacer(Modifier.height(12.dp))
        WizardButton(
            pickLabel,
            { state.listing?.path?.let(onPick) },
            Modifier.fillMaxWidth().padding(horizontal = 20.dp),
            enabled = state.listing != null,
        )
    }
}

data class InstallState(val agent: Agent, val lines: List<String> = emptyList(), val running: Boolean = false, val done: Boolean? = null)

private fun installLine(agent: Agent) = when (agent) {
    Agent.Wizard -> "curl -fsSL ${RemoteScripts.INSTALL_URL} | bash"
    Agent.Pi -> "curl -fsSL https://pi.dev/install.sh | sh\nnpm install pi-acp@${RemoteScripts.PI_ACP_VERSION}"
    Agent.ClaudeCode -> "curl -fsSL https://claude.ai/install.sh | bash"
}

@Composable
fun InstallSheetContent(machineName: String, state: InstallState, onInstall: () -> Unit, onClose: () -> Unit) {
    val colors = WizardTheme.colors
    val listState = rememberLazyListState()
    LaunchedEffect(state.lines.size) { if (state.lines.isNotEmpty()) listState.scrollToItem(state.lines.lastIndex) }
    Column(Modifier.padding(start = 20.dp, end = 20.dp, bottom = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Row(Modifier.padding(top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            AgentTile(state.agent, size = 32.dp)
            Text("Install ${state.agent.displayName} on $machineName", style = WizardTheme.type.title, color = colors.text, modifier = Modifier.padding(start = 12.dp))
        }
        Text(
            when (state.agent) {
                Agent.Pi -> "Installs the pi CLI if it's missing, then the pi-acp adapter Wizard GUI uses, into ~/.zeron/adapters:"
                else -> "Runs the official installer over SSH, into your home directory:"
            },
            style = WizardTheme.type.small,
            color = colors.muted,
        )
        Box(Modifier.fillMaxWidth().clip(FieldShape).background(colors.code).border(1.dp, colors.border, FieldShape).padding(12.dp)) {
            Text(installLine(state.agent), style = WizardTheme.type.monoSmall, color = colors.faint)
        }
        if (state.running && state.lines.isEmpty()) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                com.teddytennant.wizard.ui.life.LifeIndicator(size = 24.dp, cells = 10, seed = com.teddytennant.wizard.ui.life.LifeSeed.Toad, contentDescription = "Installing")
                Text("Starting the installer", style = WizardTheme.type.small, color = colors.muted, modifier = Modifier.padding(start = 12.dp))
            }
        }
        if (state.lines.isNotEmpty()) {
            LazyColumn(
                state = listState,
                modifier = Modifier.fillMaxWidth().heightIn(min = 120.dp, max = 300.dp).clip(FieldShape).background(colors.code).border(1.dp, colors.border, FieldShape),
                contentPadding = PaddingValues(12.dp),
            ) {
                items(state.lines.size) { i ->
                    Text(state.lines[i], style = WizardTheme.type.monoSmall, color = colors.text, softWrap = true)
                }
            }
        }
        when (state.done) {
            true -> Text("${state.agent.displayName} is installed.", style = WizardTheme.type.label, color = colors.success)
            false -> Text("The installer failed. Its output is above.", style = WizardTheme.type.label, color = colors.danger)
            null -> Unit
        }
        if (state.done == null) {
            WizardButton(if (state.running) "Installing" else "Install", onInstall, Modifier.fillMaxWidth(), busy = state.running)
        } else {
            WizardButton("Done", onClose, Modifier.fillMaxWidth(), kind = if (state.done) ButtonKind.Solid else ButtonKind.Quiet)
        }
    }
}

@Composable
private fun SheetTitle(text: String) {
    Text(text, style = WizardTheme.type.title, color = WizardTheme.colors.text, modifier = Modifier.padding(start = 20.dp, end = 20.dp, top = 8.dp, bottom = 12.dp))
}

/** Which agent a new chat runs, with what each machine has installed. */
@Composable
fun AgentSheetContent(selected: Agent, status: MachineStatus?, machineName: String?, onPick: (Agent) -> Unit, onInstall: (Agent) -> Unit) {
    val colors = WizardTheme.colors
    Column(Modifier.padding(bottom = 16.dp)) {
        SheetTitle("Agent")
        Column(Modifier.padding(horizontal = 20.dp).fillMaxWidth().clip(RoundedCornerShape(16.dp)).border(1.dp, colors.border, RoundedCornerShape(16.dp))) {
            Agent.entries.forEachIndexed { i, agent ->
                if (i > 0) Hairline(inset = 66.dp)
                val availability = status?.agent(agent)
                val online = status?.reach == Reach.Online
                val detail = when {
                    status == null || machineName == null -> null
                    !online -> "Connect to $machineName to check"
                    availability?.ready == true -> "Ready" + (availability.version?.let { "  ·  " + versionOnly(it) } ?: "")
                    availability?.missing != null -> availability.missing
                    else -> "Not installed on $machineName"
                }
                val on = agent == selected
                Row(
                    Modifier
                        .fillMaxWidth()
                        .clickable(role = Role.RadioButton) { onPick(agent) }
                        .semantics { this.selected = on }
                        .heightIn(min = 64.dp)
                        .padding(start = 14.dp, end = 8.dp, top = 10.dp, bottom = 10.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    AgentTile(agent)
                    Column(Modifier.weight(1f).padding(start = 14.dp)) {
                        Text(agent.displayName, style = WizardTheme.type.body, color = colors.text)
                        detail?.let { Text(it, style = WizardTheme.type.small, color = colors.faint, maxLines = 1, overflow = TextOverflow.Ellipsis) }
                    }
                    if (online && availability?.ready == false) {
                        TextAction("Install", { onInstall(agent) })
                    } else {
                        Box(Modifier.padding(end = 8.dp)) { Radio(on) }
                    }
                }
            }
        }
    }
}

private fun versionOnly(line: String): String =
    Regex("\\d+(\\.\\d+)+").find(line)?.value ?: line

/** Which machine a new chat runs on. */
@Composable
fun MachineSheetContent(
    machines: List<Pair<Machine, MachineStatus>>,
    selected: String?,
    onPick: (String) -> Unit,
    onAdd: () -> Unit,
    onManage: () -> Unit,
) {
    val colors = WizardTheme.colors
    Column(Modifier.padding(bottom = 16.dp)) {
        SheetTitle("Machine")
        if (machines.isNotEmpty()) {
            Column(Modifier.padding(horizontal = 20.dp).fillMaxWidth().clip(RoundedCornerShape(16.dp)).border(1.dp, colors.border, RoundedCornerShape(16.dp))) {
                machines.forEachIndexed { i, (machine, status) ->
                    if (i > 0) Hairline(inset = 50.dp)
                    val (dot, words, pulsing) = statusLine(status)
                    val on = machine.id == selected
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .clickable(role = Role.RadioButton) { onPick(machine.id) }
                            .semantics { this.selected = on }
                            .heightIn(min = 60.dp)
                            .padding(horizontal = 16.dp, vertical = 10.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Box(Modifier.size(20.dp), contentAlignment = Alignment.Center) { StatusDot(dot, pulsing = pulsing) }
                        Column(Modifier.weight(1f).padding(start = 14.dp)) {
                            Text(machine.name, style = WizardTheme.type.body, color = colors.text)
                            Text(machine.address + "  ·  " + words, style = WizardTheme.type.small, color = colors.faint, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                        Radio(on)
                    }
                }
            }
        }
        Row(Modifier.padding(start = 20.dp, end = 20.dp, top = 12.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            WizardButton("Add machine", onAdd, kind = ButtonKind.Quiet, icon = R.drawable.ic_plus)
            if (machines.isNotEmpty()) WizardButton("Manage", onManage, kind = ButtonKind.Quiet)
        }
    }
}

/** Where a new chat runs: recent folders on the machine, or browse. */
@Composable
fun FolderSheetContent(recent: List<String>, selected: String?, home: String?, onPick: (String) -> Unit, onBrowse: () -> Unit) {
    val colors = WizardTheme.colors
    Column(Modifier.padding(bottom = 16.dp)) {
        SheetTitle("Folder")
        val dirs = (listOfNotNull(home) + recent).distinct()
        Column(Modifier.padding(horizontal = 20.dp).fillMaxWidth().clip(RoundedCornerShape(16.dp)).border(1.dp, colors.border, RoundedCornerShape(16.dp))) {
            dirs.forEachIndexed { i, dir ->
                if (i > 0) Hairline(inset = 50.dp)
                val on = dir == selected
                ListRow(
                    if (dir == home) "Home" else Format.project(dir),
                    subtitle = Format.path(dir, home),
                    icon = if (dir == home) R.drawable.ic_monitor else R.drawable.ic_folder,
                    monoSubtitle = true,
                    onClick = { onPick(dir) },
                ) { Radio(on) }
            }
            if (dirs.isNotEmpty()) Hairline(inset = 50.dp)
            ListRow("Browse folders", icon = R.drawable.ic_magnifer, onClick = onBrowse) {
                WizardIcon(R.drawable.ic_alt_arrow_right, null, size = 18.dp, tint = colors.faint)
            }
        }
    }
}
