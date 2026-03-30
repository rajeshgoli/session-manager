package li.rajeshgo.sm.ui.navigation

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ViewList
import androidx.compose.material.icons.rounded.Analytics
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.ui.theme.BorderStrong
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.PanelElevated
import li.rajeshgo.sm.ui.theme.TextMuted

@Composable
fun AppBottomNav(
    currentRoute: String,
    onWatch: () -> Unit,
    onAnalytics: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Surface(
        modifier = modifier,
        color = PanelElevated,
        shape = RoundedCornerShape(18.dp),
        border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong),
        tonalElevation = 0.dp,
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 8.dp, vertical = 8.dp),
            horizontalArrangement = Arrangement.SpaceEvenly,
        ) {
            AppBottomNavItem(
                label = "Watch",
                selected = currentRoute == Routes.WATCH,
                icon = { Icon(Icons.AutoMirrored.Rounded.ViewList, contentDescription = null, modifier = Modifier.size(18.dp)) },
                onClick = onWatch,
            )
            AppBottomNavItem(
                label = "Analytics",
                selected = currentRoute == Routes.ANALYTICS,
                icon = { Icon(Icons.Rounded.Analytics, contentDescription = null, modifier = Modifier.size(18.dp)) },
                onClick = onAnalytics,
            )
        }
    }
}

@Composable
private fun AppBottomNavItem(
    label: String,
    selected: Boolean,
    icon: @Composable () -> Unit,
    onClick: () -> Unit,
) {
    val textColor = if (selected) MaterialTheme.colorScheme.onSurface else TextMuted
    val iconTint = if (selected) Cyan else TextMuted
    Row(
        modifier = Modifier
            .background(
                color = if (selected) MaterialTheme.colorScheme.surface.copy(alpha = 0.55f) else androidx.compose.ui.graphics.Color.Transparent,
                shape = RoundedCornerShape(14.dp),
            )
            .clickable(onClick = onClick)
            .padding(horizontal = 18.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        androidx.compose.runtime.CompositionLocalProvider(
            androidx.compose.material3.LocalContentColor provides iconTint,
            content = icon,
        )
        Text(
            text = label,
            color = textColor,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Medium,
        )
    }
}
