package li.rajeshgo.sm.ui.handoff

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import kotlinx.coroutines.delay
import kotlinx.serialization.json.*
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

internal fun percentLabel(value: Double): String =
    if (value % 1.0 == 0.0) "${value.toInt()}%" else "$value%"

internal fun thresholdOptions(current: Double): List<Double> =
    ((5..95 step 5).map(Int::toDouble) + current).distinct().sorted()

fun handoffSummary(session: ClientSession): String? = session.handoff?.let { policy ->
    listOfNotNull(session.contextPercent?.let { "ctx ${percentLabel(it)}" }, policy.display).joinToString(" · ")
}

@Composable
private fun rememberHandoffEditor(sessionId: String?): HandoffViewModel {
    val context = LocalContext.current.applicationContext
    val settings = remember(context) { SettingsRepository(context) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    val url by settings.serverUrl.collectAsState(initial = "")
    val token by settings.accessToken.collectAsState(initial = "")
    // A keyed composition owns each connection's requests, including cancellation.
    return key(url, token, sessionId) {
        val scope = rememberCoroutineScope()
        val editor = remember {
            fun requireLogin() = check(url.isNotBlank() && token.isNotBlank()) { "Sign in to manage handoff settings." }
            HandoffViewModel(scope, read = {
                requireLogin()
                if (sessionId == null) HandoffUiState(defaults = repository.fetchHandoffDefaults(url, token))
                else HandoffUiState(policy = repository.fetchHandoffPolicy(url, token, sessionId))
            }, write = { patch ->
                requireLogin()
                if (sessionId == null) HandoffUiState(defaults = repository.setHandoffDefaults(url, token, patch).getOrThrow())
                else HandoffUiState(policy = repository.setHandoffPolicy(url, token, sessionId, patch).getOrThrow())
            })
        }
        val lifecycle = LocalLifecycleOwner.current
        LaunchedEffect(editor, lifecycle) {
            if (url.isNotBlank() && token.isNotBlank()) {
                lifecycle.lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
                    while (true) { editor.refresh(); delay(10_000) }
                }
            }
        }
        editor
    }
}

@Composable
fun ContextHandoffSection(session: ClientSession) {
    val editor = rememberHandoffEditor(session.id)
    val state by editor.state.collectAsState()
    var confirm by remember(session.id) { mutableStateOf(false) }
    val policy = state.policy
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text("Context handoff", style = MaterialTheme.typography.titleSmall)
        Text(handoffSummary(session) ?: "Loading handoff settings…", style = MaterialTheme.typography.bodySmall)
        if (policy != null) {
            val enabled = !state.saving && session.status != "stopped"
            HandoffSwitch("Automatic handoff", policy.enabled, enabled) {
                editor.update(buildJsonObject { put("enabled", it) })
            }
            PercentPicker("Threshold", policy.thresholdPercent, thresholdOptions(policy.thresholdPercent), enabled) {
                editor.update(buildJsonObject { put("threshold_percent", it) })
            }
            Text(if (policy.source == "override") "Custom settings for this agent" else "Using defaults", style = MaterialTheme.typography.bodySmall)
            if (!policy.hasGauge) Text("No context reading; review requests and Hand off now can still ask this agent to hand off.", style = MaterialTheme.typography.bodySmall)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                TextButton(enabled = enabled, onClick = { editor.update(buildJsonObject { put("use_default", true) }) }) { Text("Use default") }
                TextButton(enabled = enabled, onClick = { confirm = true }) { Text("Hand off now") }
            }
        }
        EditorStatus(state, editor::refresh)
    }
    if (confirm) AlertDialog(
        onDismissRequest = { confirm = false },
        title = { Text("Hand off ${session.friendlyName ?: session.name}?") },
        text = { Text("Ask this agent to stop at a logical point and write a handoff note. A fresh agent will take over after it agrees to hand off.") },
        confirmButton = { TextButton(enabled = !state.saving, onClick = {
            confirm = false
            editor.update(buildJsonObject { put("ask_now", true) })
        }) { Text("Hand off now") } },
        dismissButton = { TextButton(onClick = { confirm = false }) { Text("Cancel") } },
    )
}

@Composable
fun HandoffDefaultsSection() {
    val editor = rememberHandoffEditor(null)
    val state by editor.state.collectAsState()
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text("Ask agents to prepare a fresh agent when they reach a stopping point. Custom agent settings take precedence.", style = MaterialTheme.typography.bodySmall)
        state.defaults?.let { defaults ->
            val enabled = !state.saving
            // Include plain Codex and any future providers returned by the server.
            (listOf("claude", "codex-fork", "codex-app", "codex") + defaults.providers.keys).distinct().forEach { provider ->
                HandoffSwitch(provider, defaults.providers[provider] ?: false, enabled) {
                    editor.update(buildJsonObject { put("providers", buildJsonObject { put(provider, it) }) })
                }
            }
            PercentPicker("Threshold", defaults.thresholdPercent, defaultPercentOptions(defaults.thresholdPercent, false), enabled) {
                editor.update(buildJsonObject { put("threshold_percent", it) })
            }
            PercentPicker("Review floor", defaults.reviewFloorPercent, defaultPercentOptions(defaults.reviewFloorPercent, true), enabled) {
                editor.update(buildJsonObject { put("review_floor_percent", it) })
            }
            Text("Minimum context usage before a review request asks for handoff.", style = MaterialTheme.typography.bodySmall)
            PercentPicker("Reminder at", defaults.reminderPercent, defaultPercentOptions(defaults.reminderPercent, false), enabled) {
                editor.update(buildJsonObject { put("reminder_percent", it) })
            }
            Text("One reminder, only when this is above the agent's threshold.", style = MaterialTheme.typography.bodySmall)
            HandoffSwitch("Ask on Codex review request", defaults.askOnCodexReview, enabled) {
                editor.update(buildJsonObject { put("ask_on_codex_review", it) })
            }
            HandoffSwitch("Ask on document review request", defaults.askOnDocReview, enabled) {
                editor.update(buildJsonObject { put("ask_on_doc_review", it) })
            }
        }
        EditorStatus(state, editor::refresh)
    }
}

internal fun defaultPercentOptions(current: Double, allowZero: Boolean): List<Double> =
    (((if (allowZero) 0 else 1)..100).map(Int::toDouble) + current).distinct().sorted()

@Composable
private fun EditorStatus(state: HandoffUiState, retry: () -> Unit) {
    if (state.saving) Text("Saving…", style = MaterialTheme.typography.bodySmall)
    else if (state.loading && state.policy == null && state.defaults == null) Text("Loading…")
    state.error?.let {
        Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
        TextButton(onClick = retry, enabled = !state.saving && !state.loading) { Text("Refresh") }
    }
}

@Composable
private fun HandoffSwitch(label: String, checked: Boolean, enabled: Boolean, onChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Text(label, Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
        Switch(checked = checked, onCheckedChange = onChange, enabled = enabled)
    }
}

@Composable
private fun PercentPicker(label: String, value: Double, options: List<Double>, enabled: Boolean, onChange: (Double) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Text(label, Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
        Box {
            OutlinedButton(enabled = enabled, onClick = { open = true }) { Text(percentLabel(value)) }
            DropdownMenu(expanded = open && enabled, onDismissRequest = { open = false }) {
                options.forEach { option ->
                    DropdownMenuItem(text = { Text(percentLabel(option)) }, onClick = { open = false; onChange(option) })
                }
            }
        }
    }
}
