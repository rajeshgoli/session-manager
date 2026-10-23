package li.rajeshgo.sm.ui.overnight

import android.widget.Toast
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Bedtime
import androidx.compose.material.icons.rounded.Bedtime
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.scale
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.OvernightStatus
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet

/** Night sky behind the banner: deep indigo into the panel colour. */
private val NightDeep = Color(0xFF1B1636)
private val NightPanel = Color(0xFF14121F)
private val Moonlight = Color(0xFFC4B5FD)

/** Whether the moon shows on: a toggle in flight wins over the last reply. */
@Composable
private fun overnightOn(): Pair<Boolean, OvernightStatus?> {
    val status by OvernightController.status.collectAsState()
    val pending by OvernightController.pending.collectAsState()
    return (pending ?: status?.on ?: false) to status
}

/**
 * The moon in every top bar (sm#2494). One tap: agents are told you are
 * away and alerts wait; tap again when you are back.
 */
@Composable
fun OvernightButton() {
    val context = LocalContext.current
    val haptics = LocalHapticFeedback.current
    val scope = rememberCoroutineScope()
    val (on, status) = overnightOn()
    val glow by animateColorAsState(if (on) Violet.copy(alpha = 0.18f) else Color.Transparent, tween(320), label = "moon glow")
    val tint by animateColorAsState(if (on) Moonlight else TextSecondary, tween(320), label = "moon tint")
    val pop by animateFloatAsState(if (on) 1f else 0.92f, spring(dampingRatio = 0.45f, stiffness = 380f), label = "moon pop")
    IconButton(
        onClick = {
            haptics.performHapticFeedback(HapticFeedbackType.LongPress)
            // Turning off while only bedtime or DND holds it on still works: sm dismisses that night.
            val next = !on
            scope.launch {
                OvernightController.set(context, next)?.let { Toast.makeText(context, it, Toast.LENGTH_SHORT).show() }
            }
        },
        modifier = Modifier.semantics {
            contentDescription = "Overnight mode"
            stateDescription = if (on) "On" else "Off"
        },
    ) {
        Box(
            Modifier.size(34.dp).background(glow, CircleShape),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                if (on) Icons.Rounded.Bedtime else Icons.Outlined.Bedtime,
                contentDescription = null,
                tint = tint,
                modifier = Modifier.size(22.dp).scale(if (on) pop else 1f),
            )
            // A dot when sm turned it on by itself, so the owner knows it was not the button.
            if (on && status?.manual == false && status.away != null) {
                Box(Modifier.align(Alignment.TopEnd).padding(top = 5.dp, end = 5.dp).size(6.dp).background(Moonlight, CircleShape))
            }
        }
    }
}

/** Why overnight is on, in the banner's first line. */
fun overnightHeadline(status: OvernightStatus): String {
    val away = status.away ?: return "Overnight"
    return when (away.source) {
        "bedtime" -> "Overnight · past bedtime"
        "dnd" -> "Overnight · Do Not Disturb"
        else -> "Overnight"
    } + if (away.sinceLabel.isNotBlank()) " · since ${away.sinceLabel}" else ""
}

/** What sm does about it, in the banner's second line. */
fun overnightDetail(status: OvernightStatus): String {
    val away = status.away ?: return "Agents work on their own; alerts wait."
    val alerts = if (away.holdsAlerts) "alerts wait" else "your phone silences alerts"
    val until = away.untilLabel?.takeIf { it.isNotBlank() }?.let { " until $it" }.orEmpty()
    return "Agents work on their own; $alerts$until."
}

/** What waits: blocked agents first, then unread messages; null when nothing does. */
fun overnightWaitingLine(status: OvernightStatus): String? {
    val waiting = status.waiting ?: return null
    val parts = mutableListOf<String>()
    when (waiting.blocked.size) {
        0 -> {}
        1 -> parts += "${waiting.blocked.first()} is blocked on you"
        else -> parts += "${waiting.blocked.size} agents blocked on you"
    }
    when (waiting.unread) {
        0 -> {}
        1 -> parts += "1 new message"
        else -> parts += "${waiting.unread} new messages"
    }
    return parts.takeIf { it.isNotEmpty() }?.joinToString(" · ")
}

/**
 * The night banner under every top bar while overnight is on: why, what
 * waits, and "I'm back". It slides in when the moon is tapped, which is the
 * confirmation that it took.
 */
@Composable
fun OvernightBanner(modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val (on, status) = overnightOn()
    AnimatedVisibility(
        visible = on,
        enter = expandVertically(tween(280)) + fadeIn(tween(280)),
        exit = shrinkVertically(tween(220)) + fadeOut(tween(160)),
        modifier = modifier,
    ) {
        val shown = status?.takeIf { it.on } ?: OvernightStatus(on = true)
        Surface(
            shape = RoundedCornerShape(16.dp),
            color = Color.Transparent,
            border = BorderStroke(1.dp, Violet.copy(alpha = 0.32f)),
            modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
        ) {
            Row(
                Modifier
                    .background(Brush.horizontalGradient(listOf(NightDeep, NightPanel)))
                    .padding(start = 14.dp, end = 6.dp, top = 10.dp, bottom = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(
                    Modifier.size(36.dp).background(Violet.copy(alpha = 0.16f), CircleShape),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(Icons.Rounded.Bedtime, contentDescription = null, tint = Moonlight, modifier = Modifier.size(20.dp))
                }
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(1.dp)) {
                    Text(
                        overnightHeadline(shown),
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = Moonlight,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        overnightDetail(shown),
                        style = MaterialTheme.typography.bodySmall,
                        color = TextSecondary,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                    overnightWaitingLine(shown)?.let { line ->
                        Spacer(Modifier.height(2.dp))
                        Text(
                            line,
                            style = MaterialTheme.typography.labelMedium,
                            color = if (shown.waiting?.blocked.isNullOrEmpty()) TextMuted else Rose,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                TextButton(
                    onClick = {
                        scope.launch {
                            OvernightController.set(context, false)?.let { Toast.makeText(context, it, Toast.LENGTH_SHORT).show() }
                        }
                    },
                    contentPadding = PaddingValues(horizontal = 12.dp),
                ) { Text("I'm back", color = Moonlight, fontWeight = FontWeight.SemiBold) }
            }
        }
    }
}
