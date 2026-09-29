package li.rajeshgo.sm.ui.handoff

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import kotlinx.coroutines.delay
import kotlinx.serialization.json.*
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import kotlin.math.roundToInt

internal fun percentLabel(value: Double): String =
    if (value % 1.0 == 0.0) "${value.toInt()}%" else "$value%"

internal fun thresholdOptions(current: Double): List<Double> =
    ((5..95 step 5).map(Int::toDouble) + current).distinct().sorted()

fun handoffSummary(session: ClientSession): String? = session.handoff?.let { policy ->
    listOfNotNull(contextGaugePercent(session.contextPercent)?.let { "Context $it%" }, policy.display).joinToString(" · ")
}

internal fun contextGaugePercent(value: Double?): Int? =
    value?.takeIf { it.isFinite() }?.coerceIn(0.0, 100.0)?.roundToInt()

@Composable
fun ContextHandoffStatus(session: ClientSession, contextPercent: Double? = session.contextPercent) {
    val percent = contextGaugePercent(contextPercent)
    val display = session.handoff?.display
    if (percent == null && display == null) return
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        if (percent != null) {
            val progress = (contextPercent!! / 100.0).coerceIn(0.0, 1.0).toFloat()
            val tint = if (progress <= 0.5f) lerp(Emerald, Amber, progress * 2f)
                else lerp(Amber, Rose, (progress - 0.5f) * 2f)
            Box(Modifier.size(36.dp).clearAndSetSemantics { contentDescription = "Context $percent percent" }, contentAlignment = Alignment.Center) {
                CircularProgressIndicator(
                    progress = { progress },
                    modifier = Modifier.fillMaxSize(), color = tint, trackColor = Border,
                    strokeWidth = 3.dp, strokeCap = StrokeCap.Round,
                )
                Text("$percent", fontSize = 12.sp, fontWeight = FontWeight.SemiBold, color = TextSecondary)
            }
        }
        Column(verticalArrangement = Arrangement.spacedBy(1.dp)) {
            if (percent != null) Text("Context", style = MaterialTheme.typography.labelSmall, color = TextMuted)
            display?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = TextSecondary) }
        }
    }
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
fun ContextHandoffDialog(session: ClientSession, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Context handoff") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                ContextHandoffSection(session)
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Done") } },
    )
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ContextHandoffSection(session: ClientSession) {
    val editor = rememberHandoffEditor(session.id)
    val state by editor.state.collectAsState()
    var confirm by remember(session.id) { mutableStateOf(false) }
    val policy = state.policy
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(policy?.display ?: session.handoff?.display ?: "Loading handoff settings…", style = MaterialTheme.typography.bodySmall)
        if (policy != null) {
            val enabled = !state.saving && session.status != "stopped"
            HandoffSwitch("Automatic handoff", policy.enabled, enabled) {
                editor.update(buildJsonObject { put("enabled", it) })
            }
            if (policy.enabled) PercentPicker("Threshold", policy.thresholdPercent, thresholdOptions(policy.thresholdPercent), enabled) {
                editor.update(buildJsonObject { put("threshold_percent", it) })
            }
            Text(if (policy.source == "override") "Custom settings for this agent" else "Using defaults", style = MaterialTheme.typography.bodySmall)
            if (!policy.hasGauge) Text("No context reading; review requests and Hand off now can still ask this agent to hand off.", style = MaterialTheme.typography.bodySmall)
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                TextButton(enabled = enabled && policy.source == "override", onClick = { editor.update(buildJsonObject { put("use_default", true) }) }) { Text("Use default") }
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
        Text("Let a fresh agent take over when context fills up. You can customize this for each agent.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
        state.defaults?.let { defaults ->
            val enabled = !state.saving
            // Include plain Codex and any future providers returned by the server.
            val providers = handoffProviders(defaults.providers)
            providers.forEach { provider ->
                val label = when (provider) {
                    "claude" -> "Claude"
                    "codex-fork" -> "Codex Fork"
                    "codex" -> "Codex"
                    else -> provider
                }
                HandoffSwitch(label, defaults.providers[provider] ?: false, enabled) {
                    editor.update(buildJsonObject { put("providers", buildJsonObject { put(provider, it) }) })
                }
            }
            if (providers.any { defaults.providers[it] == true }) {
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
        }
        EditorStatus(state, editor::refresh)
    }
}

internal fun handoffProviders(providers: Map<String, Boolean>): List<String> =
    (listOf("claude", "codex-fork", "codex") + providers.keys).distinct().filterNot { it == "codex-app" }

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
