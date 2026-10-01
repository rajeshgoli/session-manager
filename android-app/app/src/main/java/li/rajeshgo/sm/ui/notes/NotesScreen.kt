package li.rajeshgo.sm.ui.notes

import android.content.ClipData
import android.content.ClipboardManager
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.flow.first
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.navigation.AppTopBar
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.watch.MarkdownText
import li.rajeshgo.sm.ui.queue.shortDuration
import java.time.Duration
import java.time.OffsetDateTime

@Composable
fun NotesScreen(onBack: () -> Unit, menu: AppMenuActions, viewModel: NotesViewModel = viewModel()) {
    val state by viewModel.state.collectAsState()
    val context = LocalContext.current
    val preferences = remember(context) { context.getSharedPreferences("owner_notes", android.content.Context.MODE_PRIVATE) }
    val clipboard = remember(context) { context.getSystemService(ClipboardManager::class.java) }
    var text by remember(state.note?.id) { mutableStateOf(TextFieldValue(state.draft)) }
    LaunchedEffect(state.draft) {
        if (text.text != state.draft) text = text.copy(text = state.draft, selection = TextRange(state.draft.length))
    }
    val chosen = text.selection.let { range ->
        if (range.collapsed) state.draft else state.draft.substring(range.min.coerceAtMost(state.draft.length), range.max.coerceAtMost(state.draft.length))
    }
    var action by remember { mutableStateOf<String?>(null) }
    var actionText by remember { mutableStateOf<String?>(null) }
    var preview by remember { mutableStateOf(false) }
    var deleteConfirm by remember { mutableStateOf(false) }
    var repo by remember { mutableStateOf(preferences.getString("repo", "").orEmpty()) }
    var workspace by remember { mutableStateOf(preferences.getString("workspace", "").orEmpty()) }
    var provider by remember { mutableStateOf(preferences.getString("provider", "claude").orEmpty()) }
    var model by remember { mutableStateOf(preferences.getString("model", "").orEmpty()) }
    var effort by remember { mutableStateOf(preferences.getString("effort", "high").orEmpty()) }
    var repos by remember { mutableStateOf(emptyList<String>()) }
    var workspaces by remember { mutableStateOf(emptyList<String>()) }
    val settings = remember(context) { SettingsRepository(context.applicationContext) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    LaunchedEffect(settings) {
        val url = settings.serverUrl.first(); val token = settings.accessToken.first()
        if (url.isNotBlank() && token.isNotBlank()) {
            runCatching { repository.fetchBoard(url, token) }.onSuccess { board ->
                repos = board.repos.map { it.repo }.distinct(); if (repo.isBlank()) repo = repos.firstOrNull().orEmpty()
            }
            runCatching { repository.fetchSessions(url, token) }.onSuccess { sessions ->
                workspaces = sessions.map { it.workingDir }.filter(String::isNotBlank).distinct()
                if (workspace.isBlank()) workspace = workspaces.firstOrNull().orEmpty()
            }
        }
    }
    fun toast(message: String) { Toast.makeText(context, message, Toast.LENGTH_LONG).show() }
    val safeMenu = AppMenuActions(
        onNewSession = { viewModel.finish(menu.onNewSession) },
        onOpenHistory = { viewModel.finish(menu.onOpenHistory) },
        onOpenNotes = menu.onOpenNotes,
        onOpenGuestbook = { viewModel.finish(menu.onOpenGuestbook) },
        onOpenAnalytics = { viewModel.finish(menu.onOpenAnalytics) },
        onOpenSettings = { viewModel.finish(menu.onOpenSettings) },
        onReportBug = { viewModel.finish(menu.onReportBug) },
    )
    BackHandler { viewModel.finish(onBack) }
    Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding()) {
        AppTopBar(title = "Notes", menu = safeMenu, current = Routes.NOTES, onBack = { viewModel.finish(onBack) }, onRefresh = viewModel::refresh)
        Column(Modifier.fillMaxSize().imePadding().verticalScroll(rememberScrollState()).padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(value = state.query, onValueChange = viewModel::search,
                    label = { Text("Search notes") }, singleLine = true, modifier = Modifier.weight(1f))
                Button(onClick = viewModel::new) { Text("+ New") }
            }
            state.status.takeIf(String::isNotBlank)?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
            state.hits.forEach { hit ->
                Surface(shape = RoundedCornerShape(10.dp), tonalElevation = 2.dp,
                    modifier = Modifier.fillMaxWidth().clickable { viewModel.open(hit.id) }) {
                    Column(Modifier.padding(12.dp)) {
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            Text(hit.title.ifBlank { "Untitled" }, style = MaterialTheme.typography.titleSmall, modifier = Modifier.weight(1f))
                            val age = runCatching { shortDuration(Duration.between(OffsetDateTime.parse(hit.updatedAt), OffsetDateTime.now()).seconds.coerceAtLeast(0)) }.getOrNull()
                            age?.let { Text("$it ago", style = MaterialTheme.typography.labelSmall) }
                        }
                        val snippet = hit.snippet.take(300)
                        val highlight = MaterialTheme.colorScheme.tertiaryContainer
                        Text(buildAnnotatedString {
                            var cursor = 0
                            val matches = if (state.query.isBlank()) emptyList() else Regex(Regex.escape(state.query), RegexOption.IGNORE_CASE).findAll(snippet).toList()
                            matches.forEach { match ->
                                append(snippet.substring(cursor, match.range.first))
                                withStyle(SpanStyle(background = highlight)) { append(match.value) }
                                cursor = match.range.last + 1
                            }
                            append(snippet.substring(cursor))
                        }, maxLines = 3, style = MaterialTheme.typography.bodySmall)
                        if (state.note?.id == hit.id) Text("Open", style = MaterialTheme.typography.labelSmall)
                        Row(horizontalArrangement = Arrangement.spacedBy(2.dp)) {
                            TextButton(onClick = { viewModel.withNoteText(hit.id) { body ->
                                clipboard?.setPrimaryClip(ClipData.newPlainText(hit.title, body)); toast("Copied")
                            } }) { Text("Copy") }
                            TextButton(onClick = { viewModel.withNoteText(hit.id) { body -> actionText = body; action = "agent" } }) { Text("Start agent") }
                            TextButton(onClick = { viewModel.withNoteText(hit.id) { body -> actionText = body; action = "issue" } }) { Text("File ticket") }
                        }
                    }
                }
            }
            state.note?.let { note ->
                HorizontalDivider()
                Text(note.title.ifBlank { "Untitled" }, style = MaterialTheme.typography.titleMedium)
                Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                    TextButton(onClick = { clipboard?.setPrimaryClip(ClipData.newPlainText(note.title, chosen)); toast("Copied") }) { Text("Copy") }
                    TextButton(onClick = { actionText = chosen; action = "agent" }) { Text("Start agent") }
                    TextButton(onClick = { actionText = chosen; action = "issue" }) { Text("File ticket") }
                }
                Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                    TextButton(onClick = { preview = !preview }) { Text(if (preview) "Edit" else "Preview") }
                    TextButton(onClick = viewModel::history) { Text("History") }
                    TextButton(onClick = { deleteConfirm = true }) { Text("Delete") }
                }
                if (preview) MarkdownText(state.draft)
                else OutlinedTextField(value = text, onValueChange = { text = it; viewModel.edit(it.text) },
                    modifier = Modifier.fillMaxWidth().heightIn(min = 280.dp).onFocusChanged { if (!it.isFocused) viewModel.save() },
                    textStyle = androidx.compose.ui.text.TextStyle(fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace),
                    label = { Text("Note") }, minLines = 12)
                state.revisions.forEach { revision ->
                    TextButton(onClick = { viewModel.restore(revision.version) }) { Text("Restore version ${revision.version} · ${revision.at}") }
                }
            }
        }
    }
    if (state.conflict != null) AlertDialog(onDismissRequest = {}, title = { Text("Changed on another device") },
        text = { Text("Choose which version to keep.") },
        confirmButton = { TextButton(onClick = viewModel::keepMine) { Text("Keep mine") } },
        dismissButton = { TextButton(onClick = viewModel::loadTheirs) { Text("Load theirs") } })
    if (deleteConfirm) AlertDialog(onDismissRequest = { deleteConfirm = false }, title = { Text("Delete this note?") },
        confirmButton = { TextButton(onClick = { deleteConfirm = false; viewModel.delete() }) { Text("Delete") } },
        dismissButton = { TextButton(onClick = { deleteConfirm = false }) { Text("Cancel") } })
    action?.let { kind ->
        val payload = actionText.orEmpty()
        AlertDialog(onDismissRequest = { action = null }, title = { Text(if (kind == "agent") "Start agent" else "File ticket") },
            text = { Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(payload.take(80).ifBlank { "Empty note" }, style = MaterialTheme.typography.bodySmall)
                if (kind == "issue") {
                    Choice("Repository", repo, repos) { repo = it }
                } else {
                    Choice("Workspace", workspace, workspaces) { workspace = it }
                    Choice("Provider", provider, listOf("claude", "codex-fork")) { provider = it }
                    OutlinedTextField(model, { model = it }, label = { Text("Model · optional") }, singleLine = true)
                    Choice("Effort", effort, listOf("medium", "high", "xhigh")) { effort = it }
                }
            } },
            confirmButton = { TextButton(enabled = payload.isNotBlank() && if (kind == "issue") repo.isNotBlank() else workspace.isNotBlank(), onClick = {
                if (kind == "issue") {
                    preferences.edit().putString("repo", repo).apply()
                    viewModel.fileIssue(repo, payload, ::toast)
                } else {
                    preferences.edit().putString("workspace", workspace).putString("provider", provider)
                        .putString("model", model).putString("effort", effort).apply()
                    viewModel.startAgent(payload, provider, workspace, model.ifBlank { null }, effort, ::toast)
                }
                action = null
            }) { Text(if (kind == "issue") "File ticket" else "Start") } },
            dismissButton = { TextButton(onClick = { action = null }) { Text("Cancel") } })
    }
}

@Composable
private fun Choice(label: String, value: String, choices: List<String>, onChange: (String) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        OutlinedButton(onClick = { open = true }) { Text("$label: ${value.ifBlank { "Choose" }}") }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            choices.forEach { choice -> DropdownMenuItem(text = { Text(choice) }, onClick = { onChange(choice); open = false }) }
        }
    }
}
