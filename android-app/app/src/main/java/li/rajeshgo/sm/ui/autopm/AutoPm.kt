package li.rajeshgo.sm.ui.autopm

import android.app.Application
import android.content.Context
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ArrowBack
import androidx.compose.material.icons.automirrored.rounded.KeyboardArrowRight
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import li.rajeshgo.sm.data.model.AutopmBudget
import li.rajeshgo.sm.data.model.AutopmChange
import li.rajeshgo.sm.data.model.AutopmView
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.navigation.ScreenName
import li.rajeshgo.sm.ui.theme.*
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale

/** How often an open app re-reads the switches, and how often Settings › Auto PM refreshes (spec 2170 B.1). */
const val AUTOPM_REFRESH_MS = 30_000L

private val STATE_TEXT = mapOf("live" to "Live", "measuring" to "Measuring", "rolling_back" to "Rolling back", "kept" to "Kept", "reverted" to "Reverted")

/** "Oct 21" in the phone's zone. */
fun autopmDay(ms: Long?, zone: ZoneId = ZoneId.systemDefault()): String =
    ms?.let { DateTimeFormatter.ofPattern("MMM d", Locale.US).format(Instant.ofEpochMilli(it).atZone(zone)) }.orEmpty()

/** "2 live · 1 measuring": live changes counted by state, in a fixed order. */
fun liveSummary(changes: List<AutopmChange>): String {
    val counts = changes.groupingBy { it.state }.eachCount()
    val parts = listOf("live", "measuring", "rolling_back", "kept")
        .filter { (counts[it] ?: 0) > 0 }
        .map { "${counts[it]} ${STATE_TEXT.getValue(it).lowercase()}" }
    return if (parts.isEmpty()) "None live" else parts.joinToString(" · ")
}

/** The dates line under a change: when it went live and when sm measures it. */
fun changeMeta(change: AutopmChange, now: Long = System.currentTimeMillis(), zone: ZoneId = ZoneId.systemDefault()): String {
    val note = change.decisionNote?.takeIf { it.isNotBlank() }?.let { " · $it" }.orEmpty()
    return when (change.state) {
        "reverted" -> "Reverted ${autopmDay(change.endedAtMs, zone)}$note"
        "rolling_back" -> "Rolling back$note"
        "kept" -> "Kept ${autopmDay(change.keptAtMs, zone)} · live since ${autopmDay(change.liveAtMs, zone)}"
        else -> {
            val measured = change.measureAtMs?.takeIf { it <= now }?.let { "measuring now" } ?: "measured ${autopmDay(change.measureAtMs, zone)}"
            "Live since ${autopmDay(change.liveAtMs, zone)} · $measured"
        }
    }
}

private fun points(value: Double?): String = value?.let { if (it % 1.0 == 0.0) it.toLong().toString() else String.format(Locale.US, "%.1f", it) } ?: "—"

/** "7 of 20" points spent of the allowance. */
fun budgetText(budget: AutopmBudget?): String = when {
    budget == null -> "No meter data"
    budget.skipped -> "Skipped this week"
    else -> "${points(budget.spent)} of ${points(budget.allowance)}"
}

/** Skip this week writes now; undoing it writes null (spec 2170 C.1). */
fun skipValue(skipped: Boolean, now: Instant = Instant.now()): String? = if (skipped) null else now.toString()

/**
 * The auto PM switches this app has read (spec 2170 B.1). A missing key is
 * off. [refresh] runs when a response names a new switch version, and every
 * 30 s while the app is in front.
 */
object AutopmSwitches {
    private val values = MutableStateFlow<Map<String, Boolean>>(emptyMap())
    val switches: StateFlow<Map<String, Boolean>> = values.asStateFlow()

    fun isOn(key: String): Boolean = values.value[key] == true

    suspend fun refresh(context: Context) {
        val settings = SettingsRepository(context.applicationContext)
        val url = settings.serverUrl.first()
        val token = settings.accessToken.first()
        if (url.isBlank() || token.isBlank()) return
        runCatching { SessionManagerRepository(settings).fetchSwitches(url, token) }.onSuccess { values.value = it }
    }
}

data class AutoPmUiState(
    val view: AutopmView? = null,
    val error: String? = null,
    val busy: Boolean = false,
    val message: String? = null,
)

class AutoPmViewModel(application: Application) : AndroidViewModel(application) {
    private val settings = SettingsRepository(application)
    private val repository = SessionManagerRepository(settings)
    private val state = MutableStateFlow(AutoPmUiState())
    val uiState: StateFlow<AutoPmUiState> = state.asStateFlow()

    fun load(background: Boolean = false) {
        viewModelScope.launch {
            runCatching { repository.fetchAutopm(settings.serverUrl.first(), settings.accessToken.first(), background) }
                .onSuccess { view -> state.update { it.copy(view = view, error = null) } }
                .onFailure { error -> state.update { it.copy(error = error.message ?: "Couldn't load Auto PM") } }
        }
    }

    private fun write(block: suspend (String, String) -> Result<*>) {
        viewModelScope.launch {
            state.update { it.copy(busy = true, message = null) }
            val result = block(settings.serverUrl.first(), settings.accessToken.first())
            state.update { it.copy(busy = false, message = result.exceptionOrNull()?.let { e -> e.message ?: "Couldn't save" }) }
            load()
            AutopmSwitches.refresh(getApplication())
        }
    }

    private fun patch(build: kotlinx.serialization.json.JsonObjectBuilder.() -> Unit) = write { url, token ->
        repository.setOwnerSettings(url, token, buildJsonObject { put("autopm", buildJsonObject(build)) })
    }

    fun setEnabled(on: Boolean) = patch { put("enabled", on) }
    fun resume() = patch { put("paused_at", JsonNull) }
    fun toggleSkip(skipped: Boolean) = patch {
        val value = skipValue(skipped)
        if (value == null) put("skip_window_starting", JsonNull) else put("skip_window_starting", value)
    }

    fun vote(change: AutopmChange, up: Boolean) = write { url, token ->
        repository.sendAutopmFeedback(url, token, change.id, up).also { result ->
            if (!up && result.isSuccess) state.update { it.copy(message = "${change.title} is off. Only you can turn it back on.") }
        }
    }

    fun turnBackOn(change: AutopmChange) {
        val key = change.switchKey ?: return
        write { url, token -> repository.setAutopmSwitch(url, token, key, true) }
    }
}

/** Settings › Auto PM, and its What changed list (spec 2170 F.3 and the wireframes under "What you will see"). */
@Composable
fun AutoPmScreen(
    onBack: () -> Unit,
    onOpenInbox: () -> Unit,
    viewModel: AutoPmViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    var changesOpen by rememberSaveable { mutableStateOf(false) }
    LaunchedEffect(Unit) {
        viewModel.load()
        while (isActive) {
            delay(AUTOPM_REFRESH_MS)
            viewModel.load(background = true)
        }
    }
    BackHandler(enabled = changesOpen) { changesOpen = false }
    val back = { if (changesOpen) changesOpen = false else onBack() }
    Column(
        Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding()
            .verticalScroll(rememberScrollState()).padding(horizontal = 20.dp).padding(bottom = 24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = back) { Icon(Icons.AutoMirrored.Rounded.ArrowBack, "Back") }
            Column {
                Text(if (changesOpen) "Auto PM" else "Settings", style = MaterialTheme.typography.labelMedium, color = TextMuted)
                Text(if (changesOpen) "What changed" else "Auto PM", style = MaterialTheme.typography.headlineSmall)
            }
        }
        val view = state.view
        when {
            view == null && state.error != null -> {
                Text(state.error!!, color = Rose)
                TextButton(onClick = { viewModel.load() }) { Text("Retry") }
            }
            view == null -> LinearProgressIndicator(Modifier.fillMaxWidth())
            changesOpen -> {
                ScreenName("settings/autopm/changes")
                WhatChanged(view, state.busy, viewModel::vote, viewModel::turnBackOn)
            }
            else -> AutoPmSettings(view, state.busy, viewModel, onOpenInbox) { changesOpen = true }
        }
        state.message?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = TextSecondary) }
    }
}

@Composable
private fun Group(title: String? = null, content: @Composable ColumnScope.() -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        title?.let { Text(it.uppercase(), style = MaterialTheme.typography.labelMedium, color = TextMuted, modifier = Modifier.padding(start = 4.dp)) }
        Surface(shape = RoundedCornerShape(18.dp), color = PanelElevated) {
            Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp), content = content)
        }
    }
}

@Composable
private fun SwitchRow(title: String, sub: String?, checked: Boolean, enabled: Boolean, onChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            sub?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = TextMuted) }
        }
        Switch(checked = checked, enabled = enabled, onCheckedChange = onChange)
    }
}

@Composable
private fun AutoPmSettings(view: AutopmView, busy: Boolean, viewModel: AutoPmViewModel, onOpenInbox: () -> Unit, onOpenChanges: () -> Unit) {
    var confirmOff by remember { mutableStateOf(false) }
    val resets = view.budget?.values?.firstNotNullOfOrNull { it?.resetsAt }
        ?.let { runCatching { Instant.parse(it).toEpochMilli() }.getOrNull() }
    Group {
        SwitchRow("Auto PM on", null, view.enabled, !busy) { on -> if (on) viewModel.setEnabled(true) else confirmOff = true }
        if (view.pausedAt != null) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text("Paused: four cycles, nothing kept", Modifier.weight(1f), color = Amber)
                TextButton(onClick = viewModel::resume, enabled = !busy) { Text("Resume") }
            }
        }
        HorizontalDivider(color = Border)
        SwitchRow(
            "Skip this week",
            if (view.skipped) "Allowance 0 until the ${autopmDay(resets)} reset" else "Sets this week's allowance to zero",
            view.skipped,
            !busy,
        ) { viewModel.toggleSkip(view.skipped) }
    }
    Group("This week's budget (points)") {
        listOf("claude" to "Claude", "codex" to "Codex").forEach { (key, name) ->
            Row(Modifier.fillMaxWidth()) {
                Text(name, Modifier.weight(1f))
                Text(budgetText(view.budget?.get(key)), color = TextSecondary)
            }
        }
    }
    Group {
        Row(Modifier.fillMaxWidth().clickable(onClick = onOpenChanges), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("Live changes", style = MaterialTheme.typography.bodyLarge)
                Text(liveSummary(view.changes), style = MaterialTheme.typography.bodySmall, color = TextMuted)
            }
            Icon(Icons.AutoMirrored.Rounded.KeyboardArrowRight, "Open What changed", tint = TextMuted)
        }
        view.readout?.let { readout ->
            HorizontalDivider(color = Border)
            Row(
                Modifier.fillMaxWidth().clickable {
                    FollowOpenRequests.pending = FollowOpen(null, readout.readerPath, readout.title)
                },
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Latest readout: ${readout.week ?: readout.title}", Modifier.weight(1f))
                Icon(Icons.AutoMirrored.Rounded.KeyboardArrowRight, "Open readout", tint = TextMuted)
            }
        }
    }
    if (view.waiting.isNotEmpty()) {
        Surface(
            shape = RoundedCornerShape(18.dp),
            color = PanelElevated,
            border = BorderStroke(1.dp, Amber),
            modifier = Modifier.fillMaxWidth().clickable(onClick = onOpenInbox),
        ) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                val n = view.waiting.size
                Text("Waiting on you: $n decision${if (n == 1) "" else "s"}", fontWeight = FontWeight.SemiBold)
                Text("Shadow-tested · tap to answer in the Inbox", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            }
        }
    }
    Text("Turning Auto PM off switches every change off and holds it.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
    if (confirmOff) AlertDialog(
        onDismissRequest = { confirmOff = false },
        title = { Text("Turn Auto PM off?") },
        text = { Text("Every live change switches off and stays off until you turn it back on.") },
        confirmButton = { TextButton(onClick = { confirmOff = false; viewModel.setEnabled(false) }) { Text("Turn off", color = Rose) } },
        dismissButton = { TextButton(onClick = { confirmOff = false }) { Text("Cancel") } },
    )
}

@Composable
private fun Thumbs(change: AutopmChange, busy: Boolean, onVote: (AutopmChange, Boolean) -> Unit) {
    val can = !busy && change.state != "reverted"
    Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
        listOf(true to "👍", false to "👎").forEach { (up, glyph) ->
            val chosen = change.vote == (if (up) "up" else "down")
            FilledTonalButton(
                onClick = { onVote(change, up) },
                enabled = can,
                contentPadding = PaddingValues(horizontal = 10.dp),
                colors = if (chosen) ButtonDefaults.filledTonalButtonColors(containerColor = CyanDeep) else ButtonDefaults.filledTonalButtonColors(containerColor = PanelMuted),
            ) { Text(glyph) }
        }
    }
}

/** The What changed list: live changes with optional thumbs, then recently ended ones. */
@Composable
private fun WhatChanged(
    view: AutopmView,
    busy: Boolean,
    onVote: (AutopmChange, Boolean) -> Unit,
    onTurnBackOn: (AutopmChange) -> Unit,
) {
    Text(liveSummary(view.changes), color = TextSecondary)
    if (view.changes.isEmpty()) Text("Nothing the auto PM shipped is live.", color = TextMuted)
    view.changes.forEach { change ->
        Group {
            Text(change.title, style = MaterialTheme.typography.titleMedium)
            change.line?.takeIf { it.isNotBlank() }?.let { Text(it, style = MaterialTheme.typography.bodyMedium, color = TextSecondary) }
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(changeMeta(change), Modifier.weight(1f), style = MaterialTheme.typography.bodySmall, color = TextMuted)
                Thumbs(change, busy, onVote)
            }
        }
    }
    Text("Thumbs are optional. 👎 switches that change off at once; only you can turn it back on.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
    if (view.ended.isNotEmpty()) Group("Recently ended") {
        view.ended.forEachIndexed { index, change ->
            if (index > 0) HorizontalDivider(color = Border)
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(change.title, color = TextSecondary)
                    Text(changeMeta(change), style = MaterialTheme.typography.bodySmall, color = TextMuted)
                }
                if (change.switchKey != null) TextButton(onClick = { onTurnBackOn(change) }, enabled = !busy) { Text("Turn back on") }
            }
        }
    }
}

/**
 * The dismissible "✦ New" chip a build ticket puts on each screen its switch
 * changes (spec 2170 F.3). It shows only while the switch is on and the
 * change is live; ✕ hides it on this phone.
 */
@Composable
fun AutoPmChip(switchKey: String, modifier: Modifier = Modifier) {
    val context = LocalContext.current.applicationContext
    val switches by AutopmSwitches.switches.collectAsState()
    val preferences = remember(context) { context.getSharedPreferences("autopm_chips", Context.MODE_PRIVATE) }
    var dismissed by remember(switchKey) { mutableStateOf(preferences.getBoolean(switchKey, false)) }
    if (switches[switchKey] != true || dismissed) return
    val viewModel: AutoPmViewModel = viewModel(key = "autopm-chip")
    val state by viewModel.uiState.collectAsState()
    LaunchedEffect(switchKey) { if (state.view == null) viewModel.load() }
    val change = state.view?.changes?.firstOrNull { it.switchKey == switchKey } ?: return
    Surface(shape = RoundedCornerShape(12.dp), color = PanelElevated, border = BorderStroke(1.dp, Cyan), modifier = modifier.fillMaxWidth()) {
        Row(Modifier.padding(start = 12.dp), verticalAlignment = Alignment.CenterVertically) {
            Text("✦ New: ${change.title}", Modifier.weight(1f), maxLines = 1, overflow = TextOverflow.Ellipsis)
            Thumbs(change, state.busy, viewModel::vote)
            IconButton(onClick = {
                preferences.edit().putBoolean(switchKey, true).apply()
                dismissed = true
            }) { Icon(Icons.Rounded.Close, "Dismiss", tint = TextMuted) }
        }
    }
}
