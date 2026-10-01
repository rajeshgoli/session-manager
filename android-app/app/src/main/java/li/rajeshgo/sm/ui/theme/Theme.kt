package li.rajeshgo.sm.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.isSpecified
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.repository.TEXT_SCALE_DEFAULT

private val SessionManagerColorScheme = darkColorScheme(
    primary = Cyan,
    secondary = Emerald,
    tertiary = Violet,
    background = InkBlack,
    surface = Panel,
    surfaceVariant = PanelMuted,
    onPrimary = InkBlack,
    onSecondary = InkBlack,
    onTertiary = InkBlack,
    onBackground = TextPrimary,
    onSurface = TextPrimary,
    onSurfaceVariant = TextSecondary,
    outline = Border,
    error = Rose,
)

@Composable
fun SessionManagerTheme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val settings = remember(context) { SettingsRepository(context.applicationContext) }
    val scale by settings.textScale.collectAsState(initial = TEXT_SCALE_DEFAULT)
    val typography = remember(scale) { scaledTypography(SessionManagerTypography, scale) }
    MaterialTheme(
        colorScheme = SessionManagerColorScheme,
        typography = typography,
        content = content,
    )
}

/** Every style's font size and line height times [scale], the Text size setting (spec 1782 A1). */
fun scaledTypography(base: Typography, scale: Float): Typography {
    if (scale == 1f) return base
    fun TextUnit.times(): TextUnit = if (isSpecified) this * scale else this
    fun TextStyle.scaled(): TextStyle = copy(fontSize = fontSize.times(), lineHeight = lineHeight.times())
    return base.copy(
        displayLarge = base.displayLarge.scaled(),
        displayMedium = base.displayMedium.scaled(),
        displaySmall = base.displaySmall.scaled(),
        headlineLarge = base.headlineLarge.scaled(),
        headlineMedium = base.headlineMedium.scaled(),
        headlineSmall = base.headlineSmall.scaled(),
        titleLarge = base.titleLarge.scaled(),
        titleMedium = base.titleMedium.scaled(),
        titleSmall = base.titleSmall.scaled(),
        bodyLarge = base.bodyLarge.scaled(),
        bodyMedium = base.bodyMedium.scaled(),
        bodySmall = base.bodySmall.scaled(),
        labelLarge = base.labelLarge.scaled(),
        labelMedium = base.labelMedium.scaled(),
        labelSmall = base.labelSmall.scaled(),
    )
}
