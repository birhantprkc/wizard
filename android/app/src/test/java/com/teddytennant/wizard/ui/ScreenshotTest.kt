package com.teddytennant.wizard.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import app.cash.paparazzi.DeviceConfig
import app.cash.paparazzi.Paparazzi
import com.teddytennant.wizard.R
import com.teddytennant.wizard.data.Settings
import com.teddytennant.wizard.notify.NotificationPolicy
import com.teddytennant.wizard.notify.RunningTurn
import com.teddytennant.wizard.notify.TurnOutcome
import com.teddytennant.wizard.session.Transcript
import com.teddytennant.wizard.ui.components.PillShape
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.screens.ChatContent
import com.teddytennant.wizard.ui.screens.DialogSurface
import com.teddytennant.wizard.ui.screens.EditMachineContent
import com.teddytennant.wizard.ui.screens.HostKeyTrustContent
import com.teddytennant.wizard.ui.screens.MachineContent
import com.teddytennant.wizard.ui.screens.MachineFormState
import com.teddytennant.wizard.ui.screens.MachineScreenState
import com.teddytennant.wizard.ui.screens.MachinesContent
import com.teddytennant.wizard.ui.screens.OptionsSheetContent
import com.teddytennant.wizard.ui.screens.SettingsContent
import com.teddytennant.wizard.ui.theme.GeistMono
import com.teddytennant.wizard.ui.theme.WizardTheme
import org.junit.Rule
import org.junit.Test

/**
 * Renders the main screens to PNGs (`./gradlew recordPaparazziDebug`). Sheets
 * and dialogs are drawn in place, since layoutlib has no window for a popup.
 */
class ScreenshotTest {
    @get:Rule
    val paparazzi = Paparazzi(deviceConfig = DeviceConfig.PIXEL_6, theme = "android:Theme.Material.NoActionBar", maxPercentDifference = 0.5)

    private fun shot(name: String, dark: Boolean = true, content: @Composable () -> Unit) =
        paparazzi.snapshot(name) { WizardTheme(dark = dark) { Box(Modifier.fillMaxSize().background(WizardTheme.colors.background)) { content() } } }

    @Test fun machines() = shot("machines") {
        MachinesContent(Fixtures.cards, false, {}, {}, {}, {}, {}, {})
    }

    @Test fun machinesEmpty() = shot("machines_empty") {
        MachinesContent(emptyList(), false, {}, {}, {}, {}, {}, {})
    }

    @Test fun addMachine() = shot("add_machine") {
        EditMachineContent(
            editing = false,
            form = MachineFormState(name = "devbox", host = "devbox.local", user = "teddy", keyId = "k1"),
            keys = listOf(Fixtures.key, Fixtures.laptopKey),
            onChange = {}, onGenerateKey = {}, onImportKey = {}, onCopyKey = {}, onShareKey = {}, onSave = {}, onBack = {},
        )
    }

    @Test fun machine() = shot("machine") {
        MachineContent(
            MachineScreenState(Fixtures.devbox, Fixtures.cards[0].status, Fixtures.sessions),
            {}, {}, {}, {}, {}, {}, {}, {}, {},
        )
    }

    @Test fun hostKey() = shot("host_key") {
        MachineContent(MachineScreenState(Fixtures.devbox, com.teddytennant.wizard.session.MachineStatus()), {}, {}, {}, {}, {}, {}, {}, {}, {})
        Box(Modifier.fillMaxSize().background(WizardTheme.colors.scrim), contentAlignment = Alignment.Center) {
            DialogSurface { HostKeyTrustContent(Fixtures.presented, {}, {}) }
        }
    }

    @Test fun chat() = shot("chat") {
        ChatContent(Fixtures.chat, "devbox", Fixtures.chat.cwd, false, null, "", {}, {}, {}, {}, {}, {})
    }

    @Test fun toolCards() = shot("tool_cards") {
        val tools = Fixtures.chat.items.filterIsInstance<com.teddytennant.wizard.session.TranscriptItem.Tool>()
        Column(Modifier.padding(20.dp), verticalArrangement = androidx.compose.foundation.layout.Arrangement.spacedBy(6.dp)) {
            com.teddytennant.wizard.ui.screens.ToolCard(tools[0], initiallyOpen = true)
            com.teddytennant.wizard.ui.screens.ToolCard(tools[1], initiallyOpen = true)
            com.teddytennant.wizard.ui.screens.ToolCard(tools[2])
        }
    }

    @Test fun chatLight() = shot("chat_light", dark = false) {
        ChatContent(Fixtures.chat.copy(running = false, items = Fixtures.chat.items.dropLast(1)), "devbox", Fixtures.chat.cwd, false, null, "Now open a PR", {}, {}, {}, {}, {}, {})
    }

    @Test fun modelSheet() = shot("model_sheet") {
        ChatContent(Fixtures.chat.copy(running = false), "devbox", Fixtures.chat.cwd, false, null, "", {}, {}, {}, {}, {}, {})
        Box(Modifier.fillMaxSize().background(WizardTheme.colors.scrim), contentAlignment = Alignment.BottomCenter) {
            Column(
                Modifier.fillMaxWidth()
                    .clip(RoundedCornerShape(topStart = 24.dp, topEnd = 24.dp))
                    .background(WizardTheme.colors.card),
            ) {
                Box(Modifier.fillMaxWidth().padding(top = 10.dp, bottom = 6.dp), contentAlignment = Alignment.Center) {
                    Box(Modifier.size(width = 36.dp, height = 4.dp).clip(PillShape).background(WizardTheme.colors.borderStrong))
                }
                OptionsSheetContent(Fixtures.options, enabled = true, onPick = { _, _ -> })
            }
        }
    }

    @Test fun settings() = shot("settings") {
        SettingsContent(Settings(), 2, 3, true, "0.1.0", {}, {}, {}, {}, {}, {}, {})
    }

    /** What the ongoing and finished notifications say, drawn as a shade. */
    @Test fun notifications() = shot("notifications") {
        val turn = RunningTurn("m1", "devbox", "s", "Make the ACP session list page past 50 entries", "/home/teddy/code/wizard")
        val ongoing = NotificationPolicy.ongoing(listOf(turn))
        val done = NotificationPolicy.turnEnded("devbox", TurnOutcome.Finished, Transcript.lastReplyFirstLine(Fixtures.chat.items), null)
        val ask = NotificationPolicy.needsInput("devbox", "execute: rm -rf target/")
        Column(Modifier.fillMaxSize().background(Color(0xFF1B1B1F)).padding(horizontal = 12.dp, vertical = 48.dp)) {
            Text("12:41", color = Color(0xFFE6E1E5), fontSize = 44.sp, modifier = Modifier.padding(start = 12.dp, bottom = 24.dp))
            ShadeCard("now", ongoing.title, ongoing.body, action = "Stop", progress = true)
            Spacer(Modifier.height(8.dp))
            ShadeCard("now", done.title, done.body, sub = turn.title)
            Spacer(Modifier.height(8.dp))
            ShadeCard("2m", ask.title, ask.body)
        }
    }

    @Composable
    private fun ShadeCard(time: String, title: String, body: String, sub: String? = null, action: String? = null, progress: Boolean = false) {
        val shade = Color(0xFF2B2930)
        val text = Color(0xFFE6E1E5)
        val muted = Color(0xFFCAC4D0)
        Column(Modifier.fillMaxWidth().clip(RoundedCornerShape(24.dp)).background(shade).padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.size(22.dp).clip(CircleShape).background(Color(0xFF8B7CF6)), contentAlignment = Alignment.Center) {
                    WizardIcon(R.drawable.ic_wizard_mark, null, size = 14.dp, tint = Color.White)
                }
                Text("Wizard" + (sub?.let { " · $it" } ?: "") + " · $time", color = muted, fontSize = 12.sp, maxLines = 1, modifier = Modifier.padding(start = 8.dp))
            }
            Text(title, color = text, fontSize = 15.sp, modifier = Modifier.padding(top = 8.dp, start = 30.dp))
            Text(body, color = muted, fontSize = 14.sp, modifier = Modifier.padding(start = 30.dp, top = 2.dp))
            if (progress) {
                Box(Modifier.padding(start = 30.dp, top = 10.dp).fillMaxWidth().height(4.dp).clip(PillShape).background(Color(0xFF49454F))) {
                    Box(Modifier.fillMaxWidth(0.35f).height(4.dp).clip(PillShape).background(Color(0xFF8B7CF6)))
                }
            }
            if (action != null) {
                Text(action, color = Color(0xFFD0BCFF), fontSize = 14.sp, modifier = Modifier.padding(start = 30.dp, top = 12.dp))
            }
        }
    }

}
