package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.ui.theme.*

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostStatusSheet(state: SettingsUiState, onRefresh: () -> Unit, onClose: () -> Unit) {
    // A single read when opened. Refresh is exclusively user initiated.
    LaunchedEffect(Unit) { onRefresh() }
    ModalBottomSheet(onDismissRequest = onClose, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(24.dp), verticalArrangement = Arrangement.spacedBy(20.dp)) {
            Text("Host status", style = MaterialTheme.typography.headlineSmall)
            state.hostStatus?.host?.let { Text(it, color = TextMuted) }
            if (state.hostLoading) LinearProgressIndicator(Modifier.fillMaxWidth())
            state.hostError?.let { Text(it, color = Rose) }
            val host = state.hostStatus
            if (host != null && !state.hostLoading) {
                if (!host.available) Text("Host statistics are unavailable.", color = TextMuted)
                val memory = if (host.memoryUsedBytes != null && host.memoryTotalBytes != null) "${gib(host.memoryUsedBytes)} / ${gib(host.memoryTotalBytes)} GB" else "Unavailable"
                HostMetric("Physical memory", memory, "Includes cached memory")
                HostMetric("Memory pressure", host.memoryPressure ?: "Unavailable")
                HostMetric("CPU", percent(host.cpuPercent))
                HostMetric("GPU", percent(host.gpuPercent))
                Text("Snapshot · ${li.rajeshgo.sm.ui.watch.summaryAgeLabel(host.sampledAt)}", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            }
            OutlinedButton(onClick = onRefresh, enabled = !state.hostLoading, modifier = Modifier.fillMaxWidth()) { Text("Refresh snapshot") }
        }
    }
}

private fun gib(bytes: Long): String = "%.1f".format(bytes.toDouble() / (1024 * 1024 * 1024))
private fun percent(value: Double?): String = value?.let { "%.1f%%".format(it) } ?: "Unavailable"

@Composable
private fun HostMetric(title: String, value: String, note: String? = null) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Text(title, style = MaterialTheme.typography.labelLarge, color = TextMuted)
        Text(value, style = MaterialTheme.typography.headlineSmall, color = when (value) { "Normal" -> Emerald; "Elevated" -> Amber; "Critical" -> Rose; else -> TextSecondary })
        note?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = TextMuted) }
    }
}
