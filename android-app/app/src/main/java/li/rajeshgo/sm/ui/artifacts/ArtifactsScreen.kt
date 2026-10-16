package li.rajeshgo.sm.ui.artifacts

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.unit.dp
import kotlinx.serialization.json.JsonObject
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.navigation.AppTopBar
import li.rajeshgo.sm.ui.queue.SectionHeader
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Panel

private fun bytes(n: Long): String = if (n >= 1073741824) "%.1f GB".format(n / 1073741824.0) else "%.1f MB".format(n / 1048576.0)

@Composable
fun ArtifactsScreen(onBack: () -> Unit, menu: AppMenuActions, viewModel: ArtifactsViewModel, onNavigate: (String) -> Unit) {
    val state by viewModel.state.collectAsState()
    val data = state.data
    var epic by remember { mutableStateOf("") }
    var confirmation by remember { mutableStateOf<Pair<JsonObject,String>?>(null) }
    val listState = rememberLazyListState()
    LaunchedEffect(data, ArtifactsNavigation.name) {
        val target = ArtifactsNavigation.name ?: return@LaunchedEffect
        var offset = 4 + data?.rows("filesystems").orEmpty().size
        for ((_, rows) in artifactGroups(data?.rows("artifacts").orEmpty(), epic)) {
            val index = rows.indexOfFirst { it.text("name") == target }
            if (index >= 0) { listState.scrollToItem(offset + 1 + index); break }
            offset += rows.size + 1
        }
    }
    Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding()) {
        AppTopBar(title = "Artifacts", menu = menu, onBack = onBack)
        LazyColumn(Modifier.weight(1f), state = listState, contentPadding = PaddingValues(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            item {
                state.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                TextButton(onClick = viewModel::refresh) { Text("Refresh") }
                if (data == null) Text("Loading artifacts…")
            }
            item { Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                val frozen = data?.rows("artifacts").orEmpty().filter { it.text("state") == "frozen" }
                Surface(color = Panel, modifier = Modifier.weight(1f)) { Column(Modifier.padding(12.dp)) { Text("Frozen", color = Cyan); Text(bytes(frozen.sumOf(::artifactSize))); Text("${frozen.size} folders") } }
                Surface(color = Panel, modifier = Modifier.weight(1f)) { Column(Modifier.padding(12.dp)) { Text("Needs you", color = Cyan); Text("${data?.number("needs_you_count") ?: 0}") } }
            } }
            items(data?.rows("filesystems").orEmpty()) { f ->
                Surface(color = Panel) { Column(Modifier.fillMaxWidth().padding(16.dp)) {
                    Text(f.text("path"), color = Cyan)
                    LinearProgressIndicator(progress = { ((f.number("bytes")-f.number("free")).toFloat()/f.number("bytes").coerceAtLeast(1)).coerceIn(0f,1f) }, modifier = Modifier.fillMaxWidth())
                    Text("${bytes(f.number("free"))} free · floor ${bytes(f.number("floor"))} · enough ${bytes(f.number("enough"))}")
                    Text("Outstanding ${bytes(f.number("outstanding"))}")
                } }
            }
            item { Surface(color = Panel) { Column(Modifier.fillMaxWidth().padding(16.dp)) {
                SectionHeader("BACKUP")
                val backup = data?.get("backup") as? JsonObject
                val runs = backup?.rows("runs").orEmpty()
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                    (6 downTo 0).forEach { ago ->
                        val day = java.time.LocalDate.now().minusDays(ago.toLong()).toString()
                        val run = runs.firstOrNull { it.text("started_at").startsWith(day) }
                        Column { Text(day.takeLast(5)); Text(if (run == null) "—" else if (run.text("state") in listOf("acked","succeeded")) "✓" else "✗") }
                    }
                }
                if (runs.isEmpty()) Text("No backup runs recorded.")
                runs.forEach { Text("${if (it.text("state") == "succeeded") "✓" else "✗"} ${it.text("started_at")} · ${bytes(it.number("include_bytes"))}") }
                Text("Confirmed: ${backup?.text("confirmed_at")?.ifEmpty { "not reported" } ?: "not reported"}")
                Text("Weekly check: ${backup?.text("weekly_check")?.ifEmpty { "not reported" } ?: "not reported"}")
                Text("Backup disk use: ${backup?.text("disk_used_bytes")?.ifEmpty { "not reported" } ?: "not reported"}")
                Text("Holding old versions: ${backup?.text("holding_bytes")?.ifEmpty { "not reported" } ?: "not reported"}")
                Text("Scope: ${backup?.get("scope_roots") ?: "not reported"} · skipped: ${backup?.text("skipped_count")?.ifEmpty { "not reported" } ?: "not reported"}")
                Text("Download, disk write, restore rates: unmeasured. Rate in force: ${backup?.text("rate_default")} /s (default)")
            } } }
            item { OutlinedTextField(value = epic, onValueChange = { epic = it }, label = { Text("Epic (empty for all)") }, modifier = Modifier.fillMaxWidth()) }
            artifactGroups(data?.rows("artifacts").orEmpty(),epic).forEach { (title, rows) ->
                item { SectionHeader("$title · ${rows.size} · ${bytes(rows.sumOf(::artifactSize))}") }
                items(rows, key = { it.text("root") + it.text("name") }) { a ->
                    var expanded by remember(a.text("name"), ArtifactsNavigation.name) { mutableStateOf(ArtifactsNavigation.name == a.text("name")) }
                    Surface(color = Panel) { Column(Modifier.fillMaxWidth().padding(16.dp)) {
                        Text(a.text("name"), color = Cyan, modifier = Modifier.fillMaxWidth().clickable { expanded = !expanded })
                        Text("${bytes(a.number("disk_bytes"))}${if(a.text("state") == "frozen") " → ${bytes(a.number("frozen_bytes"))}" else ""} · ${a.text("location")} · ${a.text("state")}")
                        if (a.text("gpu_hours").isNotEmpty() || a.text("gpu_cost_usd").isNotEmpty()) Text("GPU")
                        Text(a.text("note").ifEmpty { "No note recorded" }, fontStyle = FontStyle.Italic)
                        (a["needs_you"] as? JsonObject)?.let { Text(it["reasons"].toString().replace('_',' '), color = Cyan) }
                        Text(a.rows("consumers").joinToString(" · ") { "${it.text("ref")} (${it.text("state")})" })
                        Text("${a.text("owner_session_name")} · created ${a.text("created_at")} · rebuild ${a.number("rebuild_seconds")}s")
                        Text(if (!a.flag("backup_due") && a.text("backed_up_at").isNotEmpty()) "backed up${if (!a.flag("rebuildable")) " (not rebuildable)" else ""}" else "not backed up · ${if (a.flag("rebuildable")) "rebuilds in ${a.number("rebuild_seconds")}s; return time not measured" else "not rebuildable"}")
                        if (a.text("backup_return_seconds").isNotEmpty()) Text("Estimated packed size ${bytes(a.number("backup_ship_bytes"))} · return ${a.number("backup_return_seconds")}s versus rebuild ${a.number("rebuild_seconds")}s")
                        if(a.text("investigating").isNotEmpty()) Text("investigating · ${a.text("investigating")}")
                        (a["report"] as? JsonObject)?.let { Text(it.text("detail_json")) }
                        if (expanded) {
                            Text(a.text("description")); Text("Recent use: ${a.text("last_used_at")}")
                            Text("Recipe"); a.rows("jobs").forEach { Text(it.toString()) }
                            a.rows("events").take(10).forEach { Text("${it.text("at")} · ${it.text("kind")} · ${it.text("detail_json")}") }
                            if(data?.flag("can_act") == true) {
                                (listOf("delete") + when(a.text("state")) { "frozen" -> listOf("thaw","keep_live"); "live" -> listOf("keep_live"); else -> emptyList() } + listOf("investigate")).forEach { action ->
                                    TextButton(enabled = !state.busy && (action != "investigate" || a.text("investigating").isEmpty()), onClick = { confirmation = a to action }) {
                                        Text(when(action) { "keep_live" -> "Keep live ${data.number("keep_live_days")} days"; else -> action.replaceFirstChar { it.uppercase() } })
                                    }
                                }
                            }
                        }
                    } }
                }
            }
            item { Text("Last 7 days: " + data?.rows("recent").orEmpty().joinToString(" · ") { "${it.number("count")} ${it.text("kind")}" }) }
        }
        li.rajeshgo.sm.ui.navigation.AppBottomNav(
            currentRoute = li.rajeshgo.sm.ui.navigation.Routes.ARTIFACTS,
            onInbox = { onNavigate(li.rajeshgo.sm.ui.navigation.Routes.INBOX) },
            onWatch = { onNavigate(li.rajeshgo.sm.ui.navigation.Routes.WATCH) },
            onBoard = { onNavigate(li.rajeshgo.sm.ui.navigation.Routes.BOARD) },
            onQueue = { onNavigate(li.rajeshgo.sm.ui.navigation.Routes.QUEUE) },
            modifier = Modifier.padding(12.dp),
        )
    }
    confirmation?.let { (a,action) -> AlertDialog(onDismissRequest = { confirmation = null }, title = { Text(action.replace('_',' ')) },
        text = { Text("${a.text("name")} · ${bytes(artifactSize(a))}${if(!a.flag("rebuildable")) " · not rebuildable" else ""}${a.rows("dependents").takeIf { it.isNotEmpty() }?.joinToString(prefix = " · input of ") { it.text("name") }.orEmpty()}") },
        confirmButton = { TextButton(onClick = { confirmation = null; viewModel.act(a,action) }) { Text("Confirm") } },
        dismissButton = { TextButton(onClick = { confirmation = null }) { Text("Cancel") } }) }
}
