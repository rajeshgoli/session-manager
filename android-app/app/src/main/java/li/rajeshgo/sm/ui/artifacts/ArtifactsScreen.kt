package li.rajeshgo.sm.ui.artifacts

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.serialization.json.JsonObject
import li.rajeshgo.sm.ui.navigation.*
import li.rajeshgo.sm.ui.queue.SectionHeader
import li.rajeshgo.sm.ui.theme.*
import li.rajeshgo.sm.ui.watch.MarkdownText

@Composable
fun ArtifactsScreen(onBack: () -> Unit, menu: AppMenuActions, viewModel: ArtifactsViewModel, onNavigate: (String) -> Unit) {
    val state by viewModel.state.collectAsState()
    ArtifactsContent(state, onBack, menu, viewModel::refresh, viewModel::act, onNavigate, viewModel::investigate)
    // Investigating starts an agent like any other: the New agent sheet, on the brief (sm#2266).
    state.investigation?.let { start ->
        li.rajeshgo.sm.ui.watch.CreateSessionSheet(
            source = null,
            sessions = emptyList(),
            loadModels = viewModel::sessionModels,
            loadAgentTypes = viewModel::sessionAgentTypes,
            busy = state.busy,
            error = state.createError,
            onDismiss = viewModel::closeInvestigation,
            prefill = start.request,
            onCreate = viewModel::createSession,
        )
    }
}

@Composable
internal fun ArtifactsContent(state: ArtifactsState, onBack: () -> Unit, menu: AppMenuActions,
    refresh: () -> Unit, act: (JsonObject, String) -> Unit, onNavigate: (String) -> Unit, investigate: (JsonObject) -> Unit = {}) {
    val data = state.data
    val rows = data?.rows("artifacts").orEmpty()
    val inventory = artifactInventory(rows)
    val decisions = inventory.filter(::artifactNeedsDecision)
    val followup = inventory.filter { artifactNeedsAgent(it) && !artifactNeedsDecision(it) }
    var browse by remember { mutableStateOf(false) }
    var query by remember { mutableStateOf("") }
    var limit by remember(query) { mutableIntStateOf(25) }
    var selected by remember { mutableStateOf<JsonObject?>(null) }
    var confirmation by remember { mutableStateOf<Pair<JsonObject, String>?>(null) }
    LaunchedEffect(data, ArtifactsNavigation.name) {
        ArtifactsNavigation.name?.let { name ->
            rows.firstOrNull { it.text("name") == name }?.let { selected = it; ArtifactsNavigation.name = null }
        }
    }
    Box(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding().background(MaterialTheme.colorScheme.background)) {
        LazyColumn(contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 112.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp)) {
            item {
                AppTopBar(title = if (browse) "All artifacts" else "Artifacts", subtitle = "Storage & saved work",
                    menu = menu, current = Routes.ARTIFACTS, onBack = { if (browse) browse = false else onBack() },
                    onRefresh = refresh, modifier = Modifier.padding(bottom = 8.dp))
                state.error?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall) }
            }
            if (data == null) {
                item { Text("Loading storage…", color = TextMuted, modifier = Modifier.padding(16.dp)) }
            } else if (browse) {
                item {
                    OutlinedTextField(value = query, onValueChange = { query = it }, placeholder = { Text("Search saved work") },
                        singleLine = true, modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp))
                    Text("Largest first · unmeasured folders last", style = MaterialTheme.typography.bodySmall, color = TextMuted)
                }
                val filtered = artifactInventory(rows, query)
                items(filtered.take(limit), key = { it.text("root") + "/" + it.text("name") }) { a -> ArtifactRow(a) { selected = a } }
                item {
                    if (filtered.isEmpty()) Text("No matching folders", color = TextMuted, modifier = Modifier.padding(vertical = 16.dp))
                    if (filtered.size > limit) TextButton(onClick = { limit += 25 }) { Text("Show 25 more · ${filtered.size - limit} remaining") }
                }
            } else {
                item { StoragePanel(data.rows("filesystems")) }
                item {
                    val backup = data["backup"] as? JsonObject
                    Text(backupSummary(backup), color = if (backup?.flag("enabled") == true) TextSecondary else Amber,
                        style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(start = 2.dp, top = 8.dp))
                }
                if (decisions.isNotEmpty()) {
                    item { SectionHeader("CLEANUP TO REVIEW") }
                    items(decisions.take(5), key = { "review:" + it.text("root") + it.text("name") }) { a -> ArtifactRow(a) { selected = a } }
                    if (decisions.size > 5) item { TextButton(onClick = { browse = true }) { Text("Browse all ${decisions.size} proposals") } }
                }
                item {
                    SectionHeader("TAKING UP SPACE")
                    Text("Largest folders over 1 GB", color = TextMuted, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(vertical = 6.dp))
                }
                val largest = largestArtifacts(rows)
                items(largest, key = { it.text("root") + "/" + it.text("name") }) { a -> ArtifactRow(a) { selected = a } }
                item {
                    if (largest.isEmpty()) Text("No measured folders over 1 GB", color = TextSecondary, modifier = Modifier.padding(vertical = 12.dp))
                    TextButton(onClick = { browse = true }) { Text("Browse all ${inventory.size} folders  ›") }
                }
                item {
                    SectionHeader("MANAGED IN THE BACKGROUND")
                    if (followup.isNotEmpty()) Text("${followup.size} storage issues need agent follow-up.", color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                    followup.take(5).forEach { a -> ArtifactRow(a) { selected = a } }
                    val compressed = inventory.filter { it.text("state") == "frozen" }
                    val unmeasured = inventory.count { measuredArtifactBytes(it) == null }
                    val missing = inventory.count { it.text("state") == "unrecorded" }
                    Column(Modifier.padding(vertical = 10.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        if (compressed.isNotEmpty()) Text("${compressed.size} compressed folders · ${storageSize(compressed.sumOf(::artifactSize))}", color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                        if (missing > 0) Text("$missing folders need an agent's record. This is bookkeeping, not a decision for you.", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                        if (unmeasured > 0) Text("$unmeasured folder sizes unavailable. The list above is incomplete until measured.", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                        Text("Keep work you still need. Agents check its history and dependencies before proposing cleanup.", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                    }
                }
            }
        }
        AppBottomNav(currentRoute = Routes.ARTIFACTS, onInbox = { onNavigate(Routes.INBOX) }, onWatch = { onNavigate(Routes.WATCH) },
            onBoard = { onNavigate(Routes.BOARD) }, onQueue = { onNavigate(Routes.QUEUE) }, modifier = Modifier.align(Alignment.BottomCenter).padding(16.dp))
    }
    selected?.let { previous ->
        val a = rows.firstOrNull { it.text("root") == previous.text("root") && it.text("name") == previous.text("name") } ?: previous
        ArtifactSheet(a, data?.flag("can_act") == true, state.busy, data?.number("keep_live_days") ?: 30,
            onClose = { selected = null }, onAction = { action -> if (action == "investigate") { selected = null; investigate(a) } else confirmation = a to action })
    }
    confirmation?.let { (a, action) ->
        AlertDialog(onDismissRequest = { confirmation = null }, title = { Text("${actionLabel(action)}?") },
            text = { Text("${a.text("name")} · ${storageSize(measuredArtifactBytes(a))}\n" +
                when (action) {
                    "delete" -> "Deletes this saved work. " + (if (!a.flag("rebuildable")) "No complete rebuild recipe is recorded. " else "A rebuild recipe is recorded. ") +
                        a.rows("dependents").joinToString(prefix = if (a.rows("dependents").isEmpty()) "" else "Used by: ") { it.text("name") }
                    "keep_live" -> "Keeps this work available for ${data?.number("keep_live_days")} days."
                    else -> "Restores the compressed folder so it can be used again."
                }) },
            confirmButton = { TextButton(onClick = { confirmation = null; act(a, action) }) { Text(actionLabel(action), color = if (action == "delete") Rose else Cyan) } },
            dismissButton = { TextButton(onClick = { confirmation = null }) { Text("Cancel") } })
    }
}

@Composable
private fun StoragePanel(disks: List<JsonObject>) {
    Surface(color = Panel, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth().padding(top = 10.dp)) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(20.dp)) {
            disks.forEach { f ->
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                        Text(diskName(f), style = MaterialTheme.typography.titleSmall)
                        Text(if (f.flag("mounted")) "${storageSize(f.number("free"))} free" else "Unavailable", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                    }
                    if (f.flag("mounted") && f.number("bytes") > 0) {
                        val used = (f.number("bytes") - f.number("free")).coerceAtLeast(0)
                        val low = f.number("free") < f.number("floor")
                        Text("${storageSize(used)} of ${storageSize(f.number("bytes"))} used", style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.SemiBold)
                        LinearProgressIndicator(progress = { (used.toFloat() / f.number("bytes")).coerceIn(0f, 1f) },
                            modifier = Modifier.fillMaxWidth().height(6.dp), color = if (low) Amber else Emerald, trackColor = PanelMuted)
                        if (low) Text("Space is running low", color = Amber, style = MaterialTheme.typography.bodySmall)
                    }
                }
            }
            if (disks.isEmpty()) Text("Storage information unavailable", color = TextMuted)
        }
    }
}

@Composable
private fun ArtifactRow(a: JsonObject, onClick: () -> Unit) {
    Column(Modifier.fillMaxWidth().clickable(onClick = onClick).padding(vertical = 12.dp, horizontal = 2.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Row(verticalAlignment = Alignment.Top, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Text(a.text("note").ifBlank { a.text("name").replace('_', ' ') }, fontWeight = FontWeight.SemiBold,
                maxLines = 2, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
            Text(storageSize(measuredArtifactBytes(a)), color = TextSecondary, style = MaterialTheme.typography.bodyMedium)
        }
        Text(artifactSummary(a), color = if (artifactNeedsDecision(a)) Amber else TextMuted, style = MaterialTheme.typography.bodySmall)
    }
    HorizontalDivider(color = Border.copy(alpha = 0.5f))
}

private fun actionLabel(action: String) = when (action) {
    "delete" -> "Delete folder"
    "keep_live" -> "Keep available"
    "thaw" -> "Restore folder"
    else -> "Ask an agent"
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ArtifactSheet(a: JsonObject, canAct: Boolean, busy: Boolean, days: Long, onClose: () -> Unit, onAction: (String) -> Unit) {
    var technical by remember { mutableStateOf(false) }
    ModalBottomSheet(onDismissRequest = onClose, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp, vertical = 8.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text(a.text("name"), style = MaterialTheme.typography.titleLarge)
            Text("${storageSize(measuredArtifactBytes(a))} · ${artifactSummary(a)}", color = TextSecondary)
            a.text("description").ifBlank { a.text("note") }.takeIf { it.isNotBlank() }?.let { MarkdownText(it, richLinks = true) }
            if (a.text("state") == "unrecorded") Text("An agent can recover the context from git, saved files and job history. You do not need to reconstruct it.", color = TextSecondary)
            a.rows("consumers").takeIf { it.isNotEmpty() }?.let { Text("Used by " + it.joinToString { c -> c.text("ref") }, color = TextSecondary) }
            Text(if (!a.flag("backup_due") && a.text("backed_up_at").isNotBlank()) "Backup confirmed" else "No current backup confirmed", color = TextMuted, style = MaterialTheme.typography.bodySmall)
            artifactReport(a)?.let { MarkdownText(it, richLinks = true) }
            if (canAct) {
                Button(onClick = { onAction("investigate") }, enabled = !busy && a.text("investigating").isBlank()) { Text(if (a.text("investigating").isBlank()) "Ask an agent to investigate" else "Agent investigating") }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (a.text("state") == "frozen") OutlinedButton(onClick = { onAction("thaw") }, enabled = !busy) { Text("Restore") }
                    if (a.text("state") in listOf("live", "frozen")) OutlinedButton(onClick = { onAction("keep_live") }, enabled = !busy) { Text("Keep $days days") }
                }
                if (a.text("state") != "deleted") TextButton(onClick = { onAction("delete") }, enabled = !busy) { Text("Delete folder…", color = Rose) }
            }
            TextButton(onClick = { technical = !technical }) { Text(if (technical) "Hide technical details" else "Technical details") }
            if (technical) {
                Text("${a.text("root")}/${a.text("name")}", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                a.text("size_error").takeIf { it.isNotBlank() }?.let { Text("Measurement: $it", color = TextMuted) }
                Text("Last used: ${a.text("last_used_at").ifBlank { "Not recorded" }}", color = TextMuted)
                a.rows("jobs").forEach { Text(it.toString(), style = MaterialTheme.typography.bodySmall) }
                a.rows("events").take(10).forEach { Text("${it.text("at")} · ${it.text("kind")}", style = MaterialTheme.typography.bodySmall, color = TextMuted) }
            }
            Spacer(Modifier.height(24.dp))
        }
    }
}
