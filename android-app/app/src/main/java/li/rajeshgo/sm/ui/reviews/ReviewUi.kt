package li.rajeshgo.sm.ui.reviews

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.data.model.ReviewPolicy
import li.rajeshgo.sm.data.model.Reviewer
import li.rajeshgo.sm.data.model.SessionModelsResponse
import kotlinx.coroutines.CancellationException
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary

/** Review runs use the fork's catalog, just as server validation does. */
internal fun reviewModelProvider(provider: String): String = if (provider == "codex") "codex-fork" else provider

/** Preserve a stored model even when it is absent from today's catalog. */
internal fun reviewModelOptions(catalog: List<String>, current: String?): List<String> =
    (catalog + listOfNotNull(current)).filter { it.isNotBlank() }.distinct()

val REVIEW_EFFORTS = mapOf(
    "codex" to listOf("medium", "high", "xhigh"),
    "claude" to listOf("low", "medium", "high", "xhigh", "max"),
)

/** What a switch to a run of [provider] starts on: the mid-tier model GitHub Codex falls back to. */
fun defaultRun(provider: String): Reviewer =
    if (provider == "claude") Reviewer("claude", model = "opus", effort = "high")
    else Reviewer("codex", model = "gpt-6-sol", effort = "medium")

private fun run(provider: String, model: String, effort: String) = Reviewer(provider, model = model, effort = effort)

/** A model's tier; Codex models name it last (`gpt-6.2-astra`). Mirrors the server's `review::tier`. */
fun reviewTier(model: String?): String = when {
    model == "fable" || model?.endsWith("-astra") == true -> "top"
    model == "sonnet" || model == "haiku" || model?.endsWith("-luna") == true -> "low"
    else -> "mid"
}

/** The fallback after [chosen], as the server computes it (appendix C2). */
fun reviewFallback(chosen: Reviewer): List<Reviewer> = when (chosen.kind) {
    "github_codex" -> listOf(run("codex", "gpt-6-sol", "medium"), run("claude", "opus", "high"))
    "codex" -> listOf(
        when (reviewTier(chosen.model)) {
            "top" -> run("claude", "fable", "xhigh")
            "low" -> run("claude", "sonnet", "high")
            else -> run("claude", "opus", "high")
        },
    )
    "claude" -> listOf(
        when (reviewTier(chosen.model)) {
            "top" -> run("codex", "gpt-6-astra", "high")
            "low" -> run("codex", "gpt-6-luna", "high")
            else -> run("codex", "gpt-6-sol", "medium")
        },
    )
    "paired" -> {
        val sameModel = run(chosen.provider ?: "codex", chosen.model.orEmpty(), chosen.effort.orEmpty())
        listOf(sameModel) + reviewFallback(sameModel)
    }
    else -> emptyList()
}

/** `GitHub Codex`, `Codex run · gpt-6-astra · high`, `Paired Codex · gpt-6-astra · high`. */
fun reviewerLabel(reviewer: Reviewer): String {
    fun provider(name: String?) = if (name == "claude") "Claude" else "Codex"
    return when (reviewer.kind) {
        "github_codex" -> "GitHub Codex"
        "paired" -> "Paired ${provider(reviewer.provider)} · ${reviewer.model} · ${reviewer.effort}"
        else -> "${provider(reviewer.kind)} run · ${reviewer.model} · ${reviewer.effort}"
    }
}

/** `If it can't review: Codex run · gpt-6-sol · medium, then Claude run · opus · high`. */
fun fallbackText(fallback: List<Reviewer>): String =
    if (fallback.isEmpty()) "No fallback" else "If it can't review: " + fallback.joinToString(", then ") { reviewerLabel(it) }

/**
 * Picks a reviewer: the kind, then provider (paired only), model and effort.
 * [allowPaired] is for a ticket, the only scope a paired reviewer may have.
 */
@Composable
fun ReviewerEditor(
    value: Reviewer,
    onChange: (Reviewer) -> Unit,
    allowPaired: Boolean,
    enabled: Boolean,
    loadModels: suspend (String, String) -> SessionModelsResponse,
) {
    val provider = when (value.kind) {
        "github_codex" -> null
        "paired" -> value.provider ?: "codex"
        else -> value.kind
    }
    var catalog by remember(provider) { mutableStateOf(emptyList<String>()) }
    var loading by remember(provider) { mutableStateOf(false) }
    var modelsError by remember(provider) { mutableStateOf(false) }
    var attempt by remember { mutableIntStateOf(0) }
    LaunchedEffect(provider, loadModels, attempt) {
        if (provider == null) return@LaunchedEffect
        loading = true
        modelsError = false
        try {
            catalog = loadModels(reviewModelProvider(provider), "").models
        } catch (error: CancellationException) {
            throw error
        } catch (_: Exception) {
            modelsError = true
        } finally {
            loading = false
        }
    }
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        val kinds = listOf("github_codex" to "GitHub", "codex" to "Codex", "claude" to "Claude") +
            if (allowPaired) listOf("paired" to "Paired") else emptyList()
        SegmentedChoice(kinds, value.kind, enabled) { kind ->
            if (kind == value.kind) return@SegmentedChoice
            onChange(
                when (kind) {
                    "github_codex" -> Reviewer()
                    "paired" -> defaultRun("codex").copy(kind = "paired", provider = "codex")
                    else -> defaultRun(kind)
                },
            )
        }
        if (value.kind == "github_codex") {
            Text("GitHub Codex chooses its own model.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
        } else {
            val provider = requireNotNull(provider)
            if (value.kind == "paired") {
                SegmentedChoice(listOf("codex" to "Codex agent", "claude" to "Claude agent"), provider, enabled) {
                    if (it != provider) onChange(defaultRun(it).copy(kind = "paired", provider = it))
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Dropdown("Model", value.model.orEmpty(), reviewModelOptions(catalog, value.model), enabled, Modifier.weight(1.4f)) {
                    onChange(value.copy(model = it))
                }
                val efforts = REVIEW_EFFORTS.getValue(provider)
                Dropdown("Effort", value.effort.orEmpty(), efforts, enabled, Modifier.weight(1f)) {
                    onChange(value.copy(effort = it))
                }
            }
            if (loading) Text("Loading models…", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            if (modelsError) {
                TextButton(onClick = { attempt++ }, enabled = enabled) { Text("Couldn't load models · Retry") }
            }
            if (value.kind == "paired") {
                Text(
                    "Starts at the ticket's first review request, in the author's checkout. It may build and run tests, never edit.",
                    style = MaterialTheme.typography.bodySmall,
                    color = TextMuted,
                )
            }
        }
        Text(fallbackText(reviewFallback(value)), style = MaterialTheme.typography.bodySmall, color = TextSecondary)
    }
}

@Composable
fun SegmentedChoice(options: List<Pair<String, String>>, selected: String, enabled: Boolean, onSelect: (String) -> Unit) {
    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
        options.forEachIndexed { index, (key, label) ->
            SegmentedButton(
                selected = key == selected,
                onClick = { onSelect(key) },
                enabled = enabled,
                shape = SegmentedButtonDefaults.itemShape(index, options.size),
                label = { Text(label, maxLines = 1) },
            )
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun Dropdown(label: String, value: String, options: List<String>, enabled: Boolean, modifier: Modifier, onSelect: (String) -> Unit) {
    var expanded by remember { mutableStateOf(false) }
    ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { if (enabled) expanded = it }, modifier = modifier) {
        OutlinedTextField(
            value = value,
            onValueChange = {},
            readOnly = true,
            enabled = enabled,
            singleLine = true,
            label = { Text(label) },
            trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded) },
            modifier = Modifier.menuAnchor(MenuAnchorType.PrimaryNotEditable, enabled).fillMaxWidth(),
        )
        ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            options.forEach { option -> DropdownMenuItem(text = { Text(option) }, onClick = { onSelect(option); expanded = false }) }
        }
    }
}

/**
 * The board Start sheet's Reviewer row (Figure 4b). [value] null keeps the
 * lane's policy, which [laneDefault] describes; a reviewer is stored as the
 * ticket's own policy when the author starts.
 */
@Composable
fun StartReviewerRow(
    laneDefault: li.rajeshgo.sm.data.model.StartReviewPolicy?,
    value: Reviewer?,
    onChange: (Reviewer?) -> Unit,
    enabled: Boolean,
    loadModels: suspend (String, String) -> SessionModelsResponse,
) {
    var expanded by remember { mutableStateOf(false) }
    if (!expanded) {
        Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
            Text("Reviewer · ${value?.let(::reviewerLabel) ?: laneDefault?.resolved?.let(::reviewerLabel) ?: "Default policy"}", modifier = Modifier.weight(1f), style = MaterialTheme.typography.bodySmall, maxLines = 1, overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis)
            TextButton(onClick = { expanded = true }, enabled = enabled) { Text("Change") }
        }
        return
    }
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Text("Reviewer for this ticket", style = MaterialTheme.typography.titleSmall)
        SegmentedChoice(listOf("inherit" to "Lane default", "own" to "This ticket"), if (value == null) "inherit" else "own", enabled) {
            if (it == "inherit") onChange(null) else if (value == null) onChange(defaultRun("codex"))
        }
        if (value == null) {
            Text(
                laneDefault?.let { "${reviewerLabel(it.resolved)} · from the ${it.source}" } ?: "The lane's, the repo's or the default reviewer.",
                style = MaterialTheme.typography.bodySmall,
                color = TextSecondary,
            )
        } else {
            ReviewerEditor(value, { onChange(it) }, allowPaired = true, enabled = enabled, loadModels = loadModels)
        }
    }
}

/**
 * A lane's or a ticket's review policy (Figure 7B): use the wider policy, or
 * set this one. [onSave] gets null to clear the stored policy.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ReviewPolicySheet(
    title: String,
    inheritLabel: String,
    ownLabel: String,
    current: ReviewPolicy?,
    allowPaired: Boolean,
    busy: Boolean,
    error: String?,
    onDismiss: () -> Unit,
    onSave: (Reviewer?) -> Unit,
    loadModels: suspend (String, String) -> SessionModelsResponse,
) {
    var own by remember(current) { mutableStateOf(current != null) }
    var reviewer by remember(current) { mutableStateOf(current?.reviewer ?: defaultRun("codex")) }
    ModalBottomSheet(onDismissRequest = { if (!busy) onDismiss() }, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(
            Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 24.dp).padding(bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text(title, style = MaterialTheme.typography.titleLarge)
            SegmentedChoice(listOf("inherit" to inheritLabel, "own" to ownLabel), if (own) "own" else "inherit", !busy) { own = it == "own" }
            if (own) {
                ReviewerEditor(reviewer, { reviewer = it }, allowPaired, !busy, loadModels)
            } else {
                Text("Uses the wider policy: the lane's, the repo's, or the default.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            }
            current?.setByName?.let { Text("Set by $it", style = MaterialTheme.typography.bodySmall, color = TextMuted) }
            Text("Changes apply to the next review request.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            error?.let { Text(it, color = Rose, style = MaterialTheme.typography.bodySmall) }
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp, androidx.compose.ui.Alignment.End)) {
                TextButton(onClick = onDismiss, enabled = !busy) { Text("Cancel") }
                Button(onClick = { onSave(if (own) reviewer else null) }, enabled = !busy && (own || current != null)) {
                    Text(if (busy) "Saving…" else "Save")
                }
            }
        }
    }
}
