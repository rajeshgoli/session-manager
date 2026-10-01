package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import java.time.OffsetDateTime
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import kotlinx.serialization.json.*
import li.rajeshgo.sm.data.model.PutReviewPolicyRequest
import li.rajeshgo.sm.data.model.ReviewPoliciesResponse
import li.rajeshgo.sm.data.model.ReviewStatus
import li.rajeshgo.sm.data.model.StoredReviewPolicy
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.reviews.ReviewerEditor
import li.rajeshgo.sm.ui.reviews.fallbackText
import li.rajeshgo.sm.ui.reviews.reviewerLabel
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary

/** `11:06 pm` in the phone's zone, as the web writes times. */
fun reviewClock(timestamp: String?): String? = timestamp?.let {
    runCatching {
        OffsetDateTime.parse(it).atZoneSameInstant(ZoneId.systemDefault())
            .format(DateTimeFormatter.ofPattern("h:mm a", Locale.US)).lowercase(Locale.US)
    }.getOrNull()
}

/** A narrower policy's row label: `Repo · session-manager`, `Lane · fractal-algo-rust#1843`, `Ticket · far#1848`. */
fun narrowerPolicyLabel(policy: StoredReviewPolicy): String {
    val repo = policy.repo.substringAfter('/')
    return when (policy.scope) {
        "repo" -> "Repo · $repo"
        "lane" -> "Lane · $repo#${policy.number}"
        else -> "Ticket · $repo#${policy.number}"
    }
}

/** `58 · GitHub Codex 27 · Codex runs 29 · Claude runs 2 · none left unreviewed`. */
fun reviewDayLine(status: ReviewStatus): String {
    val c = status.last24h
    val total = c.githubCodex + c.codexRuns + c.claudeRuns + c.noReviewer
    val missed = if (c.noReviewer == 0) "none left unreviewed" else "${c.noReviewer} left unreviewed"
    return "$total · GitHub Codex ${c.githubCodex} · Codex runs ${c.codexRuns} · Claude runs ${c.claudeRuns} · $missed"
}

/**
 * The `reviews` and `queue_limits.review` patch for the edited limits. Throws
 * with a message naming the bad field, as the server's check would.
 */
internal fun reviewLimitsPatch(runs: String?, skip: String?): JsonObject = buildJsonObject {
    runs?.trim()?.let { draft ->
        val value = draft.toIntOrNull()
        require(value != null && value in 0..16) { "Review runs at once: enter a whole number from 0 to 16." }
        put("queue_limits", buildJsonObject { put("review", value) })
    }
    skip?.trim()?.let { draft ->
        val value = draft.toIntOrNull()
        require(value != null && value in 50..100) { "Skip a provider at: enter a percent from 50 to 100." }
        put("reviews", buildJsonObject { put("skip_meter_percent", value) })
    }
}

/** Settings › Reviews (sm#1768 Figure 7A), shared with the web Settings page. */
@Composable
fun ReviewsSection() {
    val context = LocalContext.current.applicationContext
    val settings = remember(context) { SettingsRepository(context) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    val url by settings.serverUrl.collectAsState(initial = "")
    val token by settings.accessToken.collectAsState(initial = "")
    val scope = rememberCoroutineScope()
    var status by remember(url, token) { mutableStateOf<ReviewStatus?>(null) }
    var policies by remember(url, token) { mutableStateOf<ReviewPoliciesResponse?>(null) }
    var owner by remember(url, token) { mutableStateOf<JsonObject?>(null) }
    var message by remember(url, token) { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var reload by remember { mutableIntStateOf(0) }
    var draftReviewer by remember(policies) { mutableStateOf(policies?.default?.reviewer) }
    var runsDraft by remember(owner) { mutableStateOf<String?>(null) }
    var skipDraft by remember(owner) { mutableStateOf<String?>(null) }

    LaunchedEffect(url, token, reload) {
        if (url.isBlank() || token.isBlank()) return@LaunchedEffect
        try {
            status = repository.fetchReviewStatus(url, token)
            policies = repository.fetchReviewPolicies(url, token)
            owner = repository.fetchOwnerSettings(url, token)
        } catch (error: CancellationException) {
            throw error
        } catch (error: Exception) {
            message = error.message ?: "Couldn't load review settings."
        }
    }
    fun write(done: String, block: suspend () -> Result<*>) {
        busy = true
        message = "Saving…"
        scope.launch {
            block().onSuccess { message = done; reload++ }.onFailure { message = it.message ?: "Couldn't save." }
            busy = false
        }
    }

    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        val current = status
        val stored = policies
        if (current == null || stored == null) {
            Text(message ?: "Loading…", style = MaterialTheme.typography.bodySmall)
            return@Column
        }
        val github = current.githubCodex
        if (github.state == "paused") {
            Column(
                Modifier.fillMaxWidth().background(Amber.copy(alpha = 0.12f), RoundedCornerShape(12.dp)).padding(12.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Text("Paused", color = Amber, style = MaterialTheme.typography.labelMedium, fontWeight = FontWeight.Bold)
                Text(
                    "GitHub Codex is out of code-review quota" + (reviewClock(github.pausedAt)?.let { " since $it" } ?: "") +
                        ". Reviews go to local runs.",
                    style = MaterialTheme.typography.bodyMedium,
                )
                reviewClock(github.nextCheckAt)?.let {
                    Text("The next check is after $it.", style = MaterialTheme.typography.bodySmall, color = TextSecondary)
                }
                TextButton(enabled = !busy, onClick = {
                    write("The next review request asks GitHub Codex again.") { repository.checkGithubCodex(url, token) }
                }) { Text("Try GitHub Codex now") }
            }
        }

        Text("Default reviewer", style = MaterialTheme.typography.titleSmall)
        Text("Every repo, lane and ticket without its own.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
        val draft = draftReviewer ?: stored.default.reviewer
        ReviewerEditor(draft, { draftReviewer = it; message = null }, allowPaired = false, enabled = !busy)
        if (draft != stored.default.reviewer) {
            Button(enabled = !busy, onClick = {
                write("Saved") { repository.putReviewPolicy(url, token, PutReviewPolicyRequest("default", null, null, draft)) }
            }) { Text("Save default") }
        }

        HorizontalDivider(color = Border)
        Text("Narrower", style = MaterialTheme.typography.titleSmall)
        if (stored.policies.isEmpty()) {
            Text("None. Set a lane's or a ticket's reviewer from its ⋮ menu on the Board.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
        }
        stored.policies.forEach { policy -> NarrowerRow(policy) }

        HorizontalDivider(color = Border)
        Text("Limits", style = MaterialTheme.typography.titleSmall)
        val savedRuns = owner?.get("queue_limits")?.jsonObject?.get("review")?.jsonPrimitive?.intOrNull
        val savedSkip = owner?.get("reviews")?.jsonObject?.get("skip_meter_percent")?.jsonPrimitive?.intOrNull
        OutlinedTextField(
            value = runsDraft ?: savedRuns?.toString().orEmpty(),
            onValueChange = { runsDraft = it; message = null },
            label = { Text("Review runs at once") },
            supportingText = { Text("Queue type \"review\"; one more waits for a slot.") },
            singleLine = true,
            enabled = !busy,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            modifier = Modifier.fillMaxWidth(),
        )
        val meters = listOf("codex" to "Codex", "claude" to "Claude").joinToString(" · ") { (key, name) ->
            "$name " + (current.meters[key]?.let { "${it.toInt()}%" } ?: "unknown")
        }
        OutlinedTextField(
            value = skipDraft ?: savedSkip?.toString().orEmpty(),
            onValueChange = { skipDraft = it; message = null },
            label = { Text("Skip a provider at (% of its weekly meter)") },
            supportingText = { Text("$meters now. 100 never skips.") },
            singleLine = true,
            enabled = !busy,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            modifier = Modifier.fillMaxWidth(),
        )
        if (runsDraft != null || skipDraft != null) {
            Button(enabled = !busy, onClick = {
                val patch = try {
                    reviewLimitsPatch(runsDraft, skipDraft)
                } catch (error: IllegalArgumentException) {
                    message = error.message
                    return@Button
                }
                write("Saved") { repository.setOwnerSettings(url, token, patch) }
            }) { Text("Save limits") }
        }
        Text("Last 24 hours", style = MaterialTheme.typography.titleSmall)
        Text(reviewDayLine(current), style = MaterialTheme.typography.bodySmall, color = TextSecondary)
        message?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = if (it.startsWith("Couldn't")) Rose else TextMuted) }
    }
}

@Composable
private fun NarrowerRow(policy: StoredReviewPolicy) {
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.Top) {
        Text(narrowerPolicyLabel(policy), Modifier.weight(0.9f), style = MaterialTheme.typography.bodyMedium)
        Column(Modifier.weight(1.1f)) {
            Text(reviewerLabel(policy.reviewer), style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.SemiBold)
            Text(
                (policy.setByName?.let { "set by $it · " } ?: "") + fallbackText(policy.fallback).replaceFirstChar { it.lowercase() },
                style = MaterialTheme.typography.bodySmall,
                color = TextMuted,
            )
        }
    }
}
