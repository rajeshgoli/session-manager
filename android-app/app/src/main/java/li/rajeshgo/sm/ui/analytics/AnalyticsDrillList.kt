package li.rajeshgo.sm.ui.analytics

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import li.rajeshgo.sm.data.model.AnalyticsLegend
import li.rajeshgo.sm.ui.theme.*

/** Presentation shared by Spend and Time; values retain their own units. */
data class AnalyticsDrillRow(
    val id: String,
    val label: String,
    val value: Double,
    val valueLabel: String,
    val subtitle: String,
    val parts: Map<String, Double>,
    val tappable: Boolean = true,
)

fun spendColor(key: String): Color = when (key) {
    "opus", "sol" -> Cyan
    "fable" -> Violet
    "sonnet", "terra" -> Emerald
    "haiku", "luna" -> Amber
    "astra" -> Fuchsia
    "review" -> Rose
    else -> TextMuted
}

/** Segments follow the endpoint's legend, with unknown keys appended. */
fun orderedAnalyticsParts(parts: Map<String, Double>, legend: List<AnalyticsLegend>): List<Pair<String, Double>> =
    (legend.map { it.key } + parts.keys).distinct().mapNotNull { key -> parts[key]?.takeIf { it > 0 }?.let { key to it } }

@Composable
fun AnalyticsCompositionBar(
    parts: Map<String, Double>,
    legend: List<AnalyticsLegend>,
    fraction: Float = 1f,
    color: (String) -> Color,
) {
    val ordered = orderedAnalyticsParts(parts, legend)
    Canvas(Modifier.fillMaxWidth().height(6.dp)) {
        val total = ordered.sumOf { it.second }
        if (total <= 0) return@Canvas
        val width = size.width * fraction.coerceIn(0f, 1f)
        var x = 0f
        ordered.forEach { (key, value) ->
            val segment = (width * value / total).toFloat()
            drawRect(color(key), topLeft = Offset(x, 0f), size = Size(segment, size.height))
            x += segment
        }
    }
}

@Composable
fun AnalyticsBreadcrumb(labels: List<String>, value: String, onLevel: (Int) -> Unit) {
    // Keep the parent reachable even on a narrow phone; the full title follows below.
    val firstVisible = (labels.size - 2).coerceAtLeast(0)
    Row(Modifier.fillMaxWidth().padding(top = 20.dp, bottom = 12.dp), horizontalArrangement = Arrangement.spacedBy(7.dp)) {
        Text("All", color = Cyan, style = MaterialTheme.typography.labelMedium, modifier = Modifier.clickable { onLevel(0) })
        if (firstVisible > 1) Text("› …", color = TextMuted, style = MaterialTheme.typography.labelMedium)
        labels.drop(1).forEachIndexed { index, label ->
            if (index + 1 >= firstVisible) {
                Text("›", color = TextMuted, style = MaterialTheme.typography.labelMedium)
                Text(label, color = TextSecondary, style = MaterialTheme.typography.labelMedium, maxLines = 1, overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false).clickable { onLevel(index + 1) })
            }
        }
        Spacer(Modifier.weight(1f))
        Text(value, color = TextSecondary, style = MaterialTheme.typography.labelMedium)
    }
}

fun LazyListScope.analyticsDrillList(
    rows: List<AnalyticsDrillRow>,
    legend: List<AnalyticsLegend>,
    expanded: Boolean,
    onExpand: () -> Unit,
    formatValue: (Double) -> String,
    color: (String) -> Color,
    onOpen: (String) -> Unit,
) {
    val largest = rows.maxOfOrNull { it.value } ?: 0.0
    items(if (expanded) rows else rows.take(30), key = { "drill-${it.id}" }) { row ->
        Column(Modifier.fillMaxWidth().clickable(enabled = row.tappable) { onOpen(row.id) }.padding(vertical = 14.dp)) {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Text(row.label, style = MaterialTheme.typography.titleSmall, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                Text(row.valueLabel, style = MaterialTheme.typography.titleSmall)
            }
            Spacer(Modifier.height(9.dp))
            AnalyticsCompositionBar(row.parts, legend, if (largest > 0) (row.value / largest).toFloat() else 0f, color)
            Spacer(Modifier.height(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(row.subtitle, style = MaterialTheme.typography.bodySmall, color = TextMuted, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                if (row.tappable) Text("›", color = TextMuted, style = MaterialTheme.typography.bodySmall)
            }
        }
        HorizontalDivider(color = Border.copy(alpha = 0.45f))
    }
    if (!expanded && rows.size > 30) item {
        val rest = rows.drop(30)
        Text("${rest.size} more · ${formatValue(rest.sumOf { it.value })}", color = Cyan, style = MaterialTheme.typography.labelLarge,
            modifier = Modifier.fillMaxWidth().clickable(onClick = onExpand).padding(vertical = 18.dp))
    }
}

@Composable
fun AnalyticsEyebrow(text: String) { Text(text, style = MaterialTheme.typography.labelSmall, color = TextMuted, modifier = Modifier.padding(top = 4.dp, bottom = 2.dp)) }

@Composable
fun AnalyticsErrorBanner(error: String, onRetry: () -> Unit) {
    Surface(color = Rose.copy(alpha = 0.08f), shape = RoundedCornerShape(12.dp)) {
        Row(Modifier.fillMaxWidth().padding(start = 14.dp), horizontalArrangement = Arrangement.SpaceBetween) {
            Text(error, color = Rose, style = MaterialTheme.typography.bodySmall, modifier = Modifier.weight(1f).padding(vertical = 16.dp))
            TextButton(onClick = onRetry) { Text("Retry") }
        }
    }
}

@Composable
fun AnalyticsSkeleton() {
    Surface(color = Panel, shape = RoundedCornerShape(18.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Box(Modifier.fillMaxWidth(0.3f).height(12.dp).background(Border, RoundedCornerShape(4.dp)))
            Box(Modifier.fillMaxWidth(0.5f).height(42.dp).background(Border, RoundedCornerShape(6.dp)))
            Box(Modifier.fillMaxWidth(0.8f).height(12.dp).background(Border, RoundedCornerShape(4.dp)))
            LinearProgressIndicator(modifier = Modifier.fillMaxWidth(), color = Cyan, trackColor = Border)
        }
    }
}
