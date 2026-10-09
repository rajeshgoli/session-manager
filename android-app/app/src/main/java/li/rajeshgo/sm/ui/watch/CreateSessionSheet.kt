package li.rajeshgo.sm.ui.watch

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.CreateSessionRequest
import li.rajeshgo.sm.data.model.BugReportIssue
import li.rajeshgo.sm.ui.bug.BugAgentChoice
import li.rajeshgo.sm.ui.bug.BugFiling
import li.rajeshgo.sm.ui.theme.Border
import kotlinx.serialization.json.*

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
    val whenReady: Boolean = false,
    val agentTypes: List<AgentTypeChoice> = emptyList(),
    val selectedType: String? = null,
)

data class AgentTypeChoice(val name: String, val provider: String, val model: String, val effort: String)

fun agentTypeChoices(settings: JsonObject): List<AgentTypeChoice> =
    (settings["new_agent"] as? JsonObject)?.get("agent_types")?.let { it as? JsonArray }.orEmpty().mapNotNull { entry ->
        val item = entry as? JsonObject ?: return@mapNotNull null
        val name = (item["name"] as? JsonPrimitive)?.contentOrNull ?: return@mapNotNull null
        AgentTypeChoice(name, (item["provider"] as? JsonPrimitive)?.contentOrNull ?: "claude",
            (item["model"] as? JsonPrimitive)?.contentOrNull.orEmpty(), (item["effort"] as? JsonPrimitive)?.contentOrNull.orEmpty())
    }

fun matchAgentType(types: List<AgentTypeChoice>, provider: String, model: String?, effort: String?, preferredName: String? = null): AgentTypeChoice? {
    val matches = types.filter { it.provider == provider && it.model == model.orEmpty() && it.effort == effort.orEmpty() }
    return matches.firstOrNull { it.name == preferredName } ?: matches.firstOrNull()
}

/**
 * Report a bug (spec 1859 C4): the sheet takes the bug's text and
 * screenshot, and with "Start an agent" on shows Start's agent choices.
 */
data class BugStart(
    /** Null while the agent choices load; the text is usable before. */
    val defaults: CreateSessionRequest?,
    /** Why the agent choices are unavailable, such as no checkout of the filing repo. */
    val agentNote: String?,
    /** Null when the capture failed. */
    val screenshot: ImageBitmap?,
    /** Set once the bug is filed but its agent did not start: the button retries only the start. */
    val filedIssue: BugReportIssue? = null,
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
    loadModels: suspend (String, String) -> li.rajeshgo.sm.data.model.SessionModelsResponse,
    loadAgentTypes: suspend () -> List<AgentTypeChoice>,
    busy: Boolean,
    error: String?,
    onDismiss: () -> Unit,
    ticket: TicketStart? = null,
    /** More rows above the error and button, such as the board Start's Reviewer row; gets whether input is enabled. */
    extra: (@Composable (Boolean) -> Unit)? = null,
    bug: BugStart? = null,
    onFile: (BugFiling) -> Unit = {},
    bugDraft: String = "",
    onBugDraftChange: (String) -> Unit = {},
    onClearBugDraft: () -> Unit = {},
    onAgentTypeChange: (String?) -> Unit = {},
    onCreate: (CreateSessionRequest) -> Unit,
) {
    // The agent fields open on the bug defaults once they arrive.
    val sheetKey = ticket?.label ?: bug?.let { "bug:${it.defaults != null}" } ?: source?.id
    val template = remember(sheetKey) {
        ticket?.let { CreateSessionRequest(it.provider, it.workingDir, it.model, it.effort) } ?: bug?.defaults ?: sessionTemplate(source)
    }
    // Board Start and the bug's agent take the same choices.
    val startLike = ticket != null || bug != null
    val bugText = bugDraft
    var bugScreenshot by rememberSaveable { mutableStateOf(bug?.screenshot != null) }
    var bugAgent by rememberSaveable { mutableStateOf(false) }
    val bugLocked = bug?.filedIssue != null
    var provider by rememberSaveable(sheetKey) { mutableStateOf(template.provider) }
    var model by rememberSaveable(sheetKey) { mutableStateOf(template.model.orEmpty()) }
    var effort by rememberSaveable(sheetKey) { mutableStateOf(template.reasoningEffort.orEmpty()) }
    var otherType by rememberSaveable(sheetKey) { mutableStateOf(false) }
    var selectedTypeName by rememberSaveable(sheetKey) { mutableStateOf(ticket?.selectedType) }
    var typesInitialized by rememberSaveable(sheetKey) { mutableStateOf(false) }
    var agentTypes by remember { mutableStateOf(ticket?.agentTypes.orEmpty()) }
    var typesLoading by remember { mutableStateOf(true) }
    var typesError by remember { mutableStateOf(false) }
    var typesAttempt by remember { mutableStateOf(0) }
    LaunchedEffect(typesAttempt) {
        typesLoading = true
        typesError = false
        try { agentTypes = loadAgentTypes() }
        catch (error: kotlinx.coroutines.CancellationException) { throw error }
        catch (_: Exception) { typesError = true }
        finally { typesLoading = false }
    }
    LaunchedEffect(sheetKey, typesLoading) {
        if (!typesLoading && !typesInitialized) {
            selectedTypeName = matchAgentType(agentTypes, provider, model, effort, selectedTypeName)?.name
            otherType = selectedTypeName == null
            typesInitialized = true
        }
    }
    val selectedType = if (otherType) null else matchAgentType(agentTypes, provider, model, effort, selectedTypeName)
    LaunchedEffect(selectedType?.name, typesInitialized) {
        if (typesInitialized) onAgentTypeChange(selectedType?.name)
    }
    val customConfig = !typesLoading && selectedType == null
    var directory by rememberSaveable(sheetKey) { mutableStateOf(template.workingDir) }
    var customModel by rememberSaveable { mutableStateOf(false) }
    var name by rememberSaveable(sheetKey) { mutableStateOf(ticket?.name.orEmpty()) }
    var prompt by rememberSaveable(sheetKey) { mutableStateOf(ticket?.brief.orEmpty()) }
    var customDirectory by rememberSaveable { mutableStateOf(false) }
    val directories = (listOf("/Users/rajesh/projects/fractal-algo-rust", "/Users/rajesh/projects/session-manager", "/Users/rajesh/projects/codex-fork") + sessions.map { it.workingDir } + directory).distinct()
    var catalog by remember(provider, directory) { mutableStateOf(emptyList<String>()) }
    var modelsLoading by remember(provider, directory) { mutableStateOf(true) }
    var modelsError by remember(provider, directory) { mutableStateOf(false) }
    var catalogAttempt by remember { mutableStateOf(0) }
    LaunchedEffect(provider, directory, catalogAttempt, bugAgent, customConfig) {
        if (!customConfig || (bug != null && (!bugAgent || directory.isBlank()))) {
            modelsLoading = false
            return@LaunchedEffect
        }
        modelsLoading = true
        modelsError = false
        try {
            kotlinx.coroutines.delay(300)
            catalog = loadModels(provider, directory.trim()).models
        }
        catch (error: kotlinx.coroutines.CancellationException) { throw error }
        catch (_: Exception) { modelsError = true }
        finally { modelsLoading = false }
    }
    var local by remember { mutableStateOf<li.rajeshgo.sm.data.model.SessionModelsResponse?>(null) }
    LaunchedEffect(Unit) {
        while (true) {
            try { local = loadModels("opencode", "") }
            catch (error: kotlinx.coroutines.CancellationException) { throw error }
            catch (_: Exception) { local = null }
            kotlinx.coroutines.delay(5000)
        }
    }
    val localBlocked = provider == "opencode" && local?.canStartLocal != true
    val models = (if (startLike) catalog + listOf(model) else listOf("") + catalog + sessions.filter { it.provider == provider }.mapNotNull { it.model } + listOf(model)).distinct()
    ModalBottomSheet(sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true), onDismissRequest = { if (!busy) onDismiss() }) {
        Column(Modifier.fillMaxWidth().imePadding().verticalScroll(rememberScrollState()).padding(horizontal = 24.dp).padding(bottom = 24.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Text(
                when {
                    bug != null -> "Report a bug"
                    ticket?.whenReady == true -> "Start when ready"
                    ticket != null -> "Start"
                    source == null -> "New session"
                    else -> "Clone ${sessionDisplayName(source)}"
                },
                style = MaterialTheme.typography.headlineSmall,
            )
            if (bug == null) {
                Text(
                    when {
                        ticket != null -> ticket.label
                        source == null -> "Choose a workspace and start something new."
                        else -> "A fresh conversation with the same provider, model, effort and workspace."
                    },
                    style = MaterialTheme.typography.bodyMedium,
                )
            } else {
                BugFields(bug, bugText, onBugDraftChange, bugScreenshot, { bugScreenshot = it }, bugAgent, { bugAgent = it }, !busy && !bugLocked, !busy && !bugLocked && bug.defaults != null && bug.agentNote == null)
            }
            if (bug != null && !bugLocked) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("Draft saved on this device", style = MaterialTheme.typography.bodySmall, modifier = Modifier.weight(1f))
                    TextButton(onClick = onClearBugDraft, enabled = !busy) { Text("Clear draft") }
                }
            }
            if (bug == null || bugAgent) {
                Text("Agent type", style = MaterialTheme.typography.titleSmall)
                agentTypes.forEach { choice ->
                    FilterChip(
                        selected = selectedType == choice,
                        onClick = {
                            selectedTypeName = choice.name; otherType = false; provider = choice.provider; model = choice.model; effort = choice.effort
                            customModel = false
                        },
                        enabled = !busy && !typesLoading,
                        label = { Column {
                            Text(choice.name)
                            Text("${choice.provider} · ${choice.model.ifBlank { "Provider default" }} · ${choice.effort.ifBlank { "Default effort" }}", style = MaterialTheme.typography.bodySmall)
                        } },
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
                if (ticket?.whenReady != true && bug == null) local?.models?.firstOrNull()?.let { loaded ->
                    FilterChip(selected = provider == "opencode", onClick = {
                        otherType = true; provider = "opencode"; model = loaded; effort = ""; customModel = false
                    }, enabled = !busy && local?.canStartLocal == true, label = { Text("Local ($loaded)") })
                    local?.reason?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
                }
                FilterChip(selected = customConfig, onClick = { otherType = true }, enabled = !busy && !typesLoading, label = { Text("Other") })
                if (typesLoading) Text("Loading agent types…", style = MaterialTheme.typography.bodySmall)
                if (typesError) TextButton(onClick = { typesAttempt++ }, enabled = !busy) { Text("Couldn't load agent types · Retry") }
                if (customConfig) {
                    val providers = if (startLike) listOf("claude", "codex-fork") else listOf("claude", "codex")
                    SessionChoice("Provider", provider, (providers + listOf(provider).filter { it != "opencode" }).distinct(), !busy) {
                        provider = it; model = ""; effort = "high"; customModel = false
                    }
                    if (provider != "opencode") {
                        SessionChoice("Model", model, models + "Other model…", !busy, emptyLabel = "Provider default") {
                            if (it == "Other model…") customModel = true else { model = it; customModel = false }
                        }
                        if (modelsLoading) Text("Loading available models…", style = MaterialTheme.typography.bodySmall)
                        if (modelsError) TextButton(onClick = { catalogAttempt++ }) { Text("Couldn't load models · Retry") }
                        if (customModel) OutlinedTextField(model, { model = it }, label = { Text("Model identifier") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
                        val efforts = if (startLike) startEfforts(provider) else listOf("medium", "high")
                        SessionChoice("Effort", effort, (efforts + effort).distinct(), !busy) { effort = it }
                    }
                }
                if (!startLike) {
                    SessionChoice("Workspace", directory, directories + "Other directory…", !busy, shortPaths = true) {
                        if (it == "Other directory…") customDirectory = true else { directory = it; customDirectory = false }
                    }
                    if (customDirectory) OutlinedTextField(directory, { directory = it }, label = { Text("Absolute directory path") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
                    else Text(directory, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                } else {
                    Text(directory, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                if (bug != null) {
                    Text("Named from the new ticket, as Start does", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                } else {
                    OutlinedTextField(name, { name = it }, label = { Text(if (ticket != null) "Name" else "Name · optional") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), singleLine = true)
                    OutlinedTextField(prompt, { prompt = it }, label = { Text(if (ticket != null) "Brief" else "First message · optional") }, enabled = !busy, modifier = Modifier.fillMaxWidth(), minLines = 2, maxLines = if (ticket != null) 8 else 5)
                }
                extra?.invoke(!busy)
            }
            bug?.filedIssue?.let { issue ->
                val uriHandler = LocalUriHandler.current
                Text(
                    "Filed #${issue.number} ${issue.title}",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.clickable { uriHandler.openUri(issue.url) },
                )
            }
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            val agentReady = directory.trim().startsWith('/') && !typesLoading && !localBlocked
            val enabled = !busy && if (bug != null) {
                bugText.isNotBlank() && (!bugAgent || (bug.defaults != null && bug.agentNote == null && agentReady))
            } else agentReady
            Button(
                onClick = {
                    if (bug != null) {
                        onFile(BugFiling(bugText, bugScreenshot, if (bugAgent) BugAgentChoice(provider, model.ifBlank { null }, effort.ifBlank { null }, null) else null))
                    } else {
                        onCreate(CreateSessionRequest(provider, directory.trim(), model.ifBlank { null }, effort.ifBlank { null }, name.trim().ifBlank { null }, prompt.trim().ifBlank { null }))
                    }
                },
                enabled = enabled,
                modifier = Modifier.fillMaxWidth().height(52.dp),
            ) {
                val label = when {
                    bug == null -> if (ticket?.whenReady == true) "Start when ready" else if (ticket != null) "Start" else "Create session"
                    bugLocked -> "Start"
                    bugAgent -> "File and start"
                    else -> "File bug"
                }
                if (busy) CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp) else Text(label)
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

/** The bug's own rows (spec 1859 C4): the text, the screenshot and the "Start an agent" switch. */
@Composable
private fun BugFields(
    bug: BugStart,
    text: String,
    onText: (String) -> Unit,
    screenshot: Boolean,
    onScreenshot: (Boolean) -> Unit,
    agent: Boolean,
    onAgent: (Boolean) -> Unit,
    enabled: Boolean,
    agentEnabled: Boolean,
) {
    OutlinedTextField(
        text,
        onText,
        label = { Text("What's wrong") },
        supportingText = { Text("Public: goes into a GitHub issue. The first line is the title.") },
        enabled = enabled,
        modifier = Modifier.fillMaxWidth(),
        minLines = 4,
        maxLines = 8,
    )
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        bug.screenshot?.let { image ->
            Image(
                image,
                contentDescription = "Screenshot",
                contentScale = ContentScale.Fit,
                modifier = Modifier.height(96.dp).widthIn(max = 72.dp).border(BorderStroke(1.dp, Border), RoundedCornerShape(6.dp)),
            )
        }
        Column(Modifier.weight(1f)) {
            Text("Screenshot", style = MaterialTheme.typography.titleSmall)
            Text(
                if (bug.screenshot == null) "Screenshot unavailable" else "Private on sm, with page data and server facts",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Switch(checked = screenshot && bug.screenshot != null, onCheckedChange = onScreenshot, enabled = enabled && bug.screenshot != null)
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text("Start an agent", style = MaterialTheme.typography.titleSmall)
            val note = bug.agentNote ?: if (bug.defaults == null) "Loading agent choices…" else null
            note?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        }
        Switch(checked = agent, onCheckedChange = onAgent, enabled = agentEnabled)
    }
}
