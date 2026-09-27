package com.teddytennant.wizard.ui.screens

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.agent.Agent
import com.teddytennant.wizard.data.Settings
import com.teddytennant.wizard.data.ThemeChoice
import com.teddytennant.wizard.ssh.StoredKey
import com.teddytennant.wizard.ui.components.AgentMark
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.Field
import com.teddytennant.wizard.ui.components.Hairline
import com.teddytennant.wizard.ui.components.Segmented
import com.teddytennant.wizard.ui.components.ToggleRow
import com.teddytennant.wizard.ui.components.WandMark
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

/** The desktop's first-run steps, minus the ones that install agents on this device. */
enum class OnboardingStep { Welcome, Appearance, Machine, Notifications, Done }

data class OnboardingMachine(
    val host: String = "",
    val user: String = "",
    val port: String = "22",
    val key: StoredKey? = null,
    val error: String? = null,
    val saved: Boolean = false,
) {
    val filled: Boolean get() = host.isNotBlank() && user.isNotBlank()
}

@Composable
fun OnboardingContent(
    step: OnboardingStep,
    settings: Settings,
    machine: OnboardingMachine,
    notificationsAsked: Boolean,
    onStep: (OnboardingStep) -> Unit,
    onTheme: (ThemeChoice) -> Unit,
    onCompact: (Boolean) -> Unit,
    onArtwork: (Boolean) -> Unit,
    onMachine: (OnboardingMachine) -> Unit,
    onGenerateKey: () -> Unit,
    onCopyKey: () -> Unit,
    onShareKey: () -> Unit,
    onSaveMachine: () -> Unit,
    onAllowNotifications: () -> Unit,
    onFinish: () -> Unit,
) {
    Box(Modifier.fillMaxSize().background(Color.Black)) {
        if (settings.artwork) {
            Image(
                painterResource(R.drawable.wizard_starship),
                contentDescription = null,
                contentScale = ContentScale.Crop,
                modifier = Modifier.fillMaxSize(),
            )
        }
        Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.52f)))
        Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding().imePadding()) {
            Row(Modifier.padding(start = 20.dp, top = 14.dp), verticalAlignment = Alignment.CenterVertically) {
                WandMark(size = 18.dp, tint = Color.White)
                Spacer(Modifier.width(8.dp))
                Text("Wizard", style = WizardTheme.type.label, color = Color.White)
            }
            Column(
                Modifier.weight(1f).fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp, vertical = 16.dp),
                verticalArrangement = Arrangement.Center,
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                AnimatedContent(step, transitionSpec = { fadeIn() togetherWith fadeOut() }, label = "step") { current ->
                    StepCard {
                        when (current) {
                            OnboardingStep.Welcome -> Welcome(onStart = { onStep(OnboardingStep.Appearance) }, onSkip = onFinish)
                            OnboardingStep.Appearance -> Appearance(settings, onTheme, onCompact, onArtwork, onStep)
                            OnboardingStep.Machine -> FirstMachine(machine, onMachine, onGenerateKey, onCopyKey, onShareKey, onSaveMachine, onStep)
                            OnboardingStep.Notifications -> Notifications(notificationsAsked, onAllowNotifications, onStep)
                            OnboardingStep.Done -> Done(onStep, onFinish)
                        }
                    }
                }
                Spacer(Modifier.height(18.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(6.dp), modifier = Modifier.semantics { contentDescription = "Step ${step.ordinal + 1} of ${OnboardingStep.entries.size}" }) {
                    OnboardingStep.entries.forEach {
                        Box(Modifier.size(6.dp).clip(CircleShape).background(if (it == step) Color.White else Color.White.copy(alpha = 0.35f)))
                    }
                }
            }
        }
    }
}

@Composable
private fun StepCard(content: @Composable ColumnScope.() -> Unit) {
    val colors = WizardTheme.colors
    val shape = RoundedCornerShape(20.dp)
    Column(
        Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(colors.card.copy(alpha = 0.96f))
            .border(1.dp, colors.borderStrong, shape)
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
        content = content,
    )
}

@Composable
private fun Title(text: String) = Text(text, style = WizardTheme.type.title.copy(fontSize = WizardTheme.type.largeTitle.fontSize * 0.78f), color = WizardTheme.colors.text)

@Composable
private fun Copy(text: String) = Text(text, style = WizardTheme.type.body, color = WizardTheme.colors.muted)

@Composable
private fun Footer(content: @Composable () -> Unit) {
    Row(Modifier.fillMaxWidth().padding(top = 4.dp), horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End), verticalAlignment = Alignment.CenterVertically) {
        content()
    }
}

@Composable
private fun Nav(step: OnboardingStep, onStep: (OnboardingStep) -> Unit, next: String = "Continue", onNext: () -> Unit = { onStep(OnboardingStep.entries[step.ordinal + 1]) }) {
    Footer {
        WizardButton("Back", { onStep(OnboardingStep.entries[step.ordinal - 1]) }, kind = ButtonKind.Quiet)
        WizardButton(next, onNext)
    }
}

@Composable
private fun Welcome(onStart: () -> Unit, onSkip: () -> Unit) {
    val colors = WizardTheme.colors
    Box(Modifier.size(48.dp).clip(RoundedCornerShape(13.dp)).background(colors.text), contentAlignment = Alignment.Center) {
        WandMark(size = 26.dp, tint = colors.onSolid)
    }
    Title("Welcome to Wizard")
    Copy("Drive Wizard, Pi and Claude Code on your own machines over SSH. They run there; this phone starts chats, watches them work and tells you when they're done.")
    Row(horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically) {
        Agent.entries.forEach { AgentMark(it, size = 18.dp, tint = colors.muted) }
    }
    Footer {
        WizardButton("Skip setup", onSkip, kind = ButtonKind.Quiet)
        WizardButton("Get started", onStart)
    }
    Hairline(inset = 0.dp)
    Text(
        "Wizard's apps are built on Zeron, the open-source agent controller created by the Zeron team (zeron.sh). Thank you to its creators.",
        style = WizardTheme.type.small,
        color = colors.faint,
    )
}

@Composable
private fun Appearance(settings: Settings, onTheme: (ThemeChoice) -> Unit, onCompact: (Boolean) -> Unit, onArtwork: (Boolean) -> Unit, onStep: (OnboardingStep) -> Unit) {
    Title("Make it yours")
    Segmented(listOf(ThemeChoice.Dark to "Dark", ThemeChoice.Light to "Light", ThemeChoice.System to "System"), settings.theme, onTheme)
    Column(Modifier.padding(horizontal = 0.dp)) {
        ToggleRow("Compact mode", "Fold each turn's tool calls and thinking into one line, so replies stay front and center.", settings.compact, onCompact, inset = 0.dp)
        ToggleRow("Wizard artwork", "Show the Starship backdrop behind new Wizard chats.", settings.artwork, onArtwork, inset = 0.dp)
    }
    Nav(OnboardingStep.Appearance, onStep)
}

@Composable
private fun FirstMachine(
    machine: OnboardingMachine,
    onMachine: (OnboardingMachine) -> Unit,
    onGenerateKey: () -> Unit,
    onCopyKey: () -> Unit,
    onShareKey: () -> Unit,
    onSave: () -> Unit,
    onStep: (OnboardingStep) -> Unit,
) {
    val colors = WizardTheme.colors
    Title("Add your first machine")
    if (machine.saved) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            WizardIcon(R.drawable.ic_check, null, size = 18.dp, tint = colors.success)
            Text("${machine.user}@${machine.host} is added. The app connects to it when you start a chat.", style = WizardTheme.type.body, color = colors.text, modifier = Modifier.padding(start = 10.dp))
        }
        Nav(OnboardingStep.Machine, onStep)
        return
    }
    Copy("Any computer you can SSH into. You can add more, or do this later, in Settings.")
    Field("Host", machine.host, { onMachine(machine.copy(host = it.trim(), error = null)) }, placeholder = "buildbox.local", mono = true, keyboardType = KeyboardType.Uri)
    Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        Field("User", machine.user, { onMachine(machine.copy(user = it.trim(), error = null)) }, Modifier.weight(1f), placeholder = "dev", mono = true)
        Field("Port", machine.port, { onMachine(machine.copy(port = it.filter(Char::isDigit).take(5))) }, Modifier.width(88.dp), mono = true, keyboardType = KeyboardType.Number)
    }
    val key = machine.key
    if (key == null) {
        WizardButton("Generate an SSH key", onGenerateKey, kind = ButtonKind.Quiet, icon = R.drawable.ic_key_minimalistic)
    } else {
        PublicKeyPanel(key, { onCopyKey() }, { onShareKey() })
    }
    machine.error?.let { Text(it, style = WizardTheme.type.small, color = colors.danger) }
    Footer {
        WizardButton("Back", { onStep(OnboardingStep.Appearance) }, kind = ButtonKind.Quiet)
        if (machine.filled) WizardButton("Add machine", onSave, enabled = key != null)
        else WizardButton("Skip", { onStep(OnboardingStep.Notifications) }, kind = ButtonKind.Quiet)
    }
}

@Composable
private fun Notifications(asked: Boolean, onAllow: () -> Unit, onStep: (OnboardingStep) -> Unit) {
    Title("Stay in the loop")
    Copy("Wizard can tell you when a turn finishes while you're away, and when an agent is waiting on your answer. While a turn runs, a quiet notification keeps the connection open.")
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Bullet(R.drawable.ic_check, "Turn finished", "With the first line of the reply")
        Bullet(R.drawable.ic_bell, "Needs your input", "When an agent asks you a question")
    }
    Footer {
        WizardButton("Back", { onStep(OnboardingStep.Machine) }, kind = ButtonKind.Quiet)
        if (asked) {
            WizardButton("Continue", { onStep(OnboardingStep.Done) })
        } else {
            WizardButton("Not now", { onStep(OnboardingStep.Done) }, kind = ButtonKind.Quiet)
            WizardButton("Allow", onAllow)
        }
    }
}

@Composable
private fun Bullet(icon: Int, title: String, detail: String) {
    val colors = WizardTheme.colors
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(32.dp).clip(RoundedCornerShape(9.dp)).background(colors.raised), contentAlignment = Alignment.Center) {
            WizardIcon(icon, null, size = 16.dp, tint = colors.text)
        }
        Column(Modifier.padding(start = 12.dp)) {
            Text(title, style = WizardTheme.type.body, color = colors.text)
            Text(detail, style = WizardTheme.type.small, color = colors.faint)
        }
    }
}

@Composable
private fun Done(onStep: (OnboardingStep) -> Unit, onFinish: () -> Unit) {
    val colors = WizardTheme.colors
    Title("You're all set")
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        listOf(
            "Type on the home screen and send. The chips above the send button pick the agent, machine and folder.",
            "Every chat from every machine is in one list below the composer.",
            "Machines, keys, notifications and these choices live in Settings.",
        ).forEach { tip ->
            Row {
                Text("•", style = WizardTheme.type.body, color = colors.faint, modifier = Modifier.width(16.dp))
                Text(tip, style = WizardTheme.type.body, color = colors.muted)
            }
        }
    }
    Footer {
        WizardButton("Back", { onStep(OnboardingStep.Notifications) }, kind = ButtonKind.Quiet)
        WizardButton("Start chatting", onFinish)
    }
}
