package com.teddytennant.wizard.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Icon
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.ui.theme.WizardTheme

val CardShape = RoundedCornerShape(16.dp)
val FieldShape = RoundedCornerShape(12.dp)
val PillShape = RoundedCornerShape(percent = 50)

@Composable
fun WizardIcon(@DrawableRes id: Int, contentDescription: String?, modifier: Modifier = Modifier, size: Dp = 20.dp, tint: Color = WizardTheme.colors.muted) {
    Icon(painterResource(id), contentDescription, modifier.size(size), tint = tint)
}

/** An icon button with the 48dp touch target around a quieter glyph. */
@Composable
fun IconAction(@DrawableRes id: Int, contentDescription: String, onClick: () -> Unit, modifier: Modifier = Modifier, tint: Color = WizardTheme.colors.muted, enabled: Boolean = true) {
    Box(
        modifier
            .size(48.dp)
            .clip(CircleShape)
            .clickable(enabled = enabled, role = Role.Button, onClick = onClick)
            .semantics { this.contentDescription = contentDescription },
        contentAlignment = Alignment.Center,
    ) {
        WizardIcon(id, null, size = 22.dp, tint = if (enabled) tint else tint.copy(alpha = 0.4f))
    }
}

/** A top bar without Material's chrome: back, a title with an optional subtitle, and actions. */
@Composable
fun TopBar(
    title: String?,
    onBack: (() -> Unit)?,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    titleIcon: (@Composable () -> Unit)? = null,
    actions: @Composable RowScope.() -> Unit = {},
) {
    Row(
        modifier
            .fillMaxWidth()
            .statusBarsPadding()
            .heightIn(min = 60.dp)
            .padding(start = if (onBack != null) 4.dp else 20.dp, end = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (onBack != null) IconAction(R.drawable.ic_alt_arrow_left, "Back", onBack, tint = WizardTheme.colors.text)
        if (titleIcon != null) {
            Box(Modifier.padding(start = 2.dp, end = 10.dp)) { titleIcon() }
        }
        Column(Modifier.weight(1f).padding(start = if (onBack != null && titleIcon == null) 2.dp else 0.dp)) {
            if (title != null) {
                Text(title, style = WizardTheme.type.title, color = WizardTheme.colors.text, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.semantics { heading() })
            }
            if (subtitle != null) {
                Text(subtitle, style = WizardTheme.type.small, color = WizardTheme.colors.faint, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
        actions()
    }
}

@Composable
fun LargeTitle(text: String, modifier: Modifier = Modifier) {
    Text(text, style = WizardTheme.type.largeTitle, color = WizardTheme.colors.text, modifier = modifier.semantics { heading() })
}

@Composable
fun SectionLabel(text: String, modifier: Modifier = Modifier, trailing: @Composable RowScope.() -> Unit = {}) {
    Row(modifier.fillMaxWidth().heightIn(min = 32.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(text, style = WizardTheme.type.section, color = WizardTheme.colors.faint, modifier = Modifier.weight(1f).semantics { heading() })
        trailing()
    }
}

/** A bordered surface. Clickable when [onClick] is set. */
@Composable
fun Panel(modifier: Modifier = Modifier, onClick: (() -> Unit)? = null, onClickLabel: String? = null, padding: PaddingValues = PaddingValues(16.dp), content: @Composable ColumnScope.() -> Unit) {
    val colors = WizardTheme.colors
    Column(
        modifier
            .clip(CardShape)
            .background(colors.card)
            .border(1.dp, colors.border, CardShape)
            .then(if (onClick != null) Modifier.clickable(onClickLabel = onClickLabel, role = Role.Button, onClick = onClick) else Modifier)
            .padding(padding),
        content = content,
    )
}

/** A row inside a [Panel] list: 56dp high, icon, text, trailing. */
@Composable
fun ListRow(
    title: String,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    @DrawableRes icon: Int? = null,
    iconTint: Color = WizardTheme.colors.muted,
    titleStyle: TextStyle = WizardTheme.type.body,
    monoSubtitle: Boolean = false,
    onClick: (() -> Unit)? = null,
    trailing: @Composable RowScope.() -> Unit = {},
) {
    Row(
        modifier
            .fillMaxWidth()
            .then(if (onClick != null) Modifier.clickable(role = Role.Button, onClick = onClick) else Modifier)
            .heightIn(min = 56.dp)
            .padding(horizontal = 16.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (icon != null) {
            WizardIcon(icon, null, tint = iconTint)
            Spacer(Modifier.width(14.dp))
        }
        Column(Modifier.weight(1f)) {
            Text(title, style = titleStyle, color = WizardTheme.colors.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
            if (subtitle != null) {
                Text(
                    subtitle,
                    style = if (monoSubtitle) WizardTheme.type.monoSmall else WizardTheme.type.small,
                    color = WizardTheme.colors.faint,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        trailing()
    }
}

@Composable
fun Hairline(modifier: Modifier = Modifier, inset: Dp = 16.dp) {
    Box(modifier.padding(start = inset).fillMaxWidth().height(1.dp).background(WizardTheme.colors.border))
}

enum class ButtonKind { Solid, Quiet, Danger }

@Composable
fun WizardButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    kind: ButtonKind = ButtonKind.Solid,
    enabled: Boolean = true,
    busy: Boolean = false,
    @DrawableRes icon: Int? = null,
) {
    val colors = WizardTheme.colors
    val (bg, fg, border) = when (kind) {
        ButtonKind.Solid -> Triple(colors.solid, colors.onSolid, null)
        ButtonKind.Quiet -> Triple(Color.Transparent, colors.text, colors.borderStrong)
        ButtonKind.Danger -> Triple(colors.dangerSoft, colors.danger, colors.danger.copy(alpha = 0.35f))
    }
    Row(
        modifier
            .heightIn(min = 48.dp)
            .widthIn(min = 64.dp)
            .alpha(if (enabled) 1f else 0.45f)
            .clip(PillShape)
            .background(bg)
            .then(if (border != null) Modifier.border(BorderStroke(1.dp, border), PillShape) else Modifier)
            .clickable(enabled = enabled && !busy, role = Role.Button, onClick = onClick)
            .padding(horizontal = 20.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (busy) {
            com.teddytennant.wizard.ui.life.LifeIndicator(size = 16.dp, color = fg)
            Spacer(Modifier.width(10.dp))
        } else if (icon != null) {
            WizardIcon(icon, null, size = 18.dp, tint = fg)
            Spacer(Modifier.width(8.dp))
        }
        Text(text, style = WizardTheme.type.label.copy(fontSize = WizardTheme.type.body.fontSize), color = fg, maxLines = 1)
    }
}

/** A small text link-button with a 48dp target. */
@Composable
fun TextAction(text: String, onClick: () -> Unit, modifier: Modifier = Modifier, color: Color = WizardTheme.colors.text, enabled: Boolean = true) {
    Box(
        modifier
            .heightIn(min = 48.dp)
            .clip(PillShape)
            .clickable(enabled = enabled, role = Role.Button, onClick = onClick)
            .padding(horizontal = 12.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(text, style = WizardTheme.type.label, color = if (enabled) color else color.copy(alpha = 0.4f))
    }
}

@Composable
fun StatusDot(color: Color, modifier: Modifier = Modifier, pulsing: Boolean = false) {
    val alpha = if (pulsing) {
        val t = rememberInfiniteTransition(label = "pulse")
        val a by t.animateFloat(0.35f, 1f, infiniteRepeatable(tween(900), RepeatMode.Reverse), label = "alpha")
        a
    } else {
        1f
    }
    Box(modifier.size(7.dp).alpha(alpha).clip(CircleShape).background(color))
}

/** A labelled text field: label above, quiet filled box, error below. */
@Composable
fun Field(
    label: String,
    value: String,
    onValueChange: (String) -> Unit,
    modifier: Modifier = Modifier,
    placeholder: String? = null,
    error: String? = null,
    help: String? = null,
    mono: Boolean = false,
    password: Boolean = false,
    keyboardType: KeyboardType = KeyboardType.Text,
    singleLine: Boolean = true,
    minLines: Int = 1,
    trailing: @Composable (() -> Unit)? = null,
    imeAction: androidx.compose.ui.text.input.ImeAction = androidx.compose.ui.text.input.ImeAction.Next,
    onDone: (() -> Unit)? = null,
) {
    val colors = WizardTheme.colors
    val style = (if (mono) WizardTheme.type.mono.copy(fontSize = WizardTheme.type.body.fontSize) else WizardTheme.type.body).copy(color = colors.text)
    Column(modifier) {
        Text(label, style = WizardTheme.type.label, color = colors.muted, modifier = Modifier.padding(bottom = 8.dp))
        Row(
            Modifier
                .fillMaxWidth()
                .heightIn(min = 48.dp)
                .clip(FieldShape)
                .background(colors.input)
                .border(1.dp, if (error != null) colors.danger.copy(alpha = 0.6f) else colors.border, FieldShape)
                .padding(start = 14.dp, end = if (trailing != null) 2.dp else 14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            BasicTextField(
                value = value,
                onValueChange = onValueChange,
                singleLine = singleLine,
                minLines = minLines,
                textStyle = style,
                cursorBrush = SolidColor(colors.accent),
                visualTransformation = if (password) PasswordVisualTransformation() else VisualTransformation.None,
                keyboardOptions = KeyboardOptions(keyboardType = if (password) KeyboardType.Password else keyboardType, imeAction = imeAction, autoCorrectEnabled = !mono && !password && keyboardType == KeyboardType.Text),
                keyboardActions = KeyboardActions(onDone = { onDone?.invoke() }),
                modifier = Modifier.weight(1f).padding(vertical = 12.dp).semantics { contentDescription = label },
                decorationBox = { inner ->
                    Box {
                        if (value.isEmpty() && placeholder != null) Text(placeholder, style = style, color = colors.faint)
                        inner()
                    }
                },
            )
            trailing?.invoke()
        }
        val note = error ?: help
        if (note != null) {
            Text(note, style = WizardTheme.type.small, color = if (error != null) colors.danger else colors.faint, modifier = Modifier.padding(top = 6.dp, start = 2.dp))
        }
    }
}

/** Two to four mutually exclusive choices. */
@Composable
fun <T> Segmented(options: List<Pair<T, String>>, selected: T, onSelect: (T) -> Unit, modifier: Modifier = Modifier) {
    val colors = WizardTheme.colors
    Row(
        modifier
            .fillMaxWidth()
            .clip(FieldShape)
            .background(colors.input)
            .border(1.dp, colors.border, FieldShape)
            .padding(3.dp),
    ) {
        options.forEach { (value, label) ->
            val on = value == selected
            Box(
                Modifier
                    .weight(1f)
                    .heightIn(min = 42.dp)
                    .clip(RoundedCornerShape(9.dp))
                    .background(if (on) colors.raised else Color.Transparent)
                    .then(if (on) Modifier.border(1.dp, colors.border, RoundedCornerShape(9.dp)) else Modifier)
                    .clickable(role = Role.RadioButton, onClickLabel = label) { onSelect(value) }
                    .semantics { contentDescription = if (on) "$label, selected" else label },
                contentAlignment = Alignment.Center,
            ) {
                Text(label, style = WizardTheme.type.label, color = if (on) colors.text else colors.muted)
            }
        }
    }
}

@Composable
fun ToggleRow(title: String, subtitle: String?, checked: Boolean, onChange: (Boolean) -> Unit, modifier: Modifier = Modifier, inset: Dp = 16.dp) {
    val colors = WizardTheme.colors
    Row(
        modifier
            .fillMaxWidth()
            .clickable(role = Role.Switch) { onChange(!checked) }
            .heightIn(min = 56.dp)
            .padding(horizontal = inset, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f).padding(end = 12.dp)) {
            Text(title, style = WizardTheme.type.body, color = colors.text)
            if (subtitle != null) Text(subtitle, style = WizardTheme.type.small, color = colors.faint)
        }
        Switch(
            checked = checked,
            onCheckedChange = null,
            colors = SwitchDefaults.colors(
                checkedThumbColor = colors.onSolid,
                checkedTrackColor = colors.solid,
                checkedBorderColor = colors.solid,
                uncheckedThumbColor = colors.muted,
                uncheckedTrackColor = colors.input,
                uncheckedBorderColor = colors.borderStrong,
            ),
        )
    }
}

/** Monospace text that scrolls sideways instead of wrapping, in a quiet box. */
@Composable
fun CodeBox(text: String, modifier: Modifier = Modifier, maxLines: Int = Int.MAX_VALUE, color: Color = WizardTheme.colors.text) {
    val colors = WizardTheme.colors
    Box(
        modifier
            .fillMaxWidth()
            .clip(FieldShape)
            .background(colors.code)
            .border(1.dp, colors.border, FieldShape),
    ) {
        Text(
            text,
            style = WizardTheme.type.mono,
            color = color,
            maxLines = maxLines,
            softWrap = false,
            overflow = TextOverflow.Clip,
            modifier = Modifier
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 14.dp, vertical = 12.dp),
        )
    }
}

@Composable
fun Spinner(modifier: Modifier = Modifier, size: Dp = 16.dp, color: Color = WizardTheme.colors.muted) {
    com.teddytennant.wizard.ui.life.LifeIndicator(modifier, size = size, color = color)
}

/** The wand mark from the desktop icon. */
@Composable
fun WandMark(modifier: Modifier = Modifier, size: Dp = 22.dp, tint: Color = WizardTheme.colors.text) {
    Icon(painterResource(R.drawable.ic_wizard_mark), contentDescription = null, modifier = modifier.size(size), tint = tint)
}

/** An agent's mark as the desktop draws it: monochrome, except Claude's orange. */
@Composable
fun AgentMark(agent: com.teddytennant.wizard.agent.Agent, modifier: Modifier = Modifier, size: Dp = 16.dp, tint: Color = WizardTheme.colors.text) {
    Icon(
        painterResource(agent.icon),
        contentDescription = null,
        modifier = modifier.size(size),
        tint = agent.brandColor?.let { Color(it) } ?: tint,
    )
}

/** The mark in a small rounded tile, for list rows. */
@Composable
fun AgentTile(agent: com.teddytennant.wizard.agent.Agent, modifier: Modifier = Modifier, size: Dp = 36.dp) {
    val colors = WizardTheme.colors
    val shape = RoundedCornerShape(size * 0.28f)
    Box(
        modifier.size(size).clip(shape).background(colors.raised).border(1.dp, colors.border, shape),
        contentAlignment = Alignment.Center,
    ) { AgentMark(agent, size = size * 0.46f) }
}

/** A small pill control with a 48dp touch target. */
@Composable
fun Chip(
    label: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    contentDescription: String = label,
    leading: @Composable (() -> Unit)? = null,
) {
    val colors = WizardTheme.colors
    Box(
        modifier
            .heightIn(min = 48.dp)
            .clip(PillShape)
            .clickable(role = Role.Button, onClickLabel = contentDescription, onClick = onClick)
            .semantics { this.contentDescription = contentDescription },
        contentAlignment = Alignment.Center,
    ) {
        Row(
            Modifier
                .clip(PillShape)
                .background(colors.raised.copy(alpha = if (colors.isDark) 0.9f else 1f))
                .border(1.dp, colors.border, PillShape)
                .heightIn(min = 32.dp)
                .padding(start = if (leading != null) 9.dp else 12.dp, end = 11.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (leading != null) {
                leading()
                Spacer(Modifier.width(7.dp))
            }
            Text(label, style = WizardTheme.type.label, color = colors.text, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.widthIn(max = 150.dp))
            Spacer(Modifier.width(4.dp))
            WizardIcon(R.drawable.ic_alt_arrow_down, null, size = 12.dp, tint = colors.faint)
        }
    }
}
