package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

/** "Restore interrupted agents after a restart" (sm#2054); on by default. */
@Composable
fun HostRestartSection() {
    val context = LocalContext.current
    val settings = remember(context) { SettingsRepository(context.applicationContext) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    val scope = rememberCoroutineScope()
    var enabled by remember { mutableStateOf(true) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(settings) {
        runCatching { repository.fetchOwnerSettings(settings.serverUrl.first(), settings.accessToken.first()) }
            .onSuccess { response ->
                enabled = response["host_restart"]?.jsonObject?.get("restore_agents")
                    ?.jsonPrimitive?.content?.toBooleanStrictOrNull() ?: true
            }.onFailure { error = it.message ?: "Couldn't load the restart setting" }
    }
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text("Restore interrupted agents after a restart", Modifier.weight(1f))
            Switch(checked = enabled, enabled = !busy, onCheckedChange = { on ->
                scope.launch {
                    busy = true
                    val patch = buildJsonObject { put("host_restart", buildJsonObject { put("restore_agents", on) }) }
                    repository.setOwnerSettings(settings.serverUrl.first(), settings.accessToken.first(), patch)
                        .onSuccess { enabled = on; error = null }
                        .onFailure { error = it.message ?: "Couldn't save the restart setting" }
                    busy = false
                }
            })
        }
        Text("Queue jobs are never resubmitted; each agent is told what it lost.")
        error?.let { Text(it) }
    }
}
