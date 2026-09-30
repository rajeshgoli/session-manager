package li.rajeshgo.sm.ui.analytics

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.data.model.*
import li.rajeshgo.sm.ui.theme.*

fun timeColor(key: String): Color = when (key) {
    "model" -> Cyan
    "tools" -> Violet
    "queue" -> Amber
    "review" -> Fuchsia
    "you" -> Rose
    "agents" -> Emerald
    "idle" -> BorderStrong
    else -> TextMuted
}

private fun Map<String, Long>.asDoubles() = mapValues { it.value.toDouble() }

fun LazyListScope.timeSection(
    state: TimeState,
    expanded: Boolean,
    onExpand: () -> Unit,
    onSelect: (TimeRange) -> Unit,
    onRetry: () -> Unit,
    onOpen: (TimeNode) -> Unit,
    onLevel: (Int) -> Unit,
    onAgent: (TimeNode) -> Unit,
    onHistory: (String) -> Unit,
) {
    item {
        Row(Modifier.fillMaxWidth().padding(top = 12.dp, bottom = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            TimeRange.entries.forEach { range ->
                FilterChip(selected = state.range == range, onClick = { onSelect(range) }, label = { Text(range.label) })
            }
        }
    }
    state.error?.let { error -> item { AnalyticsErrorBanner(error, onRetry) } }
    val report = state.report
    if (report == null) {
        if (state.loading) item { AnalyticsSkeleton() }
        return
    }
    val nodes = state.nodes
    val node = nodes.last()
    if (nodes.size == 1) item { TimeHeader(report) }
    item { AnalyticsBreadcrumb(nodes.map { if (it.kind == "thread") Regex("^#[0-9]+").find(it.label)?.value ?: it.label else it.label }, timeDuration(node.activeSeconds), onLevel) }
    val thread = nodes.lastOrNull { it.kind == "thread" }
    if (node.kind == "agent") item { TimeAgentCard(node, report, onAgent) }
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
        rows = rows.map { AnalyticsDrillRow(it.id, it.label, it.activeSeconds.toDouble(), timeDuration(it.activeSeconds), timeSubtitle(it), it.parts.asDoubles()) },
        legend = report.partsLegend, expanded = expanded, onExpand = onExpand,
        formatValue = { timeDuration(it.toLong()) }, color = ::timeColor,
        onOpen = { id -> rows.firstOrNull { it.id == id }?.let(onOpen) },
    )
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun TimeHeader(report: TimeReport) {
    val total = report.total
    Surface(color = Panel, shape = RoundedCornerShape(18.dp), modifier = Modifier.fillMaxWidth().padding(top = 2.dp)) {
        Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            AnalyticsEyebrow("AGENT TIME")
            Text(timeDuration(total.activeSeconds), style = MaterialTheme.typography.displaySmall, fontWeight = FontWeight.SemiBold)
            Text("active agent time", color = TextSecondary, style = MaterialTheme.typography.bodyMedium)
            Text(
                listOfNotNull("${total.agents} ${if (total.agents == 1) "agent" else "agents"}", total.parkedSeconds.takeIf { it > 0 }?.let { "plus ${timeDuration(it)} parked" }).joinToString(" · "),
                color = TextMuted, style = MaterialTheme.typography.bodySmall,
            )
            Spacer(Modifier.height(8.dp))
            AnalyticsCompositionBar(report.root.parts.asDoubles(), report.partsLegend, color = ::timeColor)
            FlowRow(horizontalArrangement = Arrangement.spacedBy(14.dp), verticalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(top = 5.dp)) {
                orderedAnalyticsParts(report.root.parts.asDoubles(), report.partsLegend).forEach { (key, value) ->
                    Row(horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                        Box(Modifier.padding(top = 5.dp).size(6.dp).background(timeColor(key), RoundedCornerShape(2.dp)))
                        Text("${report.partsLegend.firstOrNull { it.key == key }?.label ?: key} ${timeShare(value.toLong(), report.root.activeSeconds)}", style = MaterialTheme.typography.labelSmall, color = TextSecondary)
                    }
                }
            }
        }
    }
}

@Composable
private fun TimeAgentCard(node: TimeNode, report: TimeReport, onAgent: (TimeNode) -> Unit) {
    Surface(color = Panel, shape = RoundedCornerShape(18.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(node.label, style = MaterialTheme.typography.titleLarge)
            Text("${timeDuration(node.activeSeconds)} active", style = MaterialTheme.typography.titleMedium, color = TextSecondary)
            Text(
                listOfNotNull(node.turns?.let { "$it ${if (it == 1L) "turn" else "turns"}" }, node.sessionStatus, node.parkedSeconds.takeIf { it > 0 }?.let { "then parked ${timeDuration(it)}" }).joinToString(" · "),
                color = TextMuted, style = MaterialTheme.typography.bodySmall,
            )
            HorizontalDivider(color = Border, modifier = Modifier.padding(vertical = 4.dp))
            timeBucketRows(node, report.partsLegend, report.toolLegend).forEach { row ->
                Row(Modifier.fillMaxWidth().padding(start = if (row.indented) 22.dp else 0.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    if (!row.indented) Box(Modifier.padding(top = 6.dp).size(8.dp).background(timeColor(row.key), RoundedCornerShape(2.dp)))
                    val style = if (row.indented) MaterialTheme.typography.bodySmall else MaterialTheme.typography.bodyMedium
                    val color = if (row.indented) TextMuted else MaterialTheme.colorScheme.onBackground
                    Text(row.label, style = style, color = color, modifier = Modifier.weight(1f))
                    Text(timeDuration(row.seconds), style = style, color = color)
                    Text(if (row.indented) "" else timeShare(row.seconds, node.activeSeconds), style = style, color = TextMuted, modifier = Modifier.width(40.dp))
                }
            }
            node.sessionId?.let { TextButton(onClick = { onAgent(node) }, contentPadding = PaddingValues(0.dp)) { Text("Open agent  ↗", color = Cyan) } }
        }
    }
}
