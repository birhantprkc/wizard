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
import com.teddytennant.wizard.R
import com.teddytennant.wizard.data.Settings
import com.teddytennant.wizard.data.ThemeChoice
import com.teddytennant.wizard.ssh.StoredKey
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.Hairline
import com.teddytennant.wizard.ui.components.ListRow
import com.teddytennant.wizard.ui.components.Panel
import com.teddytennant.wizard.ui.components.SectionLabel
import com.teddytennant.wizard.ui.components.Segmented
import com.teddytennant.wizard.ui.components.TextAction
import com.teddytennant.wizard.ui.components.ToggleRow
import com.teddytennant.wizard.ui.components.TopBar
import com.teddytennant.wizard.ui.components.WandMark
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

@Composable
private fun ScreenColumn(title: String, onBack: () -> Unit, content: @Composable () -> Unit) {
    Column(Modifier.fillMaxSize().background(WizardTheme.colors.background)) {
        TopBar(title, onBack)
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp).navigationBarsPadding(),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            content()
            Spacer(Modifier.height(24.dp))
        }
    }
}

@Composable
fun SettingsContent(
    settings: Settings,
    machineCount: Int,
    keyCount: Int,
    trustedHosts: Int,
    notificationsAllowed: Boolean,
    version: String,
    onMachines: () -> Unit,
    onTheme: (ThemeChoice) -> Unit,
    onNotifyFinished: (Boolean) -> Unit,
    onNotifyInput: (Boolean) -> Unit,
    onSystemNotifications: () -> Unit,
    onKeys: () -> Unit,
    onAbout: () -> Unit,
    onBack: () -> Unit,
) {
    val colors = WizardTheme.colors
    ScreenColumn("Settings", onBack) {
        SectionLabel("Machines", Modifier.padding(top = 8.dp))
        Panel(padding = PaddingValues(0.dp)) {
            ListRow(
                "Machines",
                subtitle = when (machineCount) { 0 -> "None yet"; 1 -> "1 machine"; else -> "$machineCount machines" },
                icon = R.drawable.ic_monitor,
                onClick = onMachines,
            ) { WizardIcon(R.drawable.ic_alt_arrow_right, null, size = 18.dp, tint = colors.faint) }
        }

        SectionLabel("Appearance", Modifier.padding(top = 20.dp))
        Segmented(listOf(ThemeChoice.Dark to "Dark", ThemeChoice.Light to "Light", ThemeChoice.System to "System"), settings.theme, onTheme)

        SectionLabel("Notifications", Modifier.padding(top = 20.dp))
        Panel(padding = PaddingValues(0.dp)) {
            ToggleRow("Turn finished", "When an agent finishes while you're away", settings.notifyFinished, onNotifyFinished)
            Hairline()
            ToggleRow("Needs your input", "When an agent asks you something", settings.notifyInput, onNotifyInput)
            Hairline()
            ListRow(
                "System settings",
                subtitle = if (notificationsAllowed) "Notifications are allowed" else "Notifications are off for Wizard",
                onClick = onSystemNotifications,
            ) { WizardIcon(R.drawable.ic_arrow_up_right, null, size = 18.dp, tint = colors.faint) }
        }

        SectionLabel("Security", Modifier.padding(top = 20.dp))
        Panel(padding = PaddingValues(0.dp)) {
            ListRow(
                "SSH keys",
                subtitle = when (keyCount) { 0 -> "None yet"; 1 -> "1 key"; else -> "$keyCount keys" } +
                    "  ·  " + when (trustedHosts) { 1 -> "1 trusted host"; else -> "$trustedHosts trusted hosts" },
                icon = R.drawable.ic_key_minimalistic,
                onClick = onKeys,
            ) { WizardIcon(R.drawable.ic_alt_arrow_right, null, size = 18.dp, tint = colors.faint) }
        }

        SectionLabel("About", Modifier.padding(top = 20.dp))
        Panel(padding = PaddingValues(0.dp)) {
            ListRow("About Wizard", subtitle = "Version $version, licenses", icon = R.drawable.ic_info_circle, onClick = onAbout) {
                WizardIcon(R.drawable.ic_alt_arrow_right, null, size = 18.dp, tint = colors.faint)
            }
        }
    }
}

@Composable
fun KeysContent(
    keys: List<StoredKey>,
    onGenerate: () -> Unit,
    onImport: () -> Unit,
    onCopy: (StoredKey) -> Unit,
    onShare: (StoredKey) -> Unit,
    onDelete: (StoredKey) -> Unit,
    onBack: () -> Unit,
) {
    val colors = WizardTheme.colors
    var open by remember { mutableStateOf<String?>(null) }
    ScreenColumn("SSH keys", onBack) {
        Text(
            "Private keys stay on this phone, sealed by the Android Keystore. They're never backed up.",
            style = WizardTheme.type.small,
            color = colors.muted,
            modifier = Modifier.padding(top = 4.dp, bottom = 8.dp),
        )
        if (keys.isEmpty()) {
            Text("No keys yet.", style = WizardTheme.type.body, color = colors.faint, modifier = Modifier.padding(vertical = 12.dp))
        }
        keys.forEach { key ->
            Panel(onClick = { open = if (open == key.id) null else key.id }, onClickLabel = "Show ${key.label}", padding = PaddingValues(0.dp)) {
                ListRow(
                    key.label,
                    subtitle = key.algorithm.removePrefix("ssh-") + "  " + (key.fingerprint ?: "") + if (key.imported) "  ·  imported" else "",
                    icon = R.drawable.ic_key_minimalistic,
                    monoSubtitle = true,
                )
                if (open == key.id) {
                    Column(Modifier.padding(start = 16.dp, end = 16.dp, bottom = 8.dp)) {
                        PublicKeyPanel(key, onCopy, onShare)
                        TextAction("Delete key", { onDelete(key) }, color = colors.danger)
                    }
                }
            }
        }
        Row(Modifier.padding(top = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            WizardButton("New key", onGenerate, icon = R.drawable.ic_plus)
            WizardButton("Import", onImport, kind = ButtonKind.Quiet)
        }
    }
}

data class LicenseEntry(val name: String, val license: String, val asset: String? = null)

val Licenses = listOf(
    LicenseEntry("Zeron", "MIT", "licenses/zeron-mit.txt"),
    LicenseEntry("Geist and Geist Mono", "SIL Open Font License 1.1", "licenses/geist-ofl.txt"),
    LicenseEntry("Solar icons, by 480 Design", "CC BY 4.0"),
    LicenseEntry("sshj", "Apache License 2.0"),
    LicenseEntry("Bouncy Castle", "MIT"),
    LicenseEntry("SLF4J", "MIT"),
    LicenseEntry("AndroidX and Jetpack Compose", "Apache License 2.0"),
    LicenseEntry("Kotlin, kotlinx.coroutines, kotlinx.serialization", "Apache License 2.0"),
)

@Composable
fun AboutContent(version: String, onLicense: (LicenseEntry) -> Unit, onBack: () -> Unit) {
    val colors = WizardTheme.colors
    ScreenColumn("About", onBack) {
        Column(Modifier.fillMaxWidth().padding(top = 16.dp, bottom = 12.dp), horizontalAlignment = Alignment.CenterHorizontally) {
            Box(
                Modifier.size(72.dp).clip(RoundedCornerShape(20.dp)).background(Color0C).border(1.dp, colors.border, RoundedCornerShape(20.dp)),
                contentAlignment = Alignment.Center,
            ) { WandMark(size = 34.dp, tint = androidx.compose.ui.graphics.Color(0xFFECECEE)) }
            Spacer(Modifier.height(16.dp))
            Text("Wizard", style = WizardTheme.type.title, color = colors.text)
            Text("Version $version", style = WizardTheme.type.small, color = colors.faint)
        }
        Text(
            "Wizard for Android drives Wizard on machines you own over SSH, through wizard acp. Nothing goes through a server of ours.",
            style = WizardTheme.type.body,
            color = colors.text,
        )
        Text(
            "Its look comes from Wizard GUI, the desktop app, which is a fork of Zeron by the Zeron team (zeron.sh). Zeron is MIT licensed; thank you to its creators.",
            style = WizardTheme.type.body,
            color = colors.muted,
        )
        SectionLabel("Licenses", Modifier.padding(top = 16.dp))
        Panel(padding = PaddingValues(0.dp)) {
            Licenses.forEachIndexed { i, entry ->
                if (i > 0) Hairline()
                ListRow(entry.name, subtitle = entry.license, onClick = if (entry.asset != null) ({ onLicense(entry) }) else null) {
                    if (entry.asset != null) WizardIcon(R.drawable.ic_alt_arrow_right, null, size = 18.dp, tint = colors.faint)
                }
            }
        }
        Text(
            "Wizard itself is MIT and Apache-2.0 licensed.",
            style = WizardTheme.type.small,
            color = colors.faint,
            modifier = Modifier.padding(top = 8.dp),
        )
    }
}

private val Color0C = androidx.compose.ui.graphics.Color(0xFF0C0C0E)
