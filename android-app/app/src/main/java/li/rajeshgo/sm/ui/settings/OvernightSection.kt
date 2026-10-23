package li.rajeshgo.sm.ui.settings

import android.app.TimePickerDialog
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObjectBuilder
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import li.rajeshgo.sm.data.model.OvernightSettings
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.overnight.OvernightController
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet

private val IDLE_CHOICES = listOf(10, 20, 30, 60)

/** "23:00" as "11:00 PM". */
fun clockLabel(value: String): String {
    val (hours, minutes) = value.split(":").mapNotNull { it.toIntOrNull() }.takeIf { it.size == 2 } ?: return value
    val suffix = if (hours < 12) "AM" else "PM"
    val hour = (hours % 12).let { if (it == 0) 12 else it }
    return "%d:%02d %s".format(hour, minutes, suffix)
}

/**
 * When sm turns overnight mode on by itself (sm#2494): the phone's Do Not
 * Disturb, and bedtime after a quiet spell. The moon button needs no setting.
 */
@Composable
fun OvernightSection() {
    val context = LocalContext.current
    val settings = remember(context) { SettingsRepository(context.applicationContext) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    val scope = rememberCoroutineScope()
    val status by OvernightController.status.collectAsState()
    var values by remember { mutableStateOf(status?.settings ?: OvernightSettings()) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) { OvernightController.refresh(context) }
    LaunchedEffect(status?.settings) { status?.settings?.let { if (!busy) values = it } }

    fun save(next: OvernightSettings, patch: JsonObjectBuilder.() -> Unit) {
        val before = values
        values = next
        scope.launch {
            busy = true
            val body = buildJsonObject { put("overnight", buildJsonObject(patch)) }
            repository.setOwnerSettings(settings.serverUrl.first(), settings.accessToken.first(), body)
                .onSuccess { error = null; OvernightController.refresh(context) }
                .onFailure { values = before; error = it.message ?: "Couldn't save" }
            busy = false
        }
    }

    fun pickTime(current: String, onPicked: (String) -> Unit) {
        val (hours, minutes) = current.split(":").mapNotNull { it.toIntOrNull() }.takeIf { it.size == 2 } ?: listOf(23, 0)
        TimePickerDialog(context, { _, h, m -> onPicked("%02d:%02d".format(h, m)) }, hours, minutes, false).show()
    }

    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Text(
            "The moon in the top bar tells agents you are away. sm can also turn it on by itself:",
            style = MaterialTheme.typography.bodySmall,
            color = TextSecondary,
        )
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f).padding(end = 12.dp)) {
                Text("When my phone is on Do Not Disturb")
                Text("Your phone silences the alerts; agents learn you're away.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            }
            Switch(checked = values.followDnd, enabled = !busy, onCheckedChange = { on ->
                save(values.copy(followDnd = on)) { put("follow_dnd", on) }
            })
        }
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f).padding(end = 12.dp)) {
                Text("After bedtime, once I go quiet")
                Text("Alerts wait until morning; one summary when the night ends.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            }
            Switch(checked = values.bedtimeEnabled, enabled = !busy, onCheckedChange = { on ->
                save(values.copy(bedtimeEnabled = on)) { put("bedtime_enabled", on) }
            })
        }
        if (values.bedtimeEnabled) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                TimeChip("Bedtime", clockLabel(values.bedtimeStart), enabled = !busy) {
                    pickTime(values.bedtimeStart) { picked ->
                        if (picked != values.bedtimeEnd) save(values.copy(bedtimeStart = picked)) { put("bedtime_start", picked) }
                    }
                }
                Text("→", color = TextMuted)
                TimeChip("Wake", clockLabel(values.bedtimeEnd), enabled = !busy) {
                    pickTime(values.bedtimeEnd) { picked ->
                        if (picked != values.bedtimeStart) save(values.copy(bedtimeEnd = picked)) { put("bedtime_end", picked) }
                    }
                }
            }
        }
        if (values.bedtimeEnabled || values.followDnd) {
            Text("Quiet for", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                IDLE_CHOICES.forEach { minutes ->
                    val selected = values.idleMinutes == minutes
                    Surface(
                        onClick = { if (!selected) save(values.copy(idleMinutes = minutes)) { put("idle_minutes", minutes) } },
                        enabled = !busy,
                        shape = RoundedCornerShape(999.dp),
                        color = if (selected) Violet.copy(alpha = 0.18f) else Color.Transparent,
                        border = BorderStroke(1.dp, if (selected) Violet.copy(alpha = 0.5f) else Border),
                    ) {
                        Text(
                            "$minutes min",
                            color = if (selected) Violet else TextSecondary,
                            style = MaterialTheme.typography.labelMedium,
                            modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
                        )
                    }
                }
            }
        }
        error?.let { Text(it, color = Rose, style = MaterialTheme.typography.bodySmall) }
    }
}

@Composable
private fun TimeChip(label: String, value: String, enabled: Boolean, onClick: () -> Unit) {
    Surface(
        onClick = onClick,
        enabled = enabled,
        shape = RoundedCornerShape(12.dp),
        color = Color.Transparent,
        border = BorderStroke(1.dp, Border),
    ) {
        Column(Modifier.padding(horizontal = 14.dp, vertical = 8.dp)) {
            Text(label, style = MaterialTheme.typography.labelSmall, color = TextMuted)
            Text(value, style = MaterialTheme.typography.titleMedium, color = Violet)
        }
    }
}
