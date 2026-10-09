package li.rajeshgo.sm.ui.analytics

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import java.time.Duration
import java.time.OffsetDateTime
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import li.rajeshgo.sm.data.model.*
import li.rajeshgo.sm.ui.theme.*

fun LazyListScope.spendSection(
    state: SpendState,
    expanded: Boolean,
    onExpand: () -> Unit,
    onSelect: (String?, SpendRange) -> Unit,
    onRetry: () -> Unit,
    onOpen: (SpendNode) -> Unit,
    onLevel: (Int) -> Unit,
    onAgent: (SpendNode) -> Unit,
    onHistory: (String) -> Unit,
) {
    item { SpendControls(state, onSelect) }
    state.error?.let { error -> item { AnalyticsErrorBanner(error, onRetry) } }
    val report = state.report
    if (report == null) {
        if (state.loading) item { AnalyticsSkeleton() }
        return
    }
    if (report.provider == "local") {
        item { LocalSpendCard(report) }
        return
    }
    val nodes = state.nodes
    val node = nodes.last()
    if (nodes.size == 1) item { SpendHeader(report) }
    item { AnalyticsBreadcrumb(nodes.map { if (it.kind == "thread") Regex("^#[0-9]+").find(it.label)?.value ?: it.label else it.label }, spendPercent(node.percent), onLevel) }
    val thread = nodes.lastOrNull { it.kind == "thread" }
    if (node.kind == "agent") {
        item { SpendAgentCard(node, report.provider, onAgent) }
    }
    if (nodes.size > 1 && node.kind != "agent") item {
        Text(node.label, style = MaterialTheme.typography.headlineSmall, modifier = Modifier.padding(bottom = 8.dp))
    }
    thread?.historyPath?.let { path -> item {
        TextButton(onClick = { onHistory(path) }, contentPadding = PaddingValues(0.dp)) { Text("Ticket history  ↗", color = Cyan) }
    } }
    val rows = if (node.kind == "agent") thread?.children.orEmpty().filter { it.id != node.id } else node.children
    if (node.kind == "agent" && rows.isNotEmpty()) item { AnalyticsEyebrow("SAME TICKET") }
    if (rows.isEmpty() && node.kind != "agent") item {
        Text("No agent activity in this range", color = TextMuted, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(vertical = 32.dp))
    }
    analyticsDrillList(
        rows = rows.map { AnalyticsDrillRow(it.id, it.label, it.percent, spendPercent(it.percent), spendSubtitle(it), it.parts, it.kind != "gap") },
        legend = report.partsLegend, expanded = expanded, onExpand = onExpand,
        formatValue = ::spendPercent, color = ::spendColor,
        onOpen = { id -> rows.firstOrNull { it.id == id }?.let(onOpen) },
    )
    if (nodes.size == 1) {
        item {
            val sum = report.basis.values.sum()
            if (sum > 0) Text(
                listOf("claim" to "by claim", "parent" to "by parent", "name" to "by agent name", "none" to "no ticket")
                    .filter { (report.basis[it.first] ?: 0.0) > 0 }
                    .joinToString(" · ") { (key, label) -> "${kotlin.math.round(100 * (report.basis[key] ?: 0.0) / sum).toInt()}% $label" },
                style = MaterialTheme.typography.bodySmall, color = TextMuted, modifier = Modifier.padding(vertical = 18.dp),
            )
        }
        report.notes.forEach { note -> item { Text(note, style = MaterialTheme.typography.bodySmall, color = Amber, modifier = Modifier.padding(bottom = 10.dp)) } }
    }
}

@Composable
private fun SpendControls(state: SpendState, onSelect: (String?, SpendRange) -> Unit) {
    var menu by remember { mutableStateOf(false) }
    Row(Modifier.fillMaxWidth().padding(top = 12.dp, bottom = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        listOf("claude" to "Claude", "codex" to "Codex", "local" to "Local").forEach { (key, label) ->
            FilterChip(selected = state.provider == key, onClick = { onSelect(key, state.range) }, label = { Text(label) })
        }
        Spacer(Modifier.weight(1f))
        Box {
            TextButton(onClick = { menu = true }, contentPadding = PaddingValues(horizontal = 4.dp, vertical = 8.dp)) { Text("${state.range.label} ▾", color = TextSecondary) }
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                SpendRange.entries.forEach { range -> DropdownMenuItem(text = { Text(range.label) }, onClick = { menu = false; onSelect(state.provider, range) }) }
            }
        }
    }
}

@Composable
private fun SpendHeader(report: SpendReport) {
    Surface(color = Panel, shape = RoundedCornerShape(18.dp), modifier = Modifier.fillMaxWidth().padding(top = 2.dp)) {
        Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            AnalyticsEyebrow("${report.provider.uppercase()} QUOTA")
            Text(spendPercent(report.total.percent), style = MaterialTheme.typography.displaySmall, fontWeight = FontWeight.SemiBold)
            Text(if (report.range == "4w") "of a week" else "of the week used", color = TextSecondary, style = MaterialTheme.typography.bodyMedium)
            Text("${spendTokens(report.total.tokens)} tokens", color = TextMuted, style = MaterialTheme.typography.bodySmall)
            if (report.range != "4w") report.meters.forEach { meter ->
                if (report.meters.size > 1) Text("${meter.label ?: meter.accountKey}  ${spendPercent(meter.percent)}", color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                if (report.range == "week") Text(spendMeterLine(meter), color = TextSecondary, style = MaterialTheme.typography.bodySmall)
            }
            Spacer(Modifier.height(8.dp))
            AnalyticsCompositionBar(report.root.parts, report.partsLegend, color = ::spendColor)
            SpendLegend(report)
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun SpendLegend(report: SpendReport) {
    FlowRow(horizontalArrangement = Arrangement.spacedBy(14.dp), verticalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(top = 5.dp)) {
        orderedAnalyticsParts(report.root.parts, report.partsLegend).forEach { (key, value) ->
            Row(horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                Box(Modifier.padding(top = 5.dp).size(6.dp).background(spendColor(key), RoundedCornerShape(2.dp)))
                Text("${report.partsLegend.firstOrNull { it.key == key }?.label ?: key} ${spendPercent(value)}", style = MaterialTheme.typography.labelSmall, color = TextSecondary)
            }
        }
    }
}

fun spendMeterLine(meter: SpendMeter, now: OffsetDateTime = OffsetDateTime.now()): String {
    fun date(value: String, pattern: String): String = runCatching {
        OffsetDateTime.parse(value).atZoneSameInstant(ZoneId.systemDefault()).format(DateTimeFormatter.ofPattern(pattern))
    }.getOrDefault(value)
    val parts = mutableListOf("resets ${date(meter.resetsAt, "EEE H:mm")}")
    meter.pace?.let { pace ->
        when (pace.kind) {
            "runs_out" -> pace.at?.let { parts += "at this pace runs out ${date(it, "EEE h a")}" }
            "on_pace" -> pace.percent?.let { parts += "on pace for ${spendPercent(it)}" }
        }
    }
    val age = runCatching { Duration.between(OffsetDateTime.parse(meter.observedAt), now).toMinutes() }.getOrDefault(0)
    if (age > 10) parts += "meter from ${if (age >= 60) "${age / 60}h" else "${age}m"} ago"
    return parts.joinToString(" · ")
}

@Composable
private fun SpendAgentCard(node: SpendNode, provider: String, onAgent: (SpendNode) -> Unit) {
    Surface(color = Panel, shape = RoundedCornerShape(18.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(node.label, style = MaterialTheme.typography.titleLarge)
            Text("${spendPercent(node.percent)} of the ${provider.replaceFirstChar { it.titlecase() }} week", style = MaterialTheme.typography.titleMedium, color = TextSecondary)
            Text(listOfNotNull("${node.models.orEmpty().sumOf { it.turns }} turns", node.sessionStatus).joinToString(" · "), color = TextMuted, style = MaterialTheme.typography.bodySmall)
            node.models.orEmpty().forEach { model ->
                HorizontalDivider(color = Border, modifier = Modifier.padding(vertical = 8.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Column(Modifier.weight(1f)) {
                        Text(spendModelLabel(model.model), style = MaterialTheme.typography.titleSmall)
                        Text(listOfNotNull(model.effort, "${model.turns} ${if (model.model == "codex-cloud-review") "reviews" else "turns"}").joinToString(" · "), style = MaterialTheme.typography.bodySmall, color = TextMuted)
                    }
                    Column {
                        Text(spendPercent(model.percent), style = MaterialTheme.typography.titleSmall)
                        Text("${spendPercent(if (node.percent > 0) model.percent / node.percent * 100 else 0.0)} of agent", style = MaterialTheme.typography.labelSmall, color = TextMuted)
                    }
                }
                if (model.model != "codex-cloud-review") {
                    listOf("Output" to model.tokens.output, "Cache writes" to model.tokens.cacheWrite, "Cache reads" to model.tokens.cacheRead, "Fresh input" to model.tokens.input).forEach { (label, tokens) ->
                        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                            Text(label, color = TextSecondary, style = MaterialTheme.typography.bodySmall)
                            Text(spendTokens(tokens), style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
            }
            node.sessionId?.let { TextButton(onClick = { onAgent(node) }, contentPadding = PaddingValues(0.dp)) { Text("Open agent  ↗", color = Cyan) } }
        }
    }
}

@Composable
private fun LocalSpendCard(report: SpendReport) {
    Column(Modifier.fillMaxWidth().padding(vertical = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Local", style = MaterialTheme.typography.headlineSmall)
        Text("${spendTokens(report.total.tokens)} tokens · ${localBusyHours(report.local?.busyHours ?: 0.0)} model-busy hours")
        Text("Tokens by model", style = MaterialTheme.typography.titleMedium)
        report.local?.models.orEmpty().forEach { model ->
            Text(model.model, fontWeight = FontWeight.SemiBold)
            Text("${model.turns} turns · input ${spendTokens(model.tokens.input)} · output ${spendTokens(model.tokens.output)} · cache write ${spendTokens(model.tokens.cacheWrite)} · cache read ${spendTokens(model.tokens.cacheRead)}", color = TextSecondary)
        }
        if (report.local?.models.isNullOrEmpty()) Text("No local usage in this range.", color = TextMuted)
        Text("Model-busy hours per day (UTC)", style = MaterialTheme.typography.titleMedium)
        report.local?.days.orEmpty().forEach { day ->
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Text(day.date)
                Text("${localBusyHours(day.busyHours)} h")
            }
        }
        report.notes.forEach { Text(it, color = TextMuted, style = MaterialTheme.typography.bodySmall) }
    }
}
