package li.rajeshgo.sm.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable

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
    MaterialTheme(
        colorScheme = SessionManagerColorScheme,
        typography = SessionManagerTypography,
        content = content,
    )
}
