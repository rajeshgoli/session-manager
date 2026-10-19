package li.rajeshgo.sm.ui.artifacts

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.AcUnit
import androidx.compose.material.icons.rounded.DriveFileMove
import androidx.compose.material.icons.rounded.WbSunny
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.sp
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
import kotlinx.serialization.json.*
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
                item { BackupAlert(data["backup"] as? JsonObject) }
                val ops = state.activity?.rows("operations").orEmpty()
                if (ops.isNotEmpty()) {
                    val inFlight = ops.count { it.text("state") in listOf("running", "waiting") }
                    item { SectionHeader(if (inFlight == 0) "JUST FINISHED" else if (inFlight == 1) "IN PROGRESS · ONE OPERATION" else "IN PROGRESS · $inFlight OPERATIONS, ONE AT A TIME") }
                    items(ops, key = { "op:" + it.number("id") }) { op -> OperationCard(op) }
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
                state.activity?.takeIf { (it["savings"] as? JsonObject)?.number("folders") ?: 0 > 0 || it.rows("history").isNotEmpty() }?.let { activity ->
                    item { SectionHeader("FREEZE & THAW"); FreezeCard(activity) }
                }
                item {
                    SectionHeader("MANAGED IN THE BACKGROUND")
                    if (followup.isNotEmpty()) Text("${followup.size} storage issues need agent follow-up.", color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                    followup.take(5).forEach { a -> ArtifactRow(a) { selected = a } }
                    val compressed = inventory.filter { it.text("state") == "frozen" }
                    val unmeasured = inventory.count { measuredArtifactBytes(it) == null }
                    val missing = inventory.count { it.text("state") == "unrecorded" }
                    Column(Modifier.padding(vertical = 10.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        if (compressed.isNotEmpty() && state.activity == null) Text("${compressed.size} frozen folders · ${storageSize(compressed.sumOf(::artifactSize))}", color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                        if (missing > 0) Text("$missing folders need an agent's record. This is bookkeeping, not a decision for you.", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                        if (unmeasured > 0) Text("$unmeasured folder sizes unavailable. The list above is incomplete until measured.", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                        BackupPanel(data["backup"] as? JsonObject)
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
        ArtifactSheet(a, state.activity, data?.flag("can_act") == true, state.busy, data?.number("keep_live_days") ?: 30,
            onClose = { selected = null }, onAction = { action -> if (action == "investigate") { selected = null; investigate(a) } else confirmation = a to action })
    }
    confirmation?.let { (a, action) ->
        AlertDialog(onDismissRequest = { confirmation = null }, title = { Text("${actionLabel(action)}?") },
            text = { Text("${a.text("name")} · ${storageSize(measuredArtifactBytes(a))}\n" +
                when (action) {
                    "delete" -> "Deletes this saved work. " + (if (!a.flag("rebuildable")) "No complete rebuild recipe is recorded. " else "A rebuild recipe is recorded. ") +
                        a.rows("dependents").joinToString(prefix = if (a.rows("dependents").isEmpty()) "" else "Used by: ") { it.text("name") }
                    "keep_live" -> "Keeps this work available for ${data?.number("keep_live_days")} days."
                    else -> "Thaws the frozen folder so it can be used again" + (thawTime(a)?.let { " ($it)." } ?: ".")
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
                        Column {
                            Text(diskName(f), style = MaterialTheme.typography.titleSmall)
                            Text(if (f.text("role") == "cold") "Cold disk" else "Artifact root", color = TextMuted, fontSize = 11.sp)
                        }
                        Text(if (f.flag("mounted")) "${storageSize(f.number("free"))} free" else "Unavailable", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                    }
                    if (f.flag("mounted") && f.number("bytes") > 0) {
                        val used = (f.number("bytes") - f.number("free")).coerceAtLeast(0)
                        val low = f.number("free") < f.number("floor")
                        Text("${storageSize(used)} of ${storageSize(f.number("bytes"))} used", style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.SemiBold)
                        DiskBar(used, f.number("bytes"), f.number("floor"), low)
                        Text(if (low) "Below its floor: sm keeps ${storageSize(f.number("floor"))} free"
                            else "Floor ${storageSize(f.number("floor"))} · ${storageSize((f.number("free") - f.number("floor")).coerceAtLeast(0))} above it",
                            color = if (low) Amber else TextMuted, style = MaterialTheme.typography.bodySmall)
                    }
                }
            }
            if (disks.isEmpty()) Text("Storage information unavailable", color = TextMuted)
        }
    }
}

/** Used space, and the floor sm keeps free as a hatched reserve at the end (sm#2325). */
@Composable
private fun DiskBar(used: Long, total: Long, floor: Long, low: Boolean) {
    val stripe = TextMuted.copy(alpha = 0.55f)
    androidx.compose.foundation.Canvas(Modifier.fillMaxWidth().height(8.dp).clip(RoundedCornerShape(9.dp))) {
        drawRect(PanelMuted)
        val usedWidth = size.width * (used.toFloat() / total).coerceIn(0f, 1f)
        drawRoundRect(if (low) Amber else Emerald, size = size.copy(width = usedWidth), cornerRadius = androidx.compose.ui.geometry.CornerRadius(size.height / 2))
        if (floor in 1 until total) {
            val start = size.width * (total - floor).toFloat() / total
            clipRect(left = start) {
                var x = start - size.height
                while (x < size.width) { drawLine(stripe, androidx.compose.ui.geometry.Offset(x, size.height), androidx.compose.ui.geometry.Offset(x + size.height, 0f), 1.5f); x += 5.dp.toPx() }
            }
            drawRect(TextSecondary, topLeft = androidx.compose.ui.geometry.Offset(start, 0f), size = androidx.compose.ui.geometry.Size(2.dp.toPx(), size.height))
        }
    }
}

/** Beside storage, one line only when backup needs the owner (#2291). */
@Composable
private fun BackupAlert(backup: JsonObject?) {
    val line = if (backup?.flag("enabled") != true) backupSummary(backup) else backupCard(backup)?.attention
    line?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(start = 2.dp, top = 8.dp)) }
}

@Composable
private fun BackupPanel(backup: JsonObject?) {
    val card = backupCard(backup) ?: return
    val small = MaterialTheme.typography.bodySmall
    if (card.empty) {
        Text("No confirmed backup yet.", color = TextSecondary, style = small)
        backupUpdate(backup)?.let { Text(it, color = TextSecondary, style = small) }
        return
    }
    var night by remember { mutableStateOf<BackupNight?>(null) }
    Surface(color = Panel, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Text("Backup", style = MaterialTheme.typography.titleSmall)
                Text(backupSummary(backup), color = TextMuted, style = small)
            }
            if (card.nights.isNotEmpty()) Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                card.nights.forEach { n ->
                    Box(Modifier.size(30.dp).background(PanelMuted, RoundedCornerShape(8.dp)).clickable { night = if (night == n) null else n },
                        contentAlignment = Alignment.Center) {
                        Text(n.mark, color = if (n.ok) Emerald else Amber)
                    }
                }
            }
            night?.let { Text(it.detail, color = if (!it.ok) Amber else TextSecondary, style = small) }
            card.lastNight?.let { Text(it, color = TextSecondary, style = small) }
            card.check?.let { Text(it, color = if (card.checkOk) TextSecondary else Amber, style = small) }
            card.disk?.let { d ->
                Text(d.line, style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
                LinearProgressIndicator(progress = { if (d.total > 0) (d.used.toFloat() / d.total).coerceIn(0f, 1f) else 0f },
                    modifier = Modifier.fillMaxWidth().height(6.dp), color = Emerald, trackColor = PanelMuted)
                Text(d.holding, color = TextSecondary, style = small)
                d.pruned?.let { Text(it, color = TextSecondary, style = small) }
                Text("Drive health: ${d.health}", color = if (d.failing) Amber else TextSecondary, style = small,
                    fontWeight = if (d.failing) FontWeight.Bold else FontWeight.Normal)
            }
            card.rates?.let { Text(it, color = TextMuted, style = small) }
            backupUpdate(backup)?.let { line ->
                val ok = (backup?.get("update") as? JsonObject)?.flag("ok") == true
                Text(line, color = if (ok) TextSecondary else Amber, style = small)
            }
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
    "thaw" -> "Thaw folder"
    else -> "Ask an agent"
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ArtifactSheet(a: JsonObject, activity: JsonObject?, canAct: Boolean, busy: Boolean, days: Long, onClose: () -> Unit, onAction: (String) -> Unit) {
    var technical by remember { mutableStateOf(false) }
    ModalBottomSheet(onDismissRequest = onClose, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp, vertical = 8.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text(a.text("name"), style = MaterialTheme.typography.titleLarge)
            Text("${storageSize(measuredArtifactBytes(a))} · ${artifactSummary(a)}", color = TextSecondary)
            StorageFacts(a, activity)
            a.text("description").ifBlank { a.text("note") }.takeIf { it.isNotBlank() }?.let { MarkdownText(it, richLinks = true) }
            if (a.text("state") == "unrecorded") Text("An agent can recover the context from git, saved files and job history. You do not need to reconstruct it.", color = TextSecondary)
            a.rows("consumers").takeIf { it.isNotEmpty() }?.let { Text("Used by " + it.joinToString { c -> c.text("ref") }, color = TextSecondary) }
            Text(if (!a.flag("backup_due") && a.text("backed_up_at").isNotBlank()) "Backup confirmed" else "No current backup confirmed", color = TextMuted, style = MaterialTheme.typography.bodySmall)
            artifactReport(a)?.let { MarkdownText(it, richLinks = true) }
            if (canAct) {
                Button(onClick = { onAction("investigate") }, enabled = !busy && a.text("investigating").isBlank()) { Text(if (a.text("investigating").isBlank()) "Ask an agent to investigate" else "Agent investigating") }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (a.text("state") == "frozen" && a.text("op").isEmpty()) OutlinedButton(onClick = { onAction("thaw") }, enabled = !busy) { Text("Thaw") }
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

// ---- freeze, thaw and move (sm#2325) ----------------------------------------
private fun opTone(kind: String): Color = when (kind) { "freezing" -> Cyan; "thawing" -> Orange; else -> Violet }
private fun opIcon(kind: String): ImageVector = when (kind) {
    "freezing" -> Icons.Rounded.AcUnit; "thawing" -> Icons.Rounded.WbSunny; else -> Icons.Rounded.DriveFileMove
}

@Composable
private fun OpBadge(kind: String, failed: Boolean, size: Int, running: Boolean = false) {
    val tone = if (failed) Rose else opTone(kind)
    val turn = if (running && kind == "freezing") rememberInfiniteTransition(label = "freeze")
        .animateFloat(0f, 360f, infiniteRepeatable(tween(9000, easing = LinearEasing)), label = "turn").value else 0f
    Box(Modifier.size(size.dp).background(tone.copy(alpha = 0.14f), RoundedCornerShape((size * 0.29f).dp)), contentAlignment = Alignment.Center) {
        Icon(opIcon(kind), contentDescription = null, tint = tone, modifier = Modifier.size((size * 0.53f).dp).rotate(turn))
    }
}

/** One freeze, thaw or move: what, how far along, and who asked. */
@Composable
internal fun OperationCard(op: JsonObject) {
    val kind = op.text("kind")
    val k = opKind(kind)
    val state = op.text("state")
    val settled = state == "done" || state == "failed"
    val tone = if (state == "failed") Rose else opTone(kind)
    Surface(color = Panel, shape = RoundedCornerShape(12.dp), border = androidx.compose.foundation.BorderStroke(1.dp, Border), modifier = Modifier.fillMaxWidth()) {
        Row(Modifier.height(IntrinsicSize.Min)) {
            Box(Modifier.width(3.dp).fillMaxHeight().background(tone))
            Column(Modifier.padding(start = 14.dp, end = 14.dp, top = 14.dp, bottom = 12.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    OpBadge(kind, state == "failed", 34, running = state == "running")
                    Column(Modifier.weight(1f)) {
                        Text((if (state == "failed") "${k.now} failed" else if (settled) k.past else k.now).uppercase(), color = tone,
                            style = MaterialTheme.typography.labelSmall, fontWeight = FontWeight.SemiBold, letterSpacing = 0.8.sp)
                        Text(opName(op), fontFamily = FontFamily.Monospace, fontWeight = FontWeight.SemiBold, fontSize = 14.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                    Text(opSizes(op), color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                }
                // Finished operations drop the steps; the status says done or failed.
                if (!settled) Steps(op, tone) else Spacer(Modifier.height(10.dp))
                Text((if (state == "done") "✓ " else "") + opStatus(op), style = MaterialTheme.typography.bodySmall,
                    color = when { state == "done" -> Emerald; state == "failed" || (state == "waiting" && op.text("error").isNotEmpty()) -> Amber; else -> MaterialTheme.colorScheme.onSurface })
                Text("Asked by ${actorLabel(op)}" + (actorId(op)?.let { " · $it" } ?: "") + " · ${clock(op.text("created_at"))}",
                    color = TextMuted, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(top = 2.dp))
            }
        }
    }
}

/** Queued → Copying → Finishing → Done; the copy segment fills with the estimate. */
@Composable
private fun Steps(op: JsonObject, tone: Color) {
    val step = opStep(op)
    val progress = opProgress(op)
    Row(Modifier.fillMaxWidth().padding(top = 16.dp, bottom = 10.dp)) {
        OP_STEPS.forEachIndexed { i, label ->
            val past = step >= 0 && (i < step || (i == step && step == 3))
            val now = i == step && step != 3
            Column(if (i < 3) Modifier.weight(1f) else Modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(Modifier.size(10.dp).background(if (past) tone else if (now) tone.copy(alpha = 0.25f) else Panel, CircleShape)
                        .border(2.dp, if (step < 0) Rose else if (past || now) tone else Border, CircleShape))
                    if (i < 3) Box(Modifier.weight(1f).padding(horizontal = 4.dp).height(2.dp).background(Border, RoundedCornerShape(2.dp))) {
                        val fill = if (i < step) 1f else if (i == 1 && step == 1) progress ?: 0f else 0f
                        Box(Modifier.fillMaxWidth(fill).fillMaxHeight().background(tone, RoundedCornerShape(2.dp)))
                    }
                }
                Text(label, fontSize = 11.sp, color = if (now) MaterialTheme.colorScheme.onSurface else if (past) TextSecondary else TextMuted,
                    fontWeight = if (now) FontWeight.SemiBold else FontWeight.Normal)
            }
        }
    }
}

/** A finished operation in the history list. */
@Composable
private fun HistoryRow(op: JsonObject) {
    val kind = op.text("kind")
    val k = opKind(kind)
    val failed = op.text("state") == "failed"
    Column {
        HorizontalDivider(color = Border.copy(alpha = 0.6f))
        Row(Modifier.fillMaxWidth().padding(vertical = 12.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OpBadge(kind, failed, 28)
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                Text(buildAnnotatedString {
                    withStyle(SpanStyle(color = TextSecondary)) { append(if (failed) "${k.now} failed " else "${k.past} ") }
                    withStyle(SpanStyle(fontFamily = FontFamily.Monospace, fontWeight = FontWeight.SemiBold)) { append(opName(op)) }
                }, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(buildAnnotatedString {
                    append(opSizes(op).let { if (it.isEmpty()) "" else "$it · " })
                    opSaved(op)?.takeIf { it > 0 }?.let { withStyle(SpanStyle(color = Emerald, fontWeight = FontWeight.SemiBold)) { append("saved ${storageSize(it)}") }; append(" · ") }
                    append(clock(op.text("finished_at")))
                }, color = TextSecondary, fontSize = 12.sp)
                Text(actorLabel(op) + (actorId(op)?.let { " · $it" } ?: "") + (opTook(op)?.let { " · took $it" } ?: "") +
                    (if (failed && op.text("error").isNotEmpty()) " · ${op.text("error")}" else ""), color = TextMuted, fontSize = 12.sp)
            }
        }
    }
}

/** What freezing saved, and what froze, thawed or moved lately. */
@Composable
private fun FreezeCard(activity: JsonObject) {
    val s = activity["savings"] as? JsonObject
    val history = activity.rows("history")
    var all by remember { mutableStateOf(false) }
    val data = s?.number("data_bytes") ?: 0
    val frozen = s?.number("frozen_bytes") ?: 0
    Surface(color = Panel, shape = RoundedCornerShape(12.dp), border = androidx.compose.foundation.BorderStroke(1.dp, Border), modifier = Modifier.fillMaxWidth().padding(top = 8.dp)) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.SpaceBetween) {
                Column {
                    Text("Freezing saved", color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                    Text(storageSize(s?.number("saved_bytes") ?: 0), color = Cyan, fontSize = 30.sp, fontWeight = FontWeight.SemiBold, letterSpacing = (-1).sp)
                }
                ratio(data, frozen)?.let {
                    Text("$it smaller", color = Cyan, fontSize = 12.sp, fontWeight = FontWeight.SemiBold,
                        modifier = Modifier.background(Cyan.copy(alpha = 0.14f), RoundedCornerShape(99.dp)).padding(horizontal = 10.dp, vertical = 4.dp))
                }
            }
            Text(savingsLine(s), color = TextSecondary, style = MaterialTheme.typography.bodySmall)
            if (data > 0) {
                Box(Modifier.fillMaxWidth().height(10.dp).background(Cyan.copy(alpha = 0.16f), RoundedCornerShape(9.dp))) {
                    Box(Modifier.fillMaxWidth((frozen.toFloat() / data).coerceIn(0.02f, 1f)).fillMaxHeight().background(Cyan, RoundedCornerShape(9.dp)))
                }
                Row(horizontalArrangement = Arrangement.spacedBy(18.dp)) {
                    Legend(Cyan, "Archives ${storageSize(frozen)}")
                    Legend(Cyan.copy(alpha = 0.3f), "Saved ${storageSize(s?.number("saved_bytes"))}")
                }
            }
            s?.rows("roots")?.filter { it.number("folders") > 0 }?.takeIf { it.size > 1 }?.forEach { r ->
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                    Text(r.text("root"), color = TextSecondary, fontSize = 12.sp)
                    Text("${r.number("folders")} folders · saved ${storageSize(r.number("saved_bytes"))}", color = TextSecondary, fontSize = 12.sp)
                }
            }
            if (history.isNotEmpty()) {
                Text("Recent freezes, thaws and moves", color = TextSecondary, fontSize = 12.sp, fontWeight = FontWeight.SemiBold, modifier = Modifier.padding(top = 8.dp))
                Column { (if (all) history else history.take(5)).forEach { HistoryRow(it) } }
                if (history.size > 5) TextButton(onClick = { all = !all }, contentPadding = PaddingValues(0.dp)) { Text(if (all) "Show fewer" else "Show all ${history.size}  ›", color = Cyan) }
            }
        }
    }
}

@Composable
private fun Legend(color: Color, label: String) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        Box(Modifier.size(8.dp).background(color, RoundedCornerShape(2.dp)))
        Text(label, color = TextSecondary, fontSize = 12.sp)
    }
}

/** Where a folder lives, what freezing it saves, and its operations. */
@Composable
private fun StorageFacts(a: JsonObject, activity: JsonObject?) {
    val frozen = a.text("state") == "frozen"
    val data = (a["data_bytes"] as? JsonPrimitive)?.longOrNull
    val archive = (a["frozen_bytes"] as? JsonPrimitive)?.longOrNull
    val facts = listOfNotNull(
        "State" to (a.text("op").takeIf { it.isNotEmpty() }?.let { opKind(it).now } ?: a.text("state").replaceFirstChar { it.uppercase() }),
        placementLabel(a)?.let { "Lives on" to it },
        data?.let { "Data" to storageSize(it) },
        if (frozen) archive?.let { "Archive" to storageSize(it) + (ratio(data, it)?.let { r -> " · $r smaller" } ?: "") }
        else (a["disk_bytes"] as? JsonPrimitive)?.longOrNull?.let { "On disk" to storageSize(it) },
        if (frozen) (a["saved_bytes"] as? JsonPrimitive)?.longOrNull?.let { "Saved" to storageSize(it) } else null,
        if (frozen) thawTime(a)?.let { "Thaw time" to it } else null,
    )
    val mine = { o: JsonObject -> a.text("name") in opNames(o) && o.text("root") == a.text("root") }
    val op = activity?.rows("operations")?.firstOrNull { mine(it) && it.text("state") in listOf("running", "waiting") }
    Surface(color = PanelMuted, shape = RoundedCornerShape(10.dp), border = androidx.compose.foundation.BorderStroke(1.dp, Border)) {
        Column {
            facts.chunked(2).forEachIndexed { i, pair ->
                if (i > 0) HorizontalDivider(color = Border)
                Row(Modifier.height(IntrinsicSize.Min)) {
                    pair.forEachIndexed { j, (label, value) ->
                        if (j > 0) Box(Modifier.width(1.dp).fillMaxHeight().background(Border))
                        Column(Modifier.weight(1f).padding(horizontal = 12.dp, vertical = 10.dp)) {
                            Text(label.uppercase(), color = TextMuted, fontSize = 11.sp, letterSpacing = 0.6.sp)
                            Text(value, fontSize = 14.sp, color = if (label == "Saved") Emerald else MaterialTheme.colorScheme.onSurface,
                                fontWeight = if (label == "Saved") FontWeight.SemiBold else FontWeight.Normal)
                        }
                    }
                    if (pair.size == 1) Spacer(Modifier.weight(1f))
                }
            }
        }
    }
    (a["placement"] as? JsonObject)?.text("path")?.let { Text(it, fontFamily = FontFamily.Monospace, fontSize = 12.sp, color = TextSecondary) }
    op?.let { OperationCard(it) }
    activity?.rows("history")?.filter(mine)?.take(4)?.takeIf { it.isNotEmpty() }?.let { past -> Column { past.forEach { HistoryRow(it) } } }
}
