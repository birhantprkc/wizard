package com.teddytennant.wizard.ui.screens

import androidx.compose.animation.animateContentSize
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
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.acp.PermissionRequest
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.acp.ToolContent
import com.teddytennant.wizard.acp.ToolStatus
import com.teddytennant.wizard.session.ChatState
import com.teddytennant.wizard.session.TranscriptItem
import com.teddytennant.wizard.ui.Format
import com.teddytennant.wizard.ui.components.AgentMark
import com.teddytennant.wizard.ui.components.AgentTile
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.CodeBox
import com.teddytennant.wizard.ui.components.IconAction
import com.teddytennant.wizard.ui.components.Markdown
import com.teddytennant.wizard.ui.components.PillShape
import com.teddytennant.wizard.ui.components.Spinner
import com.teddytennant.wizard.ui.components.StatusDot
import com.teddytennant.wizard.ui.components.TopBar
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

@Composable
fun ChatContent(
    state: ChatState?,
    agent: Agent,
    machineName: String,
    cwd: String,
    starting: Boolean,
    startError: String?,
    draft: String,
    onDraft: (String) -> Unit,
    onSend: () -> Unit,
    onStop: () -> Unit,
    onOptions: () -> Unit,
    onPermission: (String?) -> Unit,
    onBack: () -> Unit,
    compact: Boolean = false,
    listState: LazyListState = rememberLazyListState(),
) {
    val colors = WizardTheme.colors
    val items = state?.items.orEmpty()
    val running = state?.running == true
    Column(Modifier.fillMaxSize().background(colors.background).imePadding()) {
        TopBar(
            title = state?.title ?: "New session",
            onBack = onBack,
            subtitle = "${agent.displayName}  ·  $machineName  ·  ${Format.project(cwd)}",
            titleIcon = { AgentTile(agent, size = 32.dp) },
        ) {
            IconAction(R.drawable.ic_tuning, "Model and effort", onOptions, enabled = state != null)
        }
        Box(Modifier.weight(1f).fillMaxWidth()) {
            when {
                startError != null || state?.error != null && items.isEmpty() -> CenterNote(startError ?: state?.error.orEmpty(), isError = true)
                starting || state == null -> CenterNote("Starting a session", spinner = true)
                state.loading && items.isEmpty() -> CenterNote("Loading the transcript", spinner = true)
                items.isEmpty() -> EmptyChat(agent, cwd)
                else -> Transcript(items, running, listState, compact)
            }
        }
        state?.permission?.let { PermissionCard(agent, it.request, onPermission) }
        if (state?.error != null && items.isNotEmpty()) {
            Text(state.error, style = WizardTheme.type.small, color = colors.danger, modifier = Modifier.padding(horizontal = 20.dp, vertical = 4.dp))
        }
        Composer(
            draft = draft,
            onDraft = onDraft,
            running = running,
            enabled = state != null && !starting,
            summary = state?.options?.let(::optionsSummary),
            agentName = agent.displayName,
            onSend = onSend,
            onStop = onStop,
            onOptions = onOptions,
        )
    }
}

@Composable
private fun CenterNote(text: String, spinner: Boolean = false, isError: Boolean = false) {
    Column(Modifier.fillMaxSize().padding(32.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) {
        if (spinner) {
            Spinner(size = 20.dp)
            Spacer(Modifier.height(14.dp))
        }
        Text(text, style = WizardTheme.type.small, color = if (isError) WizardTheme.colors.danger else WizardTheme.colors.muted, textAlign = TextAlign.Center)
    }
}

@Composable
private fun EmptyChat(agent: Agent, cwd: String) {
    val colors = WizardTheme.colors
    Column(Modifier.fillMaxSize().padding(32.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) {
        AgentMark(agent, size = 28.dp, tint = colors.faint)
        Spacer(Modifier.height(16.dp))
        Text("What should ${agent.displayName} do in ${Format.project(cwd)}?", style = WizardTheme.type.heading, color = colors.muted, textAlign = TextAlign.Center)
    }
}

/** What the transcript draws: an item, or (compact mode) a run of tool calls and thinking folded into one line. */
private sealed interface Row_ {
    val key: String
    data class One(val item: TranscriptItem) : Row_ { override val key get() = item.key }
    data class Steps(override val key: String, val items: List<TranscriptItem>, val active: Boolean) : Row_
}

private fun rows(items: List<TranscriptItem>, compact: Boolean, running: Boolean): List<Row_> {
    if (!compact) return items.map { Row_.One(it) }
    val out = mutableListOf<Row_>()
    var run = mutableListOf<TranscriptItem>()
    fun flush(active: Boolean) {
        if (run.isNotEmpty()) out += Row_.Steps("steps-" + run.first().key, run, active)
        run = mutableListOf()
    }
    items.forEachIndexed { i, item ->
        if (item is TranscriptItem.Tool || item is TranscriptItem.Thinking) {
            run += item
            if (i == items.lastIndex) flush(active = running)
        } else {
            flush(active = false)
            out += Row_.One(item)
        }
    }
    return out
}

@Composable
private fun Transcript(items: List<TranscriptItem>, running: Boolean, listState: LazyListState, compact: Boolean) {
    // Follow the stream while the reader is at the bottom; stay put if they scrolled up.
    val atBottom by remember {
        derivedStateOf {
            val info = listState.layoutInfo
            val last = info.visibleItemsInfo.lastOrNull()
            last == null || last.index >= info.totalItemsCount - 2
        }
    }
    val lastSize = (items.lastOrNull() as? TranscriptItem.Agent)?.text?.length ?: 0
    LaunchedEffect(items.size, lastSize, running) {
        if (atBottom && items.isNotEmpty()) listState.scrollToItem(items.size)
    }
    LazyColumn(
        state = listState,
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(start = 20.dp, end = 20.dp, top = 8.dp, bottom = 16.dp),
    ) {
        val display = rows(items, compact, running)
        itemsIndexed(display, key = { _, it -> it.key }) { index, row ->
            val previous = display.getOrNull(index - 1)
            val item = (row as? Row_.One)?.item
            val previousItem = (previous as? Row_.One)?.item
            // Runs of tool calls sit close together; everything else breathes.
            val gap = when {
                index == 0 -> 0.dp
                item is TranscriptItem.Tool && previousItem is TranscriptItem.Tool -> 6.dp
                item is TranscriptItem.User -> if (compact) 20.dp else 28.dp
                compact -> 10.dp
                else -> 14.dp
            }
            Box(Modifier.padding(top = gap)) {
                when (row) {
                    is Row_.One -> TranscriptRow(row.item)
                    is Row_.Steps -> StepsRow(row)
                }
            }
        }
        item(key = "tail") {
            if (running) Box(Modifier.padding(top = 14.dp)) { WorkingRow() } else Spacer(Modifier.height(1.dp))
        }
    }
}

@Composable
private fun TranscriptRow(item: TranscriptItem) {
            when (item) {
                is TranscriptItem.User -> UserBubble(item.text)
                is TranscriptItem.Agent -> Markdown(item.text, Modifier.fillMaxWidth())
                is TranscriptItem.Thinking -> ThinkingRow(item.text)
                is TranscriptItem.Tool -> ToolCard(item)
                is TranscriptItem.Plan -> PlanCard(item)
                is TranscriptItem.Notice -> Text(
                    item.text,
                    style = WizardTheme.type.small,
                    color = if (item.isError) WizardTheme.colors.danger else WizardTheme.colors.faint,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp),
                )
            }
}

@Composable
private fun UserBubble(text: String) {
    val colors = WizardTheme.colors
    Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.CenterEnd) {
        Box(
            Modifier
                .widthIn(max = 320.dp)
                .clip(RoundedCornerShape(topStart = 20.dp, topEnd = 20.dp, bottomStart = 20.dp, bottomEnd = 6.dp))
                .background(colors.raised)
                .padding(horizontal = 16.dp, vertical = 11.dp),
        ) {
            Text(text, style = WizardTheme.type.body, color = colors.text)
        }
    }
}

@Composable
private fun WorkingRow() {
    val colors = WizardTheme.colors
    Row(Modifier.padding(vertical = 4.dp).semantics(mergeDescendants = true) {}, verticalAlignment = Alignment.CenterVertically) {
        StatusDot(colors.accent, pulsing = true)
        Text("Working", style = WizardTheme.type.small, color = colors.faint, modifier = Modifier.padding(start = 10.dp))
    }
}

@Composable
private fun ThinkingRow(text: String) {
    val colors = WizardTheme.colors
    var open by rememberSaveable { mutableStateOf(false) }
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .clickable(role = Role.Button, onClickLabel = if (open) "Hide thinking" else "Show thinking") { open = !open }
            .animateContentSize()
            .padding(vertical = 6.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.heightIn(min = 32.dp)) {
            WizardIcon(R.drawable.ic_magic_stick_3, null, size = 16.dp, tint = colors.faint)
            Text("Thought", style = WizardTheme.type.label, color = colors.faint, modifier = Modifier.padding(start = 8.dp))
            if (!open) {
                Text(
                    text.lineSequence().firstOrNull { it.isNotBlank() }.orEmpty().trim(),
                    style = WizardTheme.type.small,
                    color = colors.faint,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(start = 8.dp).weight(1f),
                )
            }
        }
        if (open) Text(text.trim(), style = WizardTheme.type.small, color = colors.muted, modifier = Modifier.padding(start = 24.dp, top = 4.dp))
    }
}

@Composable
private fun StepsRow(row: Row_.Steps) {
    val colors = WizardTheme.colors
    var open by rememberSaveable(row.key) { mutableStateOf(false) }
    val tools = row.items.filterIsInstance<TranscriptItem.Tool>()
    val failed = tools.count { it.status == ToolStatus.Failed }
    val summary = buildString {
        append(
            when {
                tools.isEmpty() -> "Thought"
                tools.size == 1 -> "1 step"
                else -> "${tools.size} steps"
            },
        )
        if (failed > 0) append(", $failed failed")
        if (row.active) tools.lastOrNull()?.let { append("  ·  " + it.title) }
    }
    Column(Modifier.fillMaxWidth().animateContentSize()) {
        Row(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(12.dp))
                .clickable(role = Role.Button, onClickLabel = if (open) "Collapse steps" else "Expand steps") { open = !open }
                .heightIn(min = 40.dp)
                .padding(vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            when {
                row.active -> Spinner(size = 14.dp)
                tools.isEmpty() -> WizardIcon(R.drawable.ic_magic_stick_3, null, size = 15.dp, tint = colors.faint)
                else -> WizardIcon(R.drawable.ic_widget, null, size = 15.dp, tint = colors.faint)
            }
            Text(summary, style = WizardTheme.type.small, color = colors.faint, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f).padding(start = 10.dp))
            WizardIcon(if (open) R.drawable.ic_alt_arrow_down else R.drawable.ic_alt_arrow_right, null, size = 14.dp, tint = colors.faint)
        }
        if (open) {
            Column(Modifier.padding(top = 6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                row.items.forEach { TranscriptRow(it) }
            }
        }
    }
}

private fun toolIcon(kind: String?) = when (kind) {
    "execute" -> R.drawable.ic_terminal
    "read" -> R.drawable.ic_document
    "edit", "delete", "move" -> R.drawable.ic_pen
    "search" -> R.drawable.ic_magnifer
    "fetch" -> R.drawable.ic_globe
    "think" -> R.drawable.ic_magic_stick_3
    else -> R.drawable.ic_widget
}

@Composable
fun ToolCard(tool: TranscriptItem.Tool, initiallyOpen: Boolean = false) {
    val colors = WizardTheme.colors
    var open by rememberSaveable(tool.key) { mutableStateOf(initiallyOpen) }
    val shape = RoundedCornerShape(14.dp)
    val statusWords = when (tool.status) {
        ToolStatus.Pending, ToolStatus.Running -> "running"
        ToolStatus.Completed -> "done"
        ToolStatus.Failed -> "failed"
    }
    Column(
        Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(colors.card)
            .border(1.dp, colors.border, shape)
            .animateContentSize(),
    ) {
        Row(
            Modifier
                .fillMaxWidth()
                .clickable(role = Role.Button, onClickLabel = if (open) "Collapse" else "Expand") { open = !open }
                .semantics { stateDescription = statusWords }
                .heightIn(min = 48.dp)
                .padding(start = 14.dp, end = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            WizardIcon(toolIcon(tool.kind), null, size = 17.dp, tint = colors.muted)
            Text(
                tool.title,
                style = WizardTheme.type.mono,
                color = colors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f).padding(start = 10.dp, end = 10.dp),
            )
            when (tool.status) {
                ToolStatus.Pending, ToolStatus.Running -> Spinner(size = 14.dp)
                ToolStatus.Completed -> WizardIcon(R.drawable.ic_check, "Done", size = 16.dp, tint = colors.faint)
                ToolStatus.Failed -> WizardIcon(R.drawable.ic_close, "Failed", size = 16.dp, tint = colors.danger)
            }
        }
        if (open) {
            Column(Modifier.padding(start = 12.dp, end = 12.dp, bottom = 12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                tool.input?.let {
                    Text("Input", style = WizardTheme.type.section, color = colors.faint)
                    CodeBox(it, maxLines = 30)
                }
                if (tool.output.isNotEmpty()) {
                    Text("Output", style = WizardTheme.type.section, color = colors.faint)
                    tool.output.forEach { content ->
                        when (content) {
                            is ToolContent.Text -> CodeBox(content.text.ifEmpty { "(no output)" }, maxLines = 60)
                            is ToolContent.Diff -> DiffBox(content)
                        }
                    }
                }
                if (tool.input == null && tool.output.isEmpty()) {
                    Text(if (tool.status == ToolStatus.Running) "Still running." else "No details.", style = WizardTheme.type.small, color = colors.faint)
                }
            }
        }
    }
}

@Composable
private fun DiffBox(diff: ToolContent.Diff) {
    val colors = WizardTheme.colors
    val old = diff.oldText?.lines().orEmpty().map { "- $it" }
    val new = diff.newText.lines().map { "+ $it" }
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Text(diff.path, style = WizardTheme.type.monoSmall, color = colors.muted)
        if (old.isNotEmpty()) CodeBox(old.joinToString("\n"), maxLines = 30, color = colors.danger)
        CodeBox(new.joinToString("\n"), maxLines = 30, color = colors.success)
    }
}

@Composable
private fun PlanCard(plan: TranscriptItem.Plan) {
    val colors = WizardTheme.colors
    val shape = RoundedCornerShape(14.dp)
    Column(
        Modifier.fillMaxWidth().clip(shape).background(colors.card).border(1.dp, colors.border, shape).padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("Plan", style = WizardTheme.type.section, color = colors.faint)
        plan.entries.forEach { entry ->
            Row(verticalAlignment = Alignment.CenterVertically) {
                val done = entry.status == "completed"
                val active = entry.status == "in_progress"
                Box(Modifier.size(16.dp), contentAlignment = Alignment.Center) {
                    when {
                        done -> WizardIcon(R.drawable.ic_check, "Done", size = 15.dp, tint = colors.success)
                        active -> StatusDot(colors.accent, pulsing = true)
                        else -> Box(Modifier.size(7.dp).clip(CircleShape).border(1.dp, colors.faint, CircleShape))
                    }
                }
                Text(entry.content, style = WizardTheme.type.small, color = if (done) colors.faint else colors.text, modifier = Modifier.padding(start = 10.dp))
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun PermissionCard(agent: Agent, request: PermissionRequest, onAnswer: (String?) -> Unit) {
    val colors = WizardTheme.colors
    val shape = RoundedCornerShape(16.dp)
    Column(
        Modifier
            .padding(horizontal = 16.dp, vertical = 8.dp)
            .fillMaxWidth()
            .clip(shape)
            .background(colors.card)
            .border(1.dp, colors.warning.copy(alpha = 0.5f), shape)
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            WizardIcon(R.drawable.ic_bell, null, size = 18.dp, tint = colors.warning)
            Text("${agent.displayName} is asking", style = WizardTheme.type.label, color = colors.text, modifier = Modifier.padding(start = 8.dp))
        }
        Text(request.title, style = if (request.options.all { it.kind == "allow_once" } && request.detail == null) WizardTheme.type.body else WizardTheme.type.mono, color = colors.text)
        request.detail?.let { CodeBox(it, maxLines = 8) }
        FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            request.options.forEach { option ->
                val reject = option.kind.startsWith("reject")
                WizardButton(option.name, { onAnswer(option.optionId) }, kind = if (reject) ButtonKind.Quiet else ButtonKind.Solid)
            }
            if (request.options.isEmpty()) WizardButton("Dismiss", { onAnswer(null) }, kind = ButtonKind.Quiet)
        }
    }
}

@Composable
private fun Composer(
    agentName: String,
    draft: String,
    onDraft: (String) -> Unit,
    running: Boolean,
    enabled: Boolean,
    summary: String?,
    onSend: () -> Unit,
    onStop: () -> Unit,
    onOptions: () -> Unit,
) {
    val colors = WizardTheme.colors
    val shape = RoundedCornerShape(24.dp)
    Column(Modifier.fillMaxWidth().navigationBarsPadding().padding(start = 12.dp, end = 12.dp, top = 4.dp, bottom = 8.dp)) {
        Column(
            Modifier.fillMaxWidth().clip(shape).background(colors.input).border(1.dp, colors.borderStrong, shape),
        ) {
            BasicTextField(
                value = draft,
                onValueChange = onDraft,
                enabled = enabled,
                textStyle = WizardTheme.type.body.copy(color = colors.text),
                cursorBrush = SolidColor(colors.accent),
                maxLines = 6,
                keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Sentences),
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 48.dp)
                    .padding(start = 18.dp, end = 18.dp, top = 14.dp, bottom = 4.dp)
                    .semantics { contentDescription = "Message" },
                decorationBox = { inner ->
                    Box {
                        if (draft.isEmpty()) Text(if (running) "$agentName is working" else "Ask $agentName", style = WizardTheme.type.body, color = colors.faint)
                        inner()
                    }
                },
            )
            Row(Modifier.fillMaxWidth().padding(start = 6.dp, end = 6.dp, bottom = 6.dp), verticalAlignment = Alignment.CenterVertically) {
                Row(
                    Modifier
                        .weight(1f)
                        .heightIn(min = 48.dp)
                        .clip(PillShape)
                        .clickable(enabled = enabled && summary != null, role = Role.Button, onClickLabel = "Change model and effort", onClick = onOptions)
                        .padding(horizontal = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(summary ?: "", style = WizardTheme.type.label, color = colors.muted, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
                    if (summary != null) WizardIcon(R.drawable.ic_alt_arrow_down, null, size = 14.dp, tint = colors.faint, modifier = Modifier.padding(start = 4.dp))
                }
                Spacer(Modifier.width(8.dp))
                val canSend = enabled && draft.isNotBlank() && !running
                Box(
                    Modifier
                        .size(48.dp)
                        .clip(CircleShape)
                        .clickable(enabled = running || canSend, role = Role.Button, onClick = if (running) onStop else onSend)
                        .semantics { contentDescription = if (running) "Stop" else "Send" },
                    contentAlignment = Alignment.Center,
                ) {
                    Box(
                        Modifier.size(36.dp).clip(CircleShape).background(if (running || canSend) colors.solid else colors.raised),
                        contentAlignment = Alignment.Center,
                    ) {
                        if (running) {
                            WizardIcon(R.drawable.ic_stop, null, size = 12.dp, tint = colors.onSolid)
                        } else {
                            WizardIcon(R.drawable.ic_arrow_up, null, size = 18.dp, tint = if (canSend) colors.onSolid else colors.faint)
                        }
                    }
                }
            }
        }
    }
}
