package com.teddytennant.wizard.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import com.teddytennant.wizard.R
import com.teddytennant.wizard.data.ThemeChoice

/**
 * The desktop app's palette (Wizard GUI, from Zeron's `zeron-dark` and
 * `zeron-light`), with cards lifted a step so they read on a phone.
 */
@Immutable
data class WizardColors(
    val isDark: Boolean,
    val background: Color,
    val card: Color,
    val raised: Color,
    val input: Color,
    val text: Color,
    val muted: Color,
    val faint: Color,
    val border: Color,
    val borderStrong: Color,
    val solid: Color,
    val onSolid: Color,
    val accent: Color,
    val accentSoft: Color,
    val success: Color,
    val warning: Color,
    val danger: Color,
    val dangerSoft: Color,
    val code: Color,
    val scrim: Color,
)

val DarkColors = WizardColors(
    isDark = true,
    background = Color(0xFF060606),
    card = Color(0xFF0F0F11),
    raised = Color(0xFF17171A),
    input = Color(0xFF141417),
    text = Color(0xFFE8E8EA),
    muted = Color(0xFFA9A9AE),
    faint = Color(0xFF85858A),
    border = Color.White.copy(alpha = 0.08f),
    borderStrong = Color.White.copy(alpha = 0.16f),
    solid = Color(0xFFEBEBEF),
    onSolid = Color(0xFF0C0C0E),
    accent = Color(0xFF8B7CF6),
    accentSoft = Color(0xFF8B7CF6).copy(alpha = 0.16f),
    success = Color(0xFF34D399),
    warning = Color(0xFFFACC15),
    danger = Color(0xFFF87171),
    dangerSoft = Color(0xFFF87171).copy(alpha = 0.10f),
    code = Color(0xFF0B0B0D),
    scrim = Color.Black.copy(alpha = 0.6f),
)

val LightColors = WizardColors(
    isDark = false,
    background = Color(0xFFF7F7F8),
    card = Color(0xFFFFFFFF),
    raised = Color(0xFFEDEDF0),
    input = Color(0xFFFFFFFF),
    text = Color(0xFF232327),
    muted = Color(0xFF5E5E66),
    faint = Color(0xFF797981),
    border = Color.Black.copy(alpha = 0.08f),
    borderStrong = Color.Black.copy(alpha = 0.16f),
    solid = Color(0xFF232328),
    onSolid = Color(0xFFF7F7F8),
    accent = Color(0xFF5B43E8),
    accentSoft = Color(0xFF5B43E8).copy(alpha = 0.10f),
    success = Color(0xFF15803D),
    warning = Color(0xFFA16207),
    danger = Color(0xFFDC2626),
    dangerSoft = Color(0xFFDC2626).copy(alpha = 0.07f),
    code = Color(0xFFF1F1F4),
    scrim = Color.Black.copy(alpha = 0.35f),
)

val Geist = FontFamily(
    Font(R.font.geist_regular, FontWeight.Normal),
    Font(R.font.geist_medium, FontWeight.Medium),
    Font(R.font.geist_semibold, FontWeight.SemiBold),
)

val GeistMono = FontFamily(
    Font(R.font.geist_mono_regular, FontWeight.Normal),
    Font(R.font.geist_mono_medium, FontWeight.Medium),
)

/** Few sizes, one weight step between them. */
@Immutable
data class WizardType(
    val largeTitle: TextStyle = TextStyle(fontFamily = Geist, fontWeight = FontWeight.SemiBold, fontSize = 28.sp, lineHeight = 34.sp, letterSpacing = (-0.6).sp),
    val title: TextStyle = TextStyle(fontFamily = Geist, fontWeight = FontWeight.SemiBold, fontSize = 17.sp, lineHeight = 22.sp, letterSpacing = (-0.2).sp),
    val heading: TextStyle = TextStyle(fontFamily = Geist, fontWeight = FontWeight.Medium, fontSize = 16.sp, lineHeight = 22.sp, letterSpacing = (-0.1).sp),
    val body: TextStyle = TextStyle(fontFamily = Geist, fontWeight = FontWeight.Normal, fontSize = 15.sp, lineHeight = 23.sp),
    val small: TextStyle = TextStyle(fontFamily = Geist, fontWeight = FontWeight.Normal, fontSize = 13.sp, lineHeight = 18.sp),
    val label: TextStyle = TextStyle(fontFamily = Geist, fontWeight = FontWeight.Medium, fontSize = 13.sp, lineHeight = 18.sp, letterSpacing = 0.1.sp),
    val section: TextStyle = TextStyle(fontFamily = Geist, fontWeight = FontWeight.Medium, fontSize = 12.sp, lineHeight = 16.sp, letterSpacing = 0.3.sp),
    val mono: TextStyle = TextStyle(fontFamily = GeistMono, fontWeight = FontWeight.Normal, fontSize = 12.5.sp, lineHeight = 19.sp),
    val monoSmall: TextStyle = TextStyle(fontFamily = GeistMono, fontWeight = FontWeight.Normal, fontSize = 12.sp, lineHeight = 17.sp),
)

val LocalWizardColors = staticCompositionLocalOf { DarkColors }
val LocalWizardType = staticCompositionLocalOf { WizardType() }

object WizardTheme {
    val colors: WizardColors
        @Composable @ReadOnlyComposable get() = LocalWizardColors.current
    val type: WizardType
        @Composable @ReadOnlyComposable get() = LocalWizardType.current
}

@Composable
fun isDark(choice: ThemeChoice): Boolean = when (choice) {
    ThemeChoice.Dark -> true
    ThemeChoice.Light -> false
    ThemeChoice.System -> isSystemInDarkTheme()
}

@Composable
fun WizardTheme(dark: Boolean = true, content: @Composable () -> Unit) {
    val colors = if (dark) DarkColors else LightColors
    val type = WizardType()
    val scheme = if (dark) {
        darkColorScheme(
            primary = colors.solid, onPrimary = colors.onSolid,
            secondary = colors.accent, onSecondary = Color.White,
            background = colors.background, onBackground = colors.text,
            surface = colors.card, onSurface = colors.text,
            surfaceVariant = colors.raised, onSurfaceVariant = colors.muted,
            surfaceContainerLowest = colors.background, surfaceContainerLow = colors.card,
            surfaceContainer = colors.card, surfaceContainerHigh = colors.raised, surfaceContainerHighest = colors.raised,
            outline = colors.borderStrong, outlineVariant = colors.border,
            error = colors.danger, onError = Color.Black, scrim = colors.scrim,
        )
    } else {
        lightColorScheme(
            primary = colors.solid, onPrimary = colors.onSolid,
            secondary = colors.accent, onSecondary = Color.White,
            background = colors.background, onBackground = colors.text,
            surface = colors.card, onSurface = colors.text,
            surfaceVariant = colors.raised, onSurfaceVariant = colors.muted,
            surfaceContainerLowest = colors.card, surfaceContainerLow = colors.card,
            surfaceContainer = colors.card, surfaceContainerHigh = colors.card, surfaceContainerHighest = colors.raised,
            outline = colors.borderStrong, outlineVariant = colors.border,
            error = colors.danger, onError = Color.White, scrim = colors.scrim,
        )
    }
    val typography = Typography(
        bodyLarge = type.body, bodyMedium = type.body, bodySmall = type.small,
        titleLarge = type.title, titleMedium = type.heading, titleSmall = type.label,
        labelLarge = type.label, labelMedium = type.label, labelSmall = type.section,
        headlineSmall = type.largeTitle,
    )
    CompositionLocalProvider(LocalWizardColors provides colors, LocalWizardType provides type) {
        MaterialTheme(colorScheme = scheme, typography = typography, content = content)
    }
}
