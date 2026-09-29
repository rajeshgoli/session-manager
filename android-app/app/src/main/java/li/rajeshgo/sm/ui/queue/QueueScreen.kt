package li.rajeshgo.sm.ui.queue

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.Duration
import java.time.OffsetDateTime
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.watch.MarkdownText
import li.rajeshgo.sm.ui.navigation.AppBottomNav
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.PanelMuted
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary

private const val QUEUE_REFRESH_MS = 5000L

/** True while the screen is at least resumed, so polling stops when hidden. */
@Composable
internal fun rememberResumed(): Boolean {
    val lifecycleOwner = LocalLifecycleOwner.current
    var resumed by remember {
        mutableStateOf(lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED))
    }
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, _ ->
            resumed = lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }
    return resumed
}

@Composable
fun QueueScreen(
    onNavigateToInbox: () -> Unit,
    onNavigateToWatch: () -> Unit,
    onNavigateToAnalytics: () -> Unit,
    onOpenUsage: () -> Unit,
    viewModel: QueueViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val resumed = rememberResumed()
    var sheetJob by remember { mutableStateOf<SessionJob?>(null) }
    var now by remember { mutableStateOf(OffsetDateTime.now()) }

    LaunchedEffect(Unit) { viewModel.refreshStats() }
    LaunchedEffect(resumed) {
        if (!resumed) return@LaunchedEffect
        while (isActive) {
            viewModel.refresh()
            now = OffsetDateTime.now()
            delay(QUEUE_REFRESH_MS)
        }
    }

    Box(
        modifier = Modifier
            .fillMaxSize().statusBarsPadding().navigationBarsPadding()
            .background(MaterialTheme.colorScheme.background),
    ) {
        val overview = state.overview
        if (state.loading && overview == null) {
            Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator(color = Cyan) }
        } else {
            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 112.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                item {
                    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.Bottom) {
                        Text("Queue", style = MaterialTheme.typography.headlineSmall, modifier = Modifier.weight(1f))
                        Text(
                            state.lastUpdated?.let { "updated ${shortDuration(Duration.between(it, now).seconds)} ago" } ?: "",
                            style = MaterialTheme.typography.bodySmall,
                            color = TextMuted,
                        )
                    }
                    state.refreshError?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall) }
                    if (state.signedOut) Text("Sign in to load the queue", color = Rose)
                }
                if (overview != null) {
                    item {
                        MeterPanel(overview.host, onClick = onOpenUsage)
                        Text(
                            slotsLine(overview.slots),
                            style = MaterialTheme.typography.bodySmall,
                            color = TextSecondary,
                            modifier = Modifier.padding(top = 6.dp, start = 2.dp),
                        )
                    }
                    if (overview.running.isEmpty() && overview.queued.isEmpty()) {
                        item { Text("Nothing running or waiting.", color = TextMuted, modifier = Modifier.padding(vertical = 16.dp)) }
                    }
                    if (overview.running.isNotEmpty()) {
                        item { SectionHeader("RUNNING") }
                        items(overview.running, key = { "r-${it.id}" }) { job ->
                            JobRow(
                                title = jobTitle(job),
                                agent = jobAgentLabel(job),
                                line = runningLine(job, now),
                                progress = runningProgress(job, now),
                                onClick = { sheetJob = job; viewModel.loadLog(job) },
                            )
                        }
                    }
                    if (overview.queued.isNotEmpty()) {
                        item { SectionHeader("QUEUED · IN START ORDER") }
                        items(overview.queued, key = { "q-${it.id}" }) { job ->
                            JobRow(
                                title = jobTitle(job),
                                agent = jobAgentLabel(job),
                                line = queuedLine(job, now),
                                reason = queuedReason(job),
                                prefix = job.position?.toString(),
                                onClick = { sheetJob = job; viewModel.loadLog(job) },
                            )
                        }
                    }
                    if (overview.ended.isNotEmpty()) {
                        item { SectionHeader("STOPPED BY THE QUEUE · LAST 24H") }
                        items(overview.ended, key = { "e-${it.id}" }) { job ->
                            JobRow(
                                title = jobTitle(job),
                                agent = jobAgentLabel(job),
                                line = endedLine(job, now),
                                reason = job.endedSummary,
                                prefix = endedIcon(job.endedReason),
                                prefixColor = if (job.endedReason == "gave_up") Amber else Rose,
                                onClick = { sheetJob = job; viewModel.loadLog(job) },
                            )
                        }
                    }
                    item {
                        SectionHeader("HELD BACK?")
                        HeldBackCard(
                            lines = heldBackLines(state.stats),
                            hours = state.statsHours,
                            onHours = { viewModel.refreshStats(it) },
                        )
                    }
                }
            }
        }

        Box(
            modifier = Modifier.align(Alignment.BottomCenter).padding(horizontal = 16.dp, vertical = 16.dp),
        ) {
            AppBottomNav(
                currentRoute = Routes.QUEUE,
                onInbox = onNavigateToInbox,
                onWatch = onNavigateToWatch,
                onQueue = {},
                onAnalytics = onNavigateToAnalytics,
            )
        }
    }

    sheetJob?.let { job ->
        // Show the freshest copy of the job while the sheet is open.
        val overview = state.overview
        val current = overview?.let { (it.running + it.queued + it.ended).firstOrNull { row -> row.id == job.id } } ?: job
        JobSheet(
            job = current,
            now = now,
            log = state.log?.takeIf { it.first == job.id }?.second,
            ask = state.asks[job.id],
            cancelError = state.cancelError,
            followMessage = state.followMessage?.takeIf { it.first == job.id }?.second,
            startNow = state.startNow?.takeIf { it.jobId == job.id },
            onCheckStartNow = { viewModel.checkStartNow(current) },
            onStartNow = { viewModel.startNow(current) },
            onDismissStartNow = viewModel::dismissStartNow,
            onFollow = { viewModel.follow(current) },
            onCancel = { note -> viewModel.cancel(current, note) { sheetJob = null; viewModel.clearSheetState() } },
            onAsk = { question -> viewModel.ask(current, question) },
            onOpenAgent = current.notifySessionId?.takeIf { it.isNotBlank() }?.let { sessionId ->
                {
                    sheetJob = null
                    viewModel.clearSheetState()
                    FollowOpenRequests.pending = FollowOpen(sessionId, null, jobAgentLabel(current))
                    onNavigateToWatch()
                }
            },
            onClose = { sheetJob = null; viewModel.clearSheetState() },
        )
    }
}

@Composable
private fun SectionHeader(text: String) {
    Column(Modifier.padding(top = 18.dp, bottom = 2.dp)) {
        Text(text, style = MaterialTheme.typography.labelSmall, color = TextMuted, fontWeight = FontWeight.Bold)
        HorizontalDivider(color = Border, modifier = Modifier.padding(top = 4.dp))
    }
}

internal fun meterColor(row: MeterRow): Color = when {
    row.fraction == null -> TextMuted
    row.label == "GPU" && meterBand(row.fraction) == Meter.LOW -> Cyan
    else -> when (meterBand(row.fraction)) {
        Meter.LOW -> Emerald
        Meter.MID -> Amber
        Meter.HIGH -> Rose
    }
}

@Composable
internal fun MeterPanel(host: li.rajeshgo.sm.data.model.HostStatus?, onClick: (() -> Unit)?) {
    val rows = meterRows(host)
    Surface(
        color = Panel,
        shape = RoundedCornerShape(10.dp),
        modifier = Modifier.fillMaxWidth().padding(top = 10.dp).let { if (onClick != null) it.clickable(onClick = onClick) else it },
    ) {
        Column(Modifier.padding(horizontal = 12.dp, vertical = 10.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            if (rows.isEmpty()) {
                Text("Utilization unavailable", color = TextMuted, style = MaterialTheme.typography.bodySmall)
            }
            rows.forEach { row ->
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(row.label, style = MaterialTheme.typography.labelSmall, color = TextSecondary, modifier = Modifier.width(34.dp))
                    Box(Modifier.weight(1f).height(7.dp).background(PanelMuted, RoundedCornerShape(99.dp))) {
                        Box(
                            Modifier.fillMaxWidth((row.fraction ?: 0.0).toFloat().coerceIn(0f, 1f)).height(7.dp)
                                .background(meterColor(row), RoundedCornerShape(99.dp)),
                        )
                    }
                    Text(row.value, style = MaterialTheme.typography.labelSmall, modifier = Modifier.padding(start = 8.dp))
                    row.warning?.let { Text(" $it", style = MaterialTheme.typography.labelSmall, color = Rose) }
                }
            }
            if (onClick != null && rows.isNotEmpty()) {
                Text("Mac usage over time ›", style = MaterialTheme.typography.labelSmall, color = Cyan)
            }
        }
    }
}

@Composable
private fun JobRow(
    title: String,
    agent: String,
    line: String,
    onClick: () -> Unit,
    reason: String? = null,
    progress: Float? = null,
    prefix: String? = null,
    prefixColor: Color = Cyan,
) {
    Column(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(vertical = 8.dp, horizontal = 2.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            prefix?.let {
                Text(it, color = prefixColor, fontWeight = FontWeight.Bold, modifier = Modifier.width(20.dp))
            }
            Text(title, fontWeight = FontWeight.SemiBold, maxLines = 1)
            Text(" · $agent", color = TextSecondary, maxLines = 1)
        }
        Text(line, style = MaterialTheme.typography.bodySmall, color = TextSecondary)
        reason?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
        progress?.let {
            LinearProgressIndicator(
                progress = { it },
                modifier = Modifier.fillMaxWidth().height(3.dp).padding(top = 2.dp),
                color = Cyan,
                trackColor = PanelMuted,
            )
        }
    }
    HorizontalDivider(color = Border.copy(alpha = 0.5f))
}

@Composable
private fun HeldBackCard(lines: List<String>, hours: Int, onHours: (Int) -> Unit) {
    Surface(color = Panel, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth().padding(top = 8.dp)) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                listOf(24 to "24h", 168 to "7d", 720 to "30d").forEach { (value, label) ->
                    Text(
                        label,
                        color = if (value == hours) Cyan else TextMuted,
                        fontWeight = if (value == hours) FontWeight.Bold else FontWeight.Normal,
                        modifier = Modifier.clickable { onHours(value) },
                    )
                }
            }
            if (lines.isEmpty()) {
                Text("No utilization recorded yet.", color = TextMuted)
            } else {
                lines.forEach { Text(it, style = MaterialTheme.typography.bodyMedium) }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun JobSheet(
    job: SessionJob,
    now: OffsetDateTime,
    log: String?,
    ask: AskState?,
    cancelError: String?,
    followMessage: String?,
    startNow: StartNowState?,
    onCheckStartNow: () -> Unit,
    onStartNow: () -> Unit,
    onDismissStartNow: () -> Unit,
    onFollow: () -> Unit,
    onCancel: (String?) -> Unit,
    onAsk: (String) -> Unit,
    onOpenAgent: (() -> Unit)?,
    onClose: () -> Unit,
) {
    var confirming by remember { mutableStateOf(false) }
    var question by remember(job.id) { mutableStateOf("") }
    val active = job.state == "running" || job.state == "pending"
    ModalBottomSheet(onDismissRequest = onClose, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(
            Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(jobTitle(job), style = MaterialTheme.typography.titleLarge)
            Text(listOfNotNull(job.type, job.state, jobAgentLabel(job)).joinToString(" · "), color = TextSecondary)
            jobCommand(job)?.let { Detail("Command", it, mono = true) }
            job.cwd?.let { Detail("Folder", it, mono = true) }
            Detail(
                "Times",
                listOfNotNull(
                    job.queuedAt?.let { "queued ${secondsBetween(it, now)?.let(::shortDuration) ?: "-"} ago" },
                    job.startedAt?.let { "started ${secondsBetween(it, now)?.let(::shortDuration) ?: "-"} ago" },
                    job.finishedAt?.let { "finished ${secondsBetween(it, now)?.let(::shortDuration) ?: "-"} ago" },
                ).joinToString(" · "),
            )
            jobLimits(job).takeIf { it.isNotBlank() }?.let { Detail("Limits", it) }
            if (job.state == "pending") job.holding?.detail?.let { Detail("Why it is waiting", it) }
            if (job.state == "running" && job.ownerForcedAt != null) {
                Detail("Started", "Early, by you. If the Mac runs low on memory, sm stops this run first and puts the job back in line.")
            }
            if (job.state == "pending") startNow?.let { StartNowCard(it, onStartNow, onDismissStartNow) }
            job.endedSummary?.let { Detail("What happened", it) }

            // Also on stopped jobs: "why did it stop?" is worth asking.
            if (onOpenAgent != null) {
                HorizontalDivider(color = Border)
                Text("Ask ${jobAgentLabel(job)}", style = MaterialTheme.typography.titleSmall)
                Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    ASK_AGENT_SUGGESTIONS.forEach { suggestion ->
                        AssistChip(onClick = { question = suggestion }, label = { Text(suggestion) })
                    }
                }
                OutlinedTextField(
                    value = question,
                    onValueChange = { question = it },
                    modifier = Modifier.fillMaxWidth(),
                    placeholder = { Text("Your question") },
                )
                val asking = ask != null && ask.status !in setOf("completed", "failed", "timed_out")
                Button(onClick = { onAsk(question) }, enabled = question.isNotBlank() && !asking) {
                    Text(if (asking) "Asking…" else "Send")
                }
                ask?.let {
                    Text("You asked: ${it.question}", style = MaterialTheme.typography.bodySmall, color = TextMuted)
                    when {
                        it.answer != null -> MarkdownText(it.answer)
                        it.error != null -> Text(it.error, color = Rose)
                        else -> Text("Waiting for ${jobAgentLabel(job)} (${it.status})…", color = TextMuted)
                    }
                }
            }

            HorizontalDivider(color = Border)
            Text("Log (last 40 lines)", style = MaterialTheme.typography.titleSmall)
            Text(
                log ?: "Loading…",
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodySmall,
                color = TextSecondary,
            )
            cancelError?.let { Text(it, color = Rose) }
            followMessage?.let { Text(it, color = TextSecondary) }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(bottom = 24.dp)) {
                if (job.state == "pending" && startNow == null) {
                    OutlinedButton(onClick = onCheckStartNow) { Text("Start now") }
                }
                if (active) {
                    OutlinedButton(onClick = { confirming = true }) { Text("Cancel", color = Rose) }
                    // The server follows only jobs that have not finished.
                    OutlinedButton(onClick = onFollow) { Text("Follow") }
                }
                onOpenAgent?.let { OutlinedButton(onClick = it) { Text("Open agent") } }
            }
        }
    }
    if (confirming) {
        CancelDialog(
            job = job,
            onConfirm = { note -> confirming = false; onCancel(note) },
            onDismiss = { confirming = false },
        )
    }
}

/** The warnings the owner weighs before overriding the queue (sm#1627). */
@Composable
private fun StartNowCard(state: StartNowState, onStart: () -> Unit, onDismiss: () -> Unit) {
    Surface(color = Amber.copy(alpha = 0.08f), shape = RoundedCornerShape(12.dp)) {
        Column(Modifier.fillMaxWidth().padding(14.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text("Start now?", style = MaterialTheme.typography.titleSmall, color = Amber)
            val check = state.check
            when {
                check == null && state.error == null -> Text("Checking what this skips…", color = TextMuted)
                check != null && check.warnings.isEmpty() -> Text("Nothing is holding it back right now.", color = TextSecondary)
                check != null -> check.warnings.forEach { Text("• $it", color = Amber, style = MaterialTheme.typography.bodyMedium) }
            }
            check?.let { Text(startNowMemoryLine(it), style = MaterialTheme.typography.bodySmall, color = TextSecondary) }
            Text(
                "If the Mac runs low on memory, sm stops this run first and puts the job back in line.",
                style = MaterialTheme.typography.bodySmall,
                color = TextMuted,
            )
            state.error?.let { Text(it, color = Rose, style = MaterialTheme.typography.bodySmall) }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = onStart, enabled = check != null && !state.starting) {
                    Text(if (state.starting) "Starting…" else "Start anyway")
                }
                OutlinedButton(onClick = onDismiss) { Text("Keep waiting") }
            }
        }
    }
}

@Composable
private fun Detail(label: String, value: String, mono: Boolean = false) {
    Column {
        Text(label, style = MaterialTheme.typography.labelMedium, color = TextMuted)
        Text(value, fontFamily = if (mono) FontFamily.Monospace else null, style = MaterialTheme.typography.bodyMedium)
    }
}

/** One dialog for waiting and running jobs, with an optional note to the agent. */
@Composable
private fun CancelDialog(job: SessionJob, onConfirm: (String?) -> Unit, onDismiss: () -> Unit) {
    var note by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Cancel ${jobTitle(job)}?") },
        text = {
            OutlinedTextField(
                value = note,
                onValueChange = { note = it.take(1000) },
                label = { Text("Note to ${jobAgentLabel(job)} (optional)") },
                modifier = Modifier.fillMaxWidth(),
            )
        },
        confirmButton = {
            TextButton(onClick = { onConfirm(note.trim().ifEmpty { null }) }) { Text("Cancel job", color = Rose) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(if (job.state == "running") "Keep running" else "Keep waiting") }
        },
    )
}
