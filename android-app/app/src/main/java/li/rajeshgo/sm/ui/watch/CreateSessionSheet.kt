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

fun supportsSessionCloning(provider: String?): Boolean = provider in listOf("claude", "codex", "codex-fork")

/**
 * Start from the board (sm#1665 appendix K): the sheet shows [label] instead
 * of the workspace picker, and opens on the board's name, brief and defaults.
 */
data class TicketStart(
    val label: String,
    val workingDir: String,
    val name: String,
    val brief: String,
    val provider: String,
    val model: String?,
    val effort: String,
)

/** The efforts the board's Start offers for [provider], as the web sheet does. */
private fun startEfforts(provider: String): List<String> =
    if (provider == "claude") listOf("low", "medium", "high", "max") else listOf("low", "medium", "high", "xhigh")

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
    loadModels: suspend (String, String) -> List<String>,
    busy: Boolean,
    error: String?,
    onDismiss: () -> Unit,
    ticket: TicketStart? = null,
    /** More rows above the error and button, such as the board Start's Reviewer row; gets whether input is enabled. */
    extra: (@Composable (Boolean) -> Unit)? = null,
    onCreate: (CreateSessionRequest) -> Unit,
) {
    val sheetKey = ticket?.label ?: source?.id
    val template = remember(sheetKey) {
        ticket?.let { CreateSessionRequest(it.provider, it.workingDir, it.model, it.effort) } ?: sessionTemplate(source)
    }
    var provider by rememberSaveable(sheetKey) { mutableStateOf(template.provider) }
    var model by rememberSaveable(sheetKey) { mutableStateOf(template.model.orEmpty()) }
    var effort by rememberSaveable(sheetKey) { mutableStateOf(template.reasoningEffort.orEmpty()) }
    var directory by rememberSaveable(sheetKey) { mutableStateOf(template.workingDir) }
    var customModel by rememberSaveable { mutableStateOf(false) }
    var name by rememberSaveable(sheetKey) { mutableStateOf(ticket?.name.orEmpty()) }
    var prompt by rememberSaveable(sheetKey) { mutableStateOf(ticket?.brief.orEmpty()) }
    // Start preselects the catalog's first model when the board's default is missing from it.
    var defaultUnavailable by rememberSaveable(sheetKey) { mutableStateOf(false) }
    var customDirectory by rememberSaveable { mutableStateOf(false) }
    val directories = (listOf("/Users/rajesh/projects/fractal-algo-rust", "/Users/rajesh/projects/session-manager", "/Users/rajesh/projects/codex-fork") + sessions.map { it.workingDir } + directory).distinct()
    var catalog by remember(provider, directory) { mutableStateOf(emptyList<String>()) }
    var modelsLoading by remember(provider, directory) { mutableStateOf(true) }
    var modelsError by remember(provider, directory) { mutableStateOf(false) }
    var catalogAttempt by remember { mutableStateOf(0) }
    LaunchedEffect(provider, directory, catalogAttempt) {
        modelsLoading = true
        modelsError = false
        try {
            kotlinx.coroutines.delay(300)
            catalog = loadModels(provider, directory.trim())
            if (ticket != null && catalog.isNotEmpty() && model !in catalog) {
                defaultUnavailable = model.isNotBlank()
                model = catalog.first()
            }
        }
        catch (error: kotlinx.coroutines.CancellationException) { throw error }
        catch (_: Exception) { modelsError = true }
        finally { modelsLoading = false }
    }
    val models = (if (ticket != null) catalog + listOf(model) else listOf("") + catalog + sessions.filter { it.provider == provider }.mapNotNull { it.model } + listOf(model)).distinct()
    ModalBottomSheet(sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true), onDismissRequest = { if (!busy) onDismiss() }) {
        Column(Modifier.fillMaxWidth().imePadding().verticalScroll(rememberScrollState()).padding(horizontal = 24.dp).padding(bottom = 24.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Text(
                when {
                    ticket != null -> "Start"
                    source == null -> "New session"
                    else -> "Clone ${sessionDisplayName(source)}"
                },
                style = MaterialTheme.typography.headlineSmall,
            )
            Text(
                when {
                    ticket != null -> ticket.label
                    source == null -> "Choose a workspace and start something new."
                    else -> "A fresh conversation with the same provider, model, effort and workspace."
                },
                style = MaterialTheme.typography.bodyMedium,
            )
            val providers = if (ticket != null) listOf("claude", "codex-fork") else listOf("claude", "codex")
            SessionChoice("Provider", provider, (providers + provider).distinct(), !busy) {
                provider = it; model = ""; effort = "high"; customModel = false; defaultUnavailable = false
            }
            SessionChoice("Model", model, models + "Other model…", !busy, emptyLabel = "Provider default") {
                if (it == "Other model…") customModel = true else { model = it; customModel = false }
            }
            if (modelsLoading) Text("Loading available models…", style = MaterialTheme.typography.bodySmall)
            if (defaultUnavailable && !modelsLoading) Text("The default model is unavailable; the first available model is selected.", style = MaterialTheme.typography.bodySmall)
            if (modelsError) TextButton(onClick = { catalogAttempt++ }) { Text("Couldn't load models · Retry") }
            if (customModel) OutlinedTextField(model, { model = it }, label = { Text("Model identifier") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
            val efforts = if (ticket != null) startEfforts(provider) else listOf("medium", "high")
            SessionChoice("Effort", effort, (efforts + effort).distinct(), !busy) { effort = it }
            if (ticket == null) {
                SessionChoice("Workspace", directory, directories + "Other directory…", !busy, shortPaths = true) {
                    if (it == "Other directory…") customDirectory = true else { directory = it; customDirectory = false }
                }
                if (customDirectory) OutlinedTextField(directory, { directory = it }, label = { Text("Absolute directory path") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
                else Text(directory, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            } else {
                Text(directory, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            OutlinedTextField(name, { name = it }, label = { Text(if (ticket != null) "Name" else "Name · optional") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
            OutlinedTextField(prompt, { prompt = it }, label = { Text(if (ticket != null) "Brief" else "First message · optional") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), minLines = 2, maxLines = if (ticket != null) 8 else 5)
            extra?.invoke(!busy)
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Button(onClick = { onCreate(CreateSessionRequest(provider, directory.trim(), model.ifBlank { null }, effort.ifBlank { null }, name.trim().ifBlank { null }, prompt.trim().ifBlank { null })) }, enabled = !busy && directory.trim().startsWith('/') && (ticket == null || (model.isNotBlank() && !modelsLoading)), modifier = Modifier.fillMaxWidth().height(52.dp)) {
                if (busy) CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp) else Text(if (ticket != null) "Start" else "Create session")
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
