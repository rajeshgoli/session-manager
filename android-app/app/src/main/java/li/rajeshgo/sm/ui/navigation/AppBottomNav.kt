package li.rajeshgo.sm.ui.navigation

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ViewList
import androidx.compose.material.icons.rounded.HourglassTop
import androidx.compose.material.icons.rounded.Inbox
import androidx.compose.material.icons.rounded.ViewKanban
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.ui.board.BoardBadge
import li.rajeshgo.sm.ui.inbox.InboxBadge
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.BorderStrong
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.PanelElevated
import li.rajeshgo.sm.ui.theme.TextMuted

@Composable
fun AppBottomNav(
    currentRoute: String,
    onInbox: () -> Unit,
    onWatch: () -> Unit,
    onBoard: () -> Unit,
    onQueue: () -> Unit,
    modifier: Modifier = Modifier,
    queueBadge: Int = 0,
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
                .padding(horizontal = 6.dp, vertical = 6.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            val inboxBadge = InboxBadge.needsYou
            AppBottomNavItem(
                label = when {
                    inboxBadge > 0 -> "Inbox · $inboxBadge"
                    InboxBadge.hasNew -> "Inbox •"
                    else -> "Inbox"
                },
                selected = currentRoute == Routes.INBOX,
                icon = { Icon(Icons.Rounded.Inbox, contentDescription = null, modifier = Modifier.size(18.dp), tint = if (inboxBadge > 0) Amber else androidx.compose.material3.LocalContentColor.current) },
                onClick = onInbox,
            )
            AppBottomNavItem(
                label = "Watch",
                selected = currentRoute == Routes.WATCH,
                icon = { Icon(Icons.AutoMirrored.Rounded.ViewList, contentDescription = null, modifier = Modifier.size(18.dp)) },
                onClick = onWatch,
            )
            val boardBadge = BoardBadge.count
            AppBottomNavItem(
                label = if (boardBadge > 0) "Board · $boardBadge" else "Board",
                selected = currentRoute == Routes.BOARD,
                icon = { Icon(Icons.Rounded.ViewKanban, contentDescription = null, modifier = Modifier.size(18.dp), tint = if (boardBadge > 0) Amber else androidx.compose.material3.LocalContentColor.current) },
                onClick = onBoard,
            )
            AppBottomNavItem(
                label = if (queueBadge > 0) "Queue · $queueBadge" else "Queue",
                selected = currentRoute == Routes.QUEUE,
                icon = { Icon(Icons.Rounded.HourglassTop, contentDescription = null, modifier = Modifier.size(18.dp)) },
                onClick = onQueue,
            )
        }
    }
}

/** Icon over label, each tab an equal share of the bar, so four tabs fit a phone. */
@Composable
private fun RowScope.AppBottomNavItem(
    label: String,
    selected: Boolean,
    icon: @Composable () -> Unit,
    onClick: () -> Unit,
) {
    val textColor = if (selected) MaterialTheme.colorScheme.onSurface else TextMuted
    val iconTint = if (selected) Cyan else TextMuted
    Column(
        modifier = Modifier
            .weight(1f)
            .background(
                color = if (selected) MaterialTheme.colorScheme.surface.copy(alpha = 0.55f) else androidx.compose.ui.graphics.Color.Transparent,
                shape = RoundedCornerShape(14.dp),
            )
            .clickable(onClick = onClick)
            .padding(horizontal = 4.dp, vertical = 8.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(3.dp),
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
            maxLines = 1,
        )
    }
}
