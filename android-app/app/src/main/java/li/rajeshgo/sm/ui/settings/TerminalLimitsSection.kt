package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import kotlinx.serialization.json.*
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.theme.TextMuted

/** One terminal limit: its settings key, label, allowed range and hint. Matches the server's check. */
internal data class TerminalLimit(val key: String, val label: String, val min: Long, val max: Long, val hint: String)

internal val TERMINAL_LIMITS = listOf(
    TerminalLimit("per_user", "Open terminals (you)", 1, 256, "Across the web and the phone."),
    TerminalLimit("per_session", "Viewers per agent", 1, 256, "Terminals showing the same agent at once."),
    TerminalLimit("global", "Open terminals (everyone)", 1, 256, "All terminals on this server."),
    TerminalLimit("max_attach_seconds", "Longest session (seconds)", 60, 86_400, "A terminal closes after this long."),
)

/** The `terminal_limits` patch for the edited fields; blank restores config's value. Throws with a message naming the bad field. */
internal fun terminalLimitPatch(drafts: Map<String, String>): JsonObject = buildJsonObject {
    put("terminal_limits", buildJsonObject {
        TERMINAL_LIMITS.forEach { limit ->
            val draft = drafts[limit.key]?.trim() ?: return@forEach
            if (draft.isEmpty()) {
                put(limit.key, JsonNull)
            } else {
                val value = draft.toLongOrNull()
                require(value != null && value in limit.min..limit.max) {
                    "${limit.label}: enter a whole number from ${limit.min} to ${limit.max}."
                }
                put(limit.key, value)
            }
        }
    })
}

/** Terminal attach limits, shared with the web Settings page (sm#1763). */
@Composable
fun TerminalLimitsSection() {
    val context = LocalContext.current.applicationContext
    val settings = remember(context) { SettingsRepository(context) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    val url by settings.serverUrl.collectAsState(initial = "")
    val token by settings.accessToken.collectAsState(initial = "")
    val scope = rememberCoroutineScope()
    var loaded by remember(url, token) { mutableStateOf<JsonObject?>(null) }
    val drafts = remember(url, token) { mutableStateMapOf<String, String>() }
    var status by remember(url, token) { mutableStateOf<String?>(null) }
    var saving by remember(url, token) { mutableStateOf(false) }

    fun apply(result: JsonObject) {
        loaded = result
        drafts.clear()
    }
    LaunchedEffect(url, token) {
        if (url.isBlank() || token.isBlank()) return@LaunchedEffect
        try {
            apply(repository.fetchOwnerSettings(url, token))
        } catch (error: CancellationException) {
            throw error
        } catch (error: Exception) {
            status = error.message ?: "Couldn't load terminal settings."
        }
    }

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(
            "Applies to the next terminal you open. Leave a field blank to use the configured value. " +
                "A terminal that stops answering for 30 seconds closes on its own.",
            style = MaterialTheme.typography.bodySmall, color = TextMuted,
        )
        val current = loaded
        if (current == null) {
            Text(status ?: "Loading…", style = MaterialTheme.typography.bodySmall)
            return@Column
        }
        val owner = current["terminal_limits"]?.jsonObject
        val config = current["terminal_config_limits"]?.jsonObject
        TERMINAL_LIMITS.forEach { limit ->
            val saved = owner?.get(limit.key)?.jsonPrimitive?.longOrNull?.toString() ?: ""
            OutlinedTextField(
                value = drafts[limit.key] ?: saved,
                onValueChange = { drafts[limit.key] = it; status = null },
                label = { Text(limit.label) },
                placeholder = { Text(config?.get(limit.key)?.jsonPrimitive?.content ?: "") },
                supportingText = { Text(limit.hint) },
                singleLine = true,
                enabled = !saving,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                modifier = Modifier.fillMaxWidth(),
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(enabled = drafts.isNotEmpty() && !saving, onClick = {
                val patch = try {
                    terminalLimitPatch(drafts)
                } catch (error: IllegalArgumentException) {
                    status = error.message
                    return@Button
                }
                saving = true
                status = "Saving…"
                scope.launch {
                    repository.setOwnerSettings(url, token, patch)
                        .onSuccess { apply(it); status = "Saved" }
                        .onFailure { status = it.message ?: "Couldn't save terminal settings." }
                    saving = false
                }
            }) { Text("Save") }
        }
        status?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
    }
}
