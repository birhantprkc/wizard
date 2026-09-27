package com.teddytennant.wizard.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Text
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.data.Machine
import com.teddytennant.wizard.session.MachineStatus
import com.teddytennant.wizard.session.Reach
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.IconAction
import com.teddytennant.wizard.ui.components.TopBar
import com.teddytennant.wizard.ui.components.Panel
import com.teddytennant.wizard.ui.components.StatusDot
import com.teddytennant.wizard.ui.components.WandMark
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

data class MachineCardModel(val machine: Machine, val status: MachineStatus)

/** The dot color and the words for a machine's state. */
@Composable
fun statusLine(status: MachineStatus): Triple<Color, String, Boolean> {
    val c = WizardTheme.colors
    return when (status.reach) {
        Reach.Unknown -> Triple(c.faint, "Not checked yet", false)
        Reach.Checking -> Triple(c.muted, "Connecting", true)
        Reach.Online -> {
            val ready = status.readyAgents
            when {
                ready.isEmpty() -> Triple(c.warning, "Online, no agents installed", false)
                else -> Triple(
                    c.success,
                    buildString {
                        append("Online · ")
                        append(ready.joinToString(", ") { it.displayName })
                        if (status.running > 0) append(" · ${status.running} running")
                    },
                    false,
                )
            }
        }
        Reach.Offline -> Triple(c.faint, "Offline", false)
        Reach.NeedsTrust -> Triple(c.warning, "Confirm the host key", false)
        Reach.KeyChanged -> Triple(c.danger, "Host key changed", false)
        Reach.AuthFailed -> Triple(c.danger, "Sign-in refused", false)
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MachinesContent(
    cards: List<MachineCardModel>,
    refreshing: Boolean,
    onRefresh: () -> Unit,
    onOpen: (String) -> Unit,
    onAdd: () -> Unit,
    onEdit: (String) -> Unit,
    onDelete: (String) -> Unit,
    onBack: () -> Unit,
) {
    val colors = WizardTheme.colors
    Column(Modifier.fillMaxSize().background(colors.background)) {
        TopBar("Machines", onBack) {
            IconAction(R.drawable.ic_plus, "Add machine", onAdd, tint = colors.text)
        }
        PullToRefreshBox(isRefreshing = refreshing, onRefresh = onRefresh, modifier = Modifier.fillMaxSize()) {
            if (cards.isEmpty()) {
                EmptyMachines(onAdd)
            } else {
                LazyColumn(
                    Modifier.fillMaxSize(),
                    contentPadding = PaddingValues(start = 20.dp, end = 20.dp, top = 12.dp, bottom = 32.dp),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    items(cards, key = { it.machine.id }) { card ->
                        MachineCard(card, onOpen = { onOpen(card.machine.id) }, onEdit = { onEdit(card.machine.id) }, onDelete = { onDelete(card.machine.id) })
                    }
                    item { Spacer(Modifier.navigationBarsPadding()) }
                }
            }
        }
    }
}

@Composable
private fun MachineCard(card: MachineCardModel, onOpen: () -> Unit, onEdit: () -> Unit, onDelete: () -> Unit) {
    val colors = WizardTheme.colors
    val (dot, words, pulsing) = statusLine(card.status)
    var menu by remember { mutableStateOf(false) }
    Panel(onClick = onOpen, onClickLabel = "Open ${card.machine.name}", padding = PaddingValues(start = 16.dp, top = 14.dp, end = 4.dp, bottom = 16.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(
                Modifier.size(40.dp).clip(RoundedCornerShape(11.dp)).background(colors.raised).border(1.dp, colors.border, RoundedCornerShape(11.dp)),
                contentAlignment = Alignment.Center,
            ) {
                WizardIcon(R.drawable.ic_monitor, null, size = 20.dp, tint = colors.text)
            }
            Column(Modifier.weight(1f).padding(start = 14.dp)) {
                Text(card.machine.name, style = WizardTheme.type.heading, color = colors.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(card.machine.address, style = WizardTheme.type.monoSmall, color = colors.faint, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
            Box {
                IconAction(R.drawable.ic_more_horizontal, "More for ${card.machine.name}", { menu = true })
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }, containerColor = colors.raised) {
                    DropdownMenuItem(text = { Text("Edit", style = WizardTheme.type.body) }, onClick = { menu = false; onEdit() })
                    DropdownMenuItem(text = { Text("Remove", style = WizardTheme.type.body, color = colors.danger) }, onClick = { menu = false; onDelete() })
                }
            }
        }
        Row(
            Modifier.padding(top = 14.dp).semantics(mergeDescendants = true) { contentDescription = "Status: $words" },
            verticalAlignment = Alignment.CenterVertically,
        ) {
            StatusDot(dot, pulsing = pulsing)
            Text(words, style = WizardTheme.type.small, color = colors.muted, modifier = Modifier.padding(start = 9.dp))
        }
        val message = card.status.message
        if (message != null && card.status.reach != Reach.Online && card.status.reach != Reach.Checking) {
            Text(message, style = WizardTheme.type.small, color = colors.faint, maxLines = 2, overflow = TextOverflow.Ellipsis, modifier = Modifier.padding(top = 4.dp, start = 16.dp, end = 12.dp))
        }
    }
}

@Composable
private fun EmptyMachines(onAdd: () -> Unit) {
    val colors = WizardTheme.colors
    Column(
        Modifier.fillMaxSize().padding(horizontal = 32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Box(
            Modifier.size(72.dp).clip(RoundedCornerShape(20.dp)).background(colors.card).border(1.dp, colors.border, RoundedCornerShape(20.dp)),
            contentAlignment = Alignment.Center,
        ) { WandMark(size = 30.dp) }
        Spacer(Modifier.height(24.dp))
        Text("Connect a machine", style = WizardTheme.type.title.copy(fontSize = WizardTheme.type.largeTitle.fontSize * 0.75f), color = colors.text, textAlign = TextAlign.Center)
        Spacer(Modifier.height(8.dp))
        Text(
            "Add a computer you can reach over SSH. Wizard runs there; this phone watches and steers.",
            style = WizardTheme.type.body,
            color = colors.muted,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(28.dp))
        WizardButton("Add machine", onAdd, kind = ButtonKind.Solid, icon = R.drawable.ic_plus)
        Spacer(Modifier.height(80.dp))
    }
}
