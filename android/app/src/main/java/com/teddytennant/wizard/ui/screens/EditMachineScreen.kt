package com.teddytennant.wizard.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
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
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.data.AuthKind
import com.teddytennant.wizard.data.MachineForm
import com.teddytennant.wizard.ssh.StoredKey
import com.teddytennant.wizard.ui.components.ButtonKind
import com.teddytennant.wizard.ui.components.FieldShape
import com.teddytennant.wizard.ui.components.Field
import com.teddytennant.wizard.ui.components.Hairline
import com.teddytennant.wizard.ui.components.IconAction
import com.teddytennant.wizard.ui.components.Panel
import com.teddytennant.wizard.ui.components.Segmented
import com.teddytennant.wizard.ui.components.SectionLabel
import com.teddytennant.wizard.ui.components.TextAction
import com.teddytennant.wizard.ui.components.TopBar
import com.teddytennant.wizard.ui.components.WizardButton
import com.teddytennant.wizard.ui.components.WizardIcon
import com.teddytennant.wizard.ui.theme.WizardTheme

data class MachineFormState(
    val name: String = "",
    val host: String = "",
    val port: String = "22",
    val user: String = "",
    val auth: AuthKind = AuthKind.Key,
    val keyId: String? = null,
    val password: String = "",
    val hasSavedPassword: Boolean = false,
    val errors: MachineForm.Errors = MachineForm.Errors(),
    val saving: Boolean = false,
)

@Composable
fun EditMachineContent(
    editing: Boolean,
    form: MachineFormState,
    keys: List<StoredKey>,
    onChange: (MachineFormState) -> Unit,
    onGenerateKey: () -> Unit,
    onImportKey: () -> Unit,
    onCopyKey: (StoredKey) -> Unit,
    onShareKey: (StoredKey) -> Unit,
    onSave: () -> Unit,
    onBack: () -> Unit,
) {
    val colors = WizardTheme.colors
    Column(Modifier.fillMaxSize().background(colors.background).imePadding()) {
        TopBar(if (editing) "Edit machine" else "Add machine", onBack)
        Column(
            Modifier.weight(1f).verticalScroll(rememberScrollState()).padding(horizontal = 20.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            Spacer(Modifier.height(4.dp))
            Field("Name", form.name, { onChange(form.copy(name = it)) }, placeholder = "buildbox", error = form.errors.name)
            Field(
                "Host",
                form.host,
                { value ->
                    val (user, host, port) = MachineForm.splitAddress(value)
                    onChange(
                        if (value.contains('@') && user != null) {
                            form.copy(host = host, user = user, port = port?.toString() ?: form.port)
                        } else {
                            form.copy(host = value.trim())
                        },
                    )
                },
                placeholder = "buildbox.local or 192.168.1.20",
                error = form.errors.host,
                mono = true,
                keyboardType = KeyboardType.Uri,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Field("User", form.user, { onChange(form.copy(user = it.trim())) }, Modifier.weight(1f), placeholder = "teddy", error = form.errors.user, mono = true)
                Field("Port", form.port, { onChange(form.copy(port = it.filter(Char::isDigit).take(5))) }, Modifier.width(96.dp), error = form.errors.port, mono = true, keyboardType = KeyboardType.Number)
            }

            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                SectionLabel("Sign in with")
                Segmented(listOf(AuthKind.Key to "SSH key", AuthKind.Password to "Password"), form.auth, { onChange(form.copy(auth = it)) })
                when (form.auth) {
                    AuthKind.Key -> KeyPicker(keys, form.keyId, { onChange(form.copy(keyId = it)) }, onGenerateKey, onImportKey, onCopyKey, onShareKey, form.errors.auth)
                    AuthKind.Password -> PasswordField(form, onChange)
                }
            }
            Spacer(Modifier.height(8.dp))
        }
        Box(Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 12.dp).navigationBarsPadding()) {
            WizardButton(if (editing) "Save" else "Save and connect", onSave, Modifier.fillMaxWidth(), busy = form.saving)
        }
    }
}

@Composable
private fun PasswordField(form: MachineFormState, onChange: (MachineFormState) -> Unit) {
    var visible by remember { mutableStateOf(false) }
    Field(
        "Password",
        form.password,
        { onChange(form.copy(password = it)) },
        placeholder = if (form.hasSavedPassword) "Saved. Type to replace it." else null,
        password = !visible,
        error = form.errors.auth,
        help = "Kept on this phone, encrypted with a key that never leaves the Android Keystore.",
        imeAction = ImeAction.Done,
        trailing = {
            IconAction(if (visible) R.drawable.ic_eye_closed else R.drawable.ic_eye, if (visible) "Hide password" else "Show password", { visible = !visible })
        },
    )
}

@Composable
private fun KeyPicker(
    keys: List<StoredKey>,
    selected: String?,
    onSelect: (String) -> Unit,
    onGenerate: () -> Unit,
    onImport: () -> Unit,
    onCopy: (StoredKey) -> Unit,
    onShare: (StoredKey) -> Unit,
    error: String?,
) {
    val colors = WizardTheme.colors
    if (keys.isNotEmpty()) {
        Panel(padding = androidx.compose.foundation.layout.PaddingValues(0.dp)) {
            keys.forEachIndexed { i, key ->
                if (i > 0) Hairline()
                val on = key.id == selected
                Row(
                    Modifier
                        .fillMaxWidth()
                        .clickable(role = Role.RadioButton) { onSelect(key.id) }
                        .semantics { this.selected = on }
                        .heightIn(min = 56.dp)
                        .padding(horizontal = 16.dp, vertical = 10.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Radio(on)
                    Column(Modifier.weight(1f).padding(start = 14.dp)) {
                        Text(key.label, style = WizardTheme.type.body, color = colors.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        Text(
                            key.algorithm.removePrefix("ssh-") + "  " + (key.fingerprint?.removePrefix("SHA256:")?.take(16) ?: ""),
                            style = WizardTheme.type.monoSmall,
                            color = colors.faint,
                            maxLines = 1,
                        )
                    }
                }
            }
        }
    }
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        WizardButton("New key", onGenerate, kind = ButtonKind.Quiet, icon = R.drawable.ic_plus)
        WizardButton("Import", onImport, kind = ButtonKind.Quiet, icon = R.drawable.ic_key_minimalistic)
    }
    if (error != null) Text(error, style = WizardTheme.type.small, color = colors.danger)
    val key = keys.firstOrNull { it.id == selected }
    if (key != null) PublicKeyPanel(key, onCopy, onShare)
}

@Composable
fun PublicKeyPanel(key: StoredKey, onCopy: (StoredKey) -> Unit, onShare: (StoredKey) -> Unit) {
    val colors = WizardTheme.colors
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Text(
            "Add this line to ~/.ssh/authorized_keys on the machine:",
            style = WizardTheme.type.small,
            color = colors.muted,
        )
        Box(
            Modifier
                .fillMaxWidth()
                .clip(FieldShape)
                .background(colors.code)
                .border(1.dp, colors.border, FieldShape)
                .padding(horizontal = 14.dp, vertical = 12.dp)
                .semantics { contentDescription = "Public key for ${key.label}" },
        ) {
            Text(key.publicKey, style = WizardTheme.type.monoSmall, color = colors.text)
        }
        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            TextAction("Copy", { onCopy(key) })
            TextAction("Share", { onShare(key) })
        }
    }
}

@Composable
fun Radio(on: Boolean) {
    val colors = WizardTheme.colors
    Box(
        Modifier.size(20.dp).clip(CircleShape).border(1.5.dp, if (on) colors.text else colors.borderStrong, CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        if (on) Box(Modifier.size(10.dp).clip(CircleShape).background(colors.text))
    }
}

/** Paste or pick a private key. */
@Composable
fun ImportKeyContent(
    text: String,
    passphrase: String,
    label: String,
    error: String?,
    busy: Boolean,
    onText: (String) -> Unit,
    onPassphrase: (String) -> Unit,
    onLabel: (String) -> Unit,
    onPickFile: () -> Unit,
    onImport: () -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Text("Import a private key", style = WizardTheme.type.title, color = WizardTheme.colors.text)
        Field("Name", label, onLabel, placeholder = "laptop key")
        Field("Private key", text, onText, placeholder = "-----BEGIN OPENSSH PRIVATE KEY-----", mono = true, singleLine = false, minLines = 4)
        TextAction("Choose a file", onPickFile, Modifier.padding(start = 0.dp))
        Field("Passphrase", passphrase, onPassphrase, placeholder = "If the key has one", password = true, error = error, imeAction = ImeAction.Done)
        WizardButton("Import", onImport, Modifier.fillMaxWidth(), busy = busy, enabled = text.isNotBlank())
    }
}

