package com.teddytennant.wizard.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import com.teddytennant.wizard.R
import com.teddytennant.wizard.ssh.KnownHost
import com.teddytennant.wizard.ssh.PresentedKey
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.FieldShape
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

private val DialogShape = RoundedCornerShape(24.dp)

/** A calm dialog surface: title, body, buttons stacked at the bottom. */
@Composable
fun WizardDialog(onDismiss: () -> Unit, dismissible: Boolean = true, content: @Composable () -> Unit) {
    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(dismissOnBackPress = dismissible, dismissOnClickOutside = dismissible, usePlatformDefaultWidth = false)) {
        DialogSurface(content)
    }
}

@Composable
fun DialogSurface(content: @Composable () -> Unit) {
    val colors = WizardTheme.colors
    Box(
        Modifier
            .padding(horizontal = 20.dp)
            .fillMaxWidth()
            .clip(DialogShape)
            .background(colors.card)
            .border(1.dp, colors.borderStrong, DialogShape)
            .padding(24.dp),
    ) { content() }
}

@Composable
private fun Fingerprint(label: String, value: String) {
    val colors = WizardTheme.colors
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Text(label, style = WizardTheme.type.label, color = colors.muted)
        Box(Modifier.fillMaxWidth().clip(FieldShape).background(colors.code).border(1.dp, colors.border, FieldShape).padding(12.dp)) {
            Text(value, style = WizardTheme.type.mono, color = colors.text)
        }
    }
}

private fun keyName(type: String) = when (type) {
    "ssh-ed25519" -> "ED25519"
    "ssh-rsa" -> "RSA"
    else -> type.removePrefix("ecdsa-sha2-").uppercase()
}

@Composable
fun HostKeyTrustContent(presented: PresentedKey, onTrust: () -> Unit, onCancel: () -> Unit) {
    val colors = WizardTheme.colors
    Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Text("Trust this machine?", style = WizardTheme.type.title, color = colors.text)
        Text(
            "This is the first connection to ${presented.host}:${presented.port}. Check that the fingerprint matches the machine's host key before you trust it.",
            style = WizardTheme.type.body,
            color = colors.muted,
        )
        Fingerprint("${keyName(presented.keyType)} fingerprint", presented.fingerprint)
        Text(
            "On the machine: ssh-keygen -lf /etc/ssh/ssh_host_${presented.keyType.removePrefix("ssh-").substringBefore('-').let { if (it == "sha2") "ecdsa" else it }}_key.pub",
            style = WizardTheme.type.monoSmall,
            color = colors.faint,
        )
        Column(verticalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(top = 4.dp)) {
            WizardButton("Trust and connect", onTrust, Modifier.fillMaxWidth())
            WizardButton("Cancel", onCancel, Modifier.fillMaxWidth(), kind = ButtonKind.Quiet)
        }
    }
}

@Composable
fun HostKeyChangedContent(presented: PresentedKey, trusted: KnownHost, onReplace: () -> Unit, onCancel: () -> Unit) {
    val colors = WizardTheme.colors
    var armed by remember { mutableStateOf(false) }
    Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            WizardIcon(R.drawable.ic_danger_triangle, null, tint = colors.danger, size = 22.dp)
            Text("Host key changed", style = WizardTheme.type.title, color = colors.danger, modifier = Modifier.padding(start = 10.dp))
        }
        Text(
            "${presented.host}:${presented.port} answered with a different key than the one you trusted. Someone may be intercepting the connection. It can also happen after the machine was reinstalled. Nothing was sent.",
            style = WizardTheme.type.body,
            color = colors.text,
        )
        Fingerprint("Trusted", trusted.fingerprint)
        Fingerprint("Presented now (${keyName(presented.keyType)})", presented.fingerprint)
        Column(verticalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(top = 4.dp)) {
            WizardButton("Don't connect", onCancel, Modifier.fillMaxWidth())
            WizardButton(
                if (armed) "Tap again to trust the new key" else "I know why it changed",
                { if (armed) onReplace() else armed = true },
                Modifier.fillMaxWidth(),
                kind = ButtonKind.Danger,
            )
        }
    }
}

@Composable
fun ConfirmContent(title: String, body: String, confirm: String, danger: Boolean, onConfirm: () -> Unit, onCancel: () -> Unit) {
    val colors = WizardTheme.colors
    Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Text(title, style = WizardTheme.type.title, color = colors.text)
        Text(body, style = WizardTheme.type.body, color = colors.muted)
        Row(Modifier.fillMaxWidth().padding(top = 4.dp), horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End)) {
            WizardButton("Cancel", onCancel, kind = ButtonKind.Quiet)
            WizardButton(confirm, onConfirm, kind = if (danger) ButtonKind.Danger else ButtonKind.Solid)
        }
    }
}

@Composable
fun TextDocumentContent(title: String, text: String, onClose: () -> Unit) {
    val colors = WizardTheme.colors
    Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Text(title, style = WizardTheme.type.title, color = colors.text)
        Box(Modifier.heightIn(max = 420.dp).verticalScroll(rememberScrollState())) {
            Text(text, style = WizardTheme.type.monoSmall, color = colors.muted)
        }
        WizardButton("Close", onClose, Modifier.fillMaxWidth(), kind = ButtonKind.Quiet)
    }
}
