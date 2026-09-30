package li.rajeshgo.sm.ui.analytics

import java.util.Locale
import li.rajeshgo.sm.data.model.SpendNode
import li.rajeshgo.sm.data.model.SpendReport

/** IDs form a path, since the same agent can appear in several threads. */
fun <T> analyticsPath(root: T, path: List<String>, id: (T) -> String, children: (T) -> List<T>): List<T> {
    val result = mutableListOf(root)
    for (key in path) {
        val child = children(result.last()).firstOrNull { id(it) == key } ?: break
        result += child
    }
    return result
}

enum class SpendRange(val key: String, val label: String) {
    WEEK("week", "This week"), LAST_WEEK("last_week", "Last week"), FOUR_WEEKS("4w", "4 weeks");
    companion object {
        fun fromKey(key: String) = entries.firstOrNull { it.key == key } ?: WEEK
    }
}

data class SpendState(
    val provider: String? = null,
    val range: SpendRange = SpendRange.WEEK,
    val report: SpendReport? = null,
    val path: List<String> = emptyList(),
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    val error: String? = null,
) {
    val nodes: List<SpendNode> get() = report?.let { analyticsPath(it.root, path, SpendNode::id, SpendNode::children) }.orEmpty()
    val current: SpendNode? get() = nodes.lastOrNull()
    fun back() = copy(path = path.dropLast(1))
    fun open(node: SpendNode): SpendState {
        if (node.kind == "gap") return this
        val chain = nodes
        val base = if (current?.kind == "agent") chain.dropLast(1) else chain
        if (base.lastOrNull()?.children?.none { it.id == node.id } != false) return this
        return copy(path = base.drop(1).map { it.id } + node.id)
    }
    fun received(report: SpendReport): SpendState {
        val valid = analyticsPath(report.root, path, SpendNode::id, SpendNode::children)
        return copy(provider = report.provider, report = report, path = valid.drop(1).map { it.id }, loading = false, refreshing = false, error = null)
    }
}

fun spendPercent(value: Double): String = String.format(Locale.US, "%.1f%%", value)
fun spendTokens(value: Long): String = when {
    value >= 1_000_000_000 -> String.format(Locale.US, "%.2fB", value / 1e9)
    value >= 1_000_000 -> String.format(Locale.US, "%.1fM", value / 1e6)
    value >= 1_000 -> String.format(Locale.US, "%.1fK", value / 1e3)
    else -> value.toString()
}
fun spendModelLabel(model: String): String {
    if (model == "codex-cloud-review") return "Cloud reviews"
    val family = listOf("fable", "opus", "sonnet", "haiku", "astra", "sol", "terra", "luna").firstOrNull { model.lowercase().contains(it) }
    if (family != null) {
        val version = if (model.startsWith("claude-")) model.substringAfter("$family-", "").replace('-', '.') else ""
        return listOf(family.replaceFirstChar { it.titlecase() }, version).filter { it.isNotBlank() }.joinToString(" ")
    }
    return model
}
fun spendSubtitle(node: SpendNode): String = when (node.kind) {
    "repo" -> "${spendTokens(node.tokens)} tokens · ${node.children.size} ${if (node.children.size == 1) "thread" else "threads"}"
    "thread" -> listOfNotNull("${node.children.size} ${if (node.children.size == 1) "agent" else "agents"}", node.state).joinToString(" · ")
    "agent" -> listOfNotNull(node.models?.maxByOrNull { it.percent }?.let { spendModelLabel(it.model) }, "${node.models.orEmpty().sumOf { it.turns }} turns").joinToString(" · ")
    "gap" -> "Usage outside the recorded ledger"
    else -> "${spendTokens(node.tokens)} tokens"
}
