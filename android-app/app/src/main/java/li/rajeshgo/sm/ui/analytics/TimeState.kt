package li.rajeshgo.sm.ui.analytics

import li.rajeshgo.sm.data.model.AnalyticsLegend
import li.rajeshgo.sm.data.model.TimeNode
import li.rajeshgo.sm.data.model.TimeReport

enum class TimeRange(val key: String, val label: String) {
    DAY("24h", "24 h"), WEEK("7d", "7 days"), MONTH("30d", "30 days");
    companion object {
        fun fromKey(key: String) = entries.firstOrNull { it.key == key } ?: WEEK
    }
}

data class TimeState(
    val range: TimeRange = TimeRange.WEEK,
    val report: TimeReport? = null,
    val path: List<String> = emptyList(),
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    val error: String? = null,
) {
    val nodes: List<TimeNode> get() = report?.let { analyticsPath(it.root, path, TimeNode::id, TimeNode::children) }.orEmpty()
    val current: TimeNode? get() = nodes.lastOrNull()
    fun back() = copy(path = path.dropLast(1))
    fun open(node: TimeNode): TimeState {
        val chain = nodes
        val base = if (current?.kind == "agent") chain.dropLast(1) else chain
        if (base.lastOrNull()?.children?.none { it.id == node.id } != false) return this
        return copy(path = base.drop(1).map { it.id } + node.id)
    }
    fun received(report: TimeReport): TimeState {
        val valid = analyticsPath(report.root, path, TimeNode::id, TimeNode::children)
        return copy(report = report, path = valid.drop(1).map { it.id }, loading = false, refreshing = false, error = null)
    }
}

/** "7 h 12 m", "44 m", "30 s"; hours only from 100 h so the header stays short. */
fun timeDuration(seconds: Long): String {
    val minutes = (seconds + 30) / 60
    return when {
        seconds >= 100 * 3600 -> "${(seconds + 1800) / 3600} h"
        minutes >= 60 -> if (minutes % 60 == 0L) "${minutes / 60} h" else "${minutes / 60} h ${minutes % 60} m"
        seconds >= 60 -> "$minutes m"
        else -> "$seconds s"
    }
}

/** Share of `whole`; a non-zero part that rounds to nothing shows as "<1%" so it doesn't read as absent. */
fun timeShare(part: Long, whole: Long): String {
    val percent = if (whole > 0) Math.round(100.0 * part / whole) else 0L
    return if (percent == 0L && part > 0) "<1%" else "$percent%"
}

private fun plural(count: Int, one: String) = "$count ${if (count == 1) one else "${one}s"}"

fun timeSubtitle(node: TimeNode): String {
    val parked = node.parkedSeconds.takeIf { it > 0 }?.let { "${timeDuration(it)} parked" }
    return when (node.kind) {
        "repo" -> listOfNotNull(plural(node.children.size, "thread"), parked)
        "thread" -> listOfNotNull(plural(node.children.size, "agent"), node.state, parked)
        "agent" -> listOfNotNull(node.turns?.let { plural(it.toInt(), "turn") }, node.sessionStatus, parked)
        else -> listOfNotNull(parked)
    }.joinToString(" · ")
}

/** One line of the agent card: a bucket, or a tool kind indented under Tools. */
data class TimeBucketRow(val key: String, val label: String, val seconds: Long, val indented: Boolean)

/** Buckets largest first, with the tool kinds (largest first) directly under Tools. */
fun timeBucketRows(node: TimeNode, partsLegend: List<AnalyticsLegend>, toolLegend: List<AnalyticsLegend>): List<TimeBucketRow> {
    fun label(legend: List<AnalyticsLegend>, key: String) = legend.firstOrNull { it.key == key }?.label ?: key
    return node.parts.filterValues { it > 0 }.entries.sortedByDescending { it.value }.flatMap { (key, seconds) ->
        listOf(TimeBucketRow(key, label(partsLegend, key), seconds, false)) +
            if (key == "tools") node.tools.filterValues { it > 0 }.entries.sortedByDescending { it.value }
                .map { (tool, toolSeconds) -> TimeBucketRow(tool, label(toolLegend, tool), toolSeconds, true) }
            else emptyList()
    }
}
