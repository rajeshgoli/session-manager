package li.rajeshgo.sm.ui.watch

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.CreateSessionRequest

fun sessionTemplate(source: ClientSession?): CreateSessionRequest = CreateSessionRequest(
    provider = source?.provider ?: "claude",
    workingDir = source?.workingDir ?: "/Users/rajesh/projects/fractal-algo-rust",
    model = source?.model,
    reasoningEffort = if (source == null) "high" else source.reasoningEffort,
)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun CreateSessionSheet(
    source: ClientSession?,
    sessions: List<ClientSession>,
    loadModels: suspend (String) -> List<String>,
    busy: Boolean,
    error: String?,
    onDismiss: () -> Unit,
    onCreate: (CreateSessionRequest) -> Unit,
) {
    val template = remember(source?.id) { sessionTemplate(source) }
    var provider by rememberSaveable(source?.id) { mutableStateOf(template.provider) }
    var model by rememberSaveable(source?.id) { mutableStateOf(template.model.orEmpty()) }
    var effort by rememberSaveable(source?.id) { mutableStateOf(template.reasoningEffort.orEmpty()) }
    var directory by rememberSaveable(source?.id) { mutableStateOf(template.workingDir) }
    var customModel by rememberSaveable { mutableStateOf(false) }
    var name by rememberSaveable(source?.id) { mutableStateOf("") }
    var prompt by rememberSaveable(source?.id) { mutableStateOf("") }
    var customDirectory by rememberSaveable { mutableStateOf(false) }
    val directories = (listOf("/Users/rajesh/projects/fractal-algo-rust", "/Users/rajesh/projects/session-manager", "/Users/rajesh/projects/codex-fork") + sessions.map { it.workingDir } + directory).distinct()
    var catalog by remember(provider) { mutableStateOf(emptyList<String>()) }
    var modelsLoading by remember(provider) { mutableStateOf(true) }
    var modelsError by remember(provider) { mutableStateOf(false) }
    var catalogAttempt by remember { mutableStateOf(0) }
    LaunchedEffect(provider, catalogAttempt) {
        modelsLoading = true
        modelsError = false
        try { catalog = loadModels(provider) }
        catch (error: kotlinx.coroutines.CancellationException) { throw error }
        catch (_: Exception) { modelsError = true }
        finally { modelsLoading = false }
    }
    val models = (listOf("") + catalog + sessions.filter { it.provider == provider }.mapNotNull { it.model } + listOf(model)).distinct()
    ModalBottomSheet(sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true), onDismissRequest = { if (!busy) onDismiss() }) {
        Column(Modifier.fillMaxWidth().imePadding().verticalScroll(rememberScrollState()).padding(horizontal = 24.dp).padding(bottom = 24.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Text(if (source == null) "New session" else "Clone ${sessionDisplayName(source)}", style = MaterialTheme.typography.headlineSmall)
            Text(if (source == null) "Choose a workspace and start something new." else "A fresh conversation with the same provider, model, effort and workspace.", style = MaterialTheme.typography.bodyMedium)
            SessionChoice("Provider", provider, (listOf("claude", "codex") + provider).distinct(), !busy) {
                provider = it; model = ""; effort = "high"; customModel = false
            }
            SessionChoice("Model", model, models + "Other model…", !busy, emptyLabel = "Provider default") {
                if (it == "Other model…") customModel = true else { model = it; customModel = false }
            }
            if (modelsLoading) Text("Loading available models…", style = MaterialTheme.typography.bodySmall)
            if (modelsError) TextButton(onClick = { catalogAttempt++ }) { Text("Couldn't load models · Retry") }
            if (customModel) OutlinedTextField(model, { model = it }, label = { Text("Model identifier") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
            SessionChoice("Effort", effort, (listOf("medium", "high") + effort).distinct(), !busy) { effort = it }
            SessionChoice("Workspace", directory, directories + "Other directory…", !busy, shortPaths = true) {
                if (it == "Other directory…") customDirectory = true else { directory = it; customDirectory = false }
            }
            if (customDirectory) OutlinedTextField(directory, { directory = it }, label = { Text("Absolute directory path") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
            else Text(directory, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            OutlinedTextField(name, { name = it }, label = { Text("Name · optional") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
            OutlinedTextField(prompt, { prompt = it }, label = { Text("First message · optional") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), minLines = 2, maxLines = 5)
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Button(onClick = { onCreate(CreateSessionRequest(provider, directory.trim(), model.ifBlank { null }, effort.ifBlank { null }, name.trim().ifBlank { null }, prompt.trim().ifBlank { null })) }, enabled = !busy && directory.trim().startsWith('/'), modifier = Modifier.fillMaxWidth().height(52.dp)) {
                if (busy) CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp) else Text("Create session")
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SessionChoice(label: String, value: String, options: List<String>, enabled: Boolean, emptyLabel: String = "Default", shortPaths: Boolean = false, onSelect: (String) -> Unit) {
    var expanded by remember { mutableStateOf(false) }
    fun display(text: String) = if (text.isEmpty()) emptyLabel else if (shortPaths) text.substringAfterLast('/') else text
    ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { if (enabled) expanded = it }) {
        OutlinedTextField(value = display(value), onValueChange = {}, readOnly = true, enabled = enabled, label = { Text(label) }, trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded) }, modifier = Modifier.menuAnchor(MenuAnchorType.PrimaryNotEditable, enabled).fillMaxWidth())
        ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            options.forEach { option -> DropdownMenuItem(text = { Text(display(option)) }, onClick = { onSelect(option); expanded = false }) }
        }
    }
}
