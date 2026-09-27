package com.teddytennant.wizard.ui.screens

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.data.RecentChat
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.ui.Format
import com.teddytennant.wizard.ui.components.AgentMark
import com.teddytennant.wizard.ui.components.AgentTile
import com.teddytennant.wizard.ui.components.Chip
import com.teddytennant.wizard.ui.components.Hairline
import com.teddytennant.wizard.ui.components.IconAction
import com.teddytennant.wizard.ui.components.Panel
import com.teddytennant.wizard.ui.components.SectionLabel
import com.teddytennant.wizard.ui.components.Spinner
import com.teddytennant.wizard.ui.components.StatusDot
import com.teddytennant.wizard.ui.components.WandMark
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme
import java.time.Instant

/** One row of the recent list, with what it needs from outside the chat. */
data class RecentRow(val chat: RecentChat, val machineName: String, val running: Boolean)

data class HomeState(
    val machine: Machine?,
    val status: MachineStatus,
    val agent: Agent,
    val cwd: String?,
    val recents: List<RecentRow>,
    val refreshing: Boolean,
    val hasMachines: Boolean,
    val artwork: Boolean,
)

@Composable
fun HomeContent(
    state: HomeState,
    draft: String,
    onDraft: (String) -> Unit,
    onSend: () -> Unit,
    onAgent: () -> Unit,
    onMachine: () -> Unit,
    onFolder: () -> Unit,
    onRecent: (RecentChat) -> Unit,
    onSettings: () -> Unit,
    now: Instant = Instant.now(),
) {
    val colors = WizardTheme.colors
    val showArt = state.artwork
    BoxWithConstraints(Modifier.fillMaxSize().background(colors.background)) {
        val heroHeight = (maxHeight * 0.52f).coerceIn(300.dp, 480.dp)
        if (showArt) Starship(Modifier.fillMaxWidth().height(heroHeight))
        LazyColumn(
            Modifier.fillMaxSize().imePadding(),
            contentPadding = PaddingValues(bottom = 24.dp),
        ) {
            item(key = "bar") {
                Row(
                    Modifier.fillMaxWidth().statusBarsPadding().padding(start = 20.dp, end = 8.dp, top = 4.dp).height(56.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    val onArt = if (showArt) Color.White else colors.text
                    WandMark(size = 20.dp, tint = onArt)
                    Spacer(Modifier.width(10.dp))
                    Text("Wizard", style = WizardTheme.type.heading, color = onArt, modifier = Modifier.weight(1f))
                    if (state.refreshing) Spinner(size = 14.dp, color = if (showArt) Color.White.copy(alpha = 0.8f) else colors.faint)
                    IconAction(R.drawable.ic_settings, "Settings", onSettings, tint = onArt)
                }
            }
            item(key = "space") { Spacer(Modifier.height(if (showArt) heroHeight - 196.dp else 96.dp)) }
            item(key = "composer") {
                NewChatComposer(state, draft, onDraft, onSend, onAgent, onMachine, onFolder, Modifier.padding(horizontal = 14.dp))
            }
            item(key = "recent-label") {
                SectionLabel("Recent", Modifier.padding(start = 20.dp, end = 20.dp, top = 28.dp, bottom = 8.dp))
            }
            if (state.recents.isEmpty()) {
                item(key = "recent-empty") {
                    Text(
                        if (state.hasMachines) "Chats from every machine and agent show up here, newest first." else "Add a machine to start. Your chats will collect here.",
                        style = WizardTheme.type.small,
                        color = colors.faint,
                        modifier = Modifier.padding(horizontal = 20.dp),
                    )
                }
            } else {
                item(key = "recent") {
                    Panel(Modifier.padding(horizontal = 14.dp), padding = PaddingValues(0.dp)) {
                        state.recents.forEachIndexed { i, row ->
                            if (i > 0) Hairline(inset = 66.dp)
                            RecentChatRow(row, now) { onRecent(row.chat) }
                        }
                    }
                }
            }
            item(key = "end") { Spacer(Modifier.navigationBarsPadding()) }
        }
    }
}

/** The desktop's new-chat artwork, fading from the top into the page. */
@Composable
fun Starship(modifier: Modifier = Modifier) {
    val bg = WizardTheme.colors.background
    Box(modifier) {
        Image(
            painterResource(R.drawable.wizard_starship),
            contentDescription = null,
            contentScale = ContentScale.Crop,
            alignment = Alignment.TopCenter,
            modifier = Modifier.fillMaxSize(),
        )
        Box(
            Modifier.fillMaxSize().background(
                Brush.verticalGradient(
                    0f to Color.Black.copy(alpha = 0.38f),
                    0.22f to bg.copy(alpha = 0.10f),
                    0.55f to bg.copy(alpha = 0.45f),
                    0.82f to bg.copy(alpha = 0.88f),
                    1f to bg,
                ),
            ),
        )
    }
}

@Composable
private fun NewChatComposer(
    state: HomeState,
    draft: String,
    onDraft: (String) -> Unit,
    onSend: () -> Unit,
    onAgent: () -> Unit,
    onMachine: () -> Unit,
    onFolder: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = WizardTheme.colors
    val shape = RoundedCornerShape(24.dp)
    val canSend = draft.isNotBlank()
    Column(
        modifier
            .fillMaxWidth()
            .clip(shape)
            .background(colors.card.copy(alpha = if (colors.isDark) 0.94f else 0.97f))
            .border(1.dp, colors.borderStrong, shape),
    ) {
        BasicTextField(
            value = draft,
            onValueChange = onDraft,
            textStyle = WizardTheme.type.body.copy(color = colors.text, fontSize = WizardTheme.type.heading.fontSize),
            cursorBrush = SolidColor(colors.accent),
            minLines = 2,
            maxLines = 8,
            keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Sentences),
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 72.dp)
                .padding(start = 18.dp, end = 18.dp, top = 16.dp, bottom = 4.dp)
                .semantics { contentDescription = "Message" },
            decorationBox = { inner ->
                Box {
                    if (draft.isEmpty()) Text("Do anything…", style = WizardTheme.type.body.copy(fontSize = WizardTheme.type.heading.fontSize), color = colors.faint)
                    inner()
                }
            },
        )
        Row(Modifier.fillMaxWidth().padding(start = 8.dp, end = 6.dp, bottom = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            Row(
                Modifier.weight(1f).horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Chip(state.agent.displayName, onAgent, contentDescription = "Agent: ${state.agent.displayName}", leading = { AgentMark(state.agent, size = 14.dp) })
                val machine = state.machine
                if (machine == null) {
                    Chip("Add machine", onMachine, leading = { WizardIcon(R.drawable.ic_plus, null, size = 14.dp, tint = colors.text) })
                } else {
                    val (dot, _, pulsing) = statusLine(state.status)
                    Chip(machine.name, onMachine, contentDescription = "Machine: ${machine.name}", leading = { StatusDot(dot, pulsing = pulsing) })
                    val folder = state.cwd?.let { Format.project(it).ifEmpty { "/" } } ?: "Home"
                    Chip(folder, onFolder, contentDescription = "Folder: $folder", leading = { WizardIcon(R.drawable.ic_folder, null, size = 14.dp, tint = colors.muted) })
                }
            }
            Spacer(Modifier.width(6.dp))
            Box(
                Modifier
                    .size(48.dp)
                    .clip(CircleShape)
                    .then(if (canSend) Modifier.clickableButton(onSend) else Modifier)
                    .semantics { contentDescription = "Send" },
                contentAlignment = Alignment.Center,
            ) {
                Box(
                    Modifier.size(36.dp).clip(CircleShape).background(if (canSend) colors.solid else colors.raised),
                    contentAlignment = Alignment.Center,
                ) {
                    WizardIcon(R.drawable.ic_arrow_up, null, size = 18.dp, tint = if (canSend) colors.onSolid else colors.faint)
                }
            }
        }
    }
}

@Composable
fun RecentChatRow(row: RecentRow, now: Instant, onClick: () -> Unit) {
    val colors = WizardTheme.colors
    val chat = row.chat
    Row(
        Modifier
            .fillMaxWidth()
            .clickableButton(onClick)
            .heightIn(min = 64.dp)
            .padding(horizontal = 14.dp, vertical = 10.dp)
            .semantics(mergeDescendants = true) {},
        verticalAlignment = Alignment.CenterVertically,
    ) {
        AgentTile(chat.agent)
        Column(Modifier.weight(1f).padding(start = 14.dp)) {
            Text(chat.title ?: "Untitled chat", style = WizardTheme.type.body, color = colors.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(
                listOf(row.machineName, Format.project(chat.cwd), Format.ago(Instant.ofEpochMilli(chat.updatedAt), now))
                    .filter { it.isNotBlank() }.joinToString("  ·  "),
                style = WizardTheme.type.small,
                color = colors.faint,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        if (row.running) {
            Spacer(Modifier.width(10.dp))
            StatusDot(colors.accent, pulsing = true)
        }
    }
}

fun Modifier.clickableButton(onClick: () -> Unit): Modifier =
    this.clickable(role = Role.Button, onClick = onClick)
