package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
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

@Composable
fun AutoRetireSection() {
    val context = LocalContext.current
    val settings = remember(context) { SettingsRepository(context.applicationContext) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    val scope = rememberCoroutineScope()
    var enabled by remember { mutableStateOf(true) }
    var minutes by remember { mutableStateOf("60") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    suspend fun save(on: Boolean, delay: Int) {
        busy = true
        val patch = buildJsonObject { put("auto_retire", buildJsonObject { put("enabled", on); put("idle_minutes", delay) }) }
        repository.setOwnerSettings(settings.serverUrl.first(), settings.accessToken.first(), patch)
            .onSuccess { enabled = on; minutes = delay.toString(); error = null }
            .onFailure { error = it.message ?: "Couldn't save auto-retire setting" }
        busy = false
    }
    LaunchedEffect(settings) {
        runCatching { repository.fetchOwnerSettings(settings.serverUrl.first(), settings.accessToken.first()) }
            .onSuccess { response ->
                response["auto_retire"]?.jsonObject?.let {
                    enabled = it["enabled"]?.jsonPrimitive?.content?.toBooleanStrictOrNull() ?: true
                    minutes = it["idle_minutes"]?.jsonPrimitive?.content ?: "60"
                }
            }.onFailure { error = it.message ?: "Couldn't load auto-retire setting" }
    }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text("Retire finished agents automatically", Modifier.weight(1f))
            Switch(checked = enabled, enabled = !busy, onCheckedChange = { on -> scope.launch { save(on, minutes.toIntOrNull()?.coerceIn(15, 1440) ?: 60) } })
        }
        OutlinedTextField(value = minutes, onValueChange = { minutes = it.filter(Char::isDigit).take(4) },
            label = { Text("Minutes idle (15–1440)") }, singleLine = true,
            modifier = Modifier.fillMaxWidth())
        TextButton(enabled = !busy && (minutes.toIntOrNull() ?: 0) in 15..1440,
            onClick = { scope.launch { save(enabled, minutes.toInt()) } }) { Text("Save delay") }
        error?.let { Text(it) }
    }
}
