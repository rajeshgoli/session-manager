package li.rajeshgo.sm.ui.navigation

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ArrowBack
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.Analytics
import androidx.compose.material.icons.rounded.AutoStories
import androidx.compose.material.icons.rounded.History
import androidx.compose.material.icons.rounded.MoreVert
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.update.SettingsIconButtonWithUpdate
import li.rajeshgo.sm.ui.update.UpdateAvailabilityViewModel

/** Where the three-dots menu goes; the same on every screen (sm#1659). */
class AppMenuActions(
    val onNewSession: () -> Unit,
    val onOpenHistory: () -> Unit,
    val onOpenGuestbook: () -> Unit,
    val onOpenAnalytics: () -> Unit,
    val onOpenSettings: () -> Unit,
)

/**
 * The top bar every screen shares: title and status line, the three-dots
 * menu, and Settings with its update dot. Pages opened from the menu pass
 * [onBack]; [current] hides the menu entry for the page already showing.
 */
@Composable
fun AppTopBar(
    title: String,
    menu: AppMenuActions,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    busy: Boolean = false,
    current: String? = null,
    onBack: (() -> Unit)? = null,
    onRefresh: (() -> Unit)? = null,
    /** Screen buttons before the menu, such as the Board's add lane. */
    actions: (@Composable () -> Unit)? = null,
    updateViewModel: UpdateAvailabilityViewModel = viewModel(),
) {
    val update by updateViewModel.uiState.collectAsState()
    var menuExpanded by remember { mutableStateOf(false) }

    Surface(
        modifier = modifier,
        shape = RoundedCornerShape(18.dp),
        color = Panel,
        border = BorderStroke(1.dp, Border),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = if (onBack != null) 4.dp else 14.dp, end = 4.dp, top = 10.dp, bottom = 10.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (onBack != null) {
                IconButton(onClick = onBack) {
                    Icon(Icons.AutoMirrored.Rounded.ArrowBack, contentDescription = "Back", tint = TextSecondary)
                }
            }
            Column(Modifier.weight(1f)) {
                Text(
                    title,
                    style = MaterialTheme.typography.titleLarge,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (!subtitle.isNullOrBlank()) {
                    Spacer(Modifier.height(2.dp))
                    Text(
                        text = subtitle,
                        style = MaterialTheme.typography.labelSmall,
                        color = if (busy) Cyan else TextMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            actions?.invoke()
            Box {
                IconButton(onClick = { menuExpanded = true }) {
                    Icon(Icons.Rounded.MoreVert, contentDescription = "Menu", tint = if (busy) Cyan else TextSecondary)
                }
                DropdownMenu(expanded = menuExpanded, onDismissRequest = { menuExpanded = false }) {
                    MenuEntry("New session", Icons.Rounded.Add) { menuExpanded = false; menu.onNewSession() }
                    if (current != Routes.HISTORY) {
                        MenuEntry("History", Icons.Rounded.History) { menuExpanded = false; menu.onOpenHistory() }
                    }
                    if (current != Routes.GUESTBOOK) {
                        MenuEntry("Guestbook", Icons.Rounded.AutoStories) { menuExpanded = false; menu.onOpenGuestbook() }
                    }
                    if (current != Routes.ANALYTICS) {
                        MenuEntry("Analytics", Icons.Rounded.Analytics) { menuExpanded = false; menu.onOpenAnalytics() }
                    }
                    if (onRefresh != null) {
                        MenuEntry(if (busy) "Refresh (running...)" else "Refresh", Icons.Rounded.Refresh, enabled = !busy) {
                            menuExpanded = false
                            onRefresh()
                        }
                    }
                }
            }
            SettingsIconButtonWithUpdate(hasUpdate = update.availableUpdate != null, onClick = menu.onOpenSettings)
        }
    }
}

@Composable
private fun MenuEntry(
    label: String,
    icon: androidx.compose.ui.graphics.vector.ImageVector,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    DropdownMenuItem(
        text = { Text(label) },
        leadingIcon = { Icon(icon, contentDescription = null, tint = TextSecondary) },
        onClick = onClick,
        enabled = enabled,
    )
}
