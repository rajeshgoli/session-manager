package li.rajeshgo.sm.ui.queue

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ArrowBack
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.ZoneId
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import li.rajeshgo.sm.data.model.UtilizationSeries
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.PanelElevated
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet

private val TYPE_COLORS = listOf(
    "tests" to Cyan,
    "perf" to Amber,
    "background" to Violet,
    "service" to TextMuted,
)

/** How the Mac has been used: CPU/GPU, memory with pressure, queue jobs (sm#1609). */
@Composable
fun UsageScreen(
    onBack: () -> Unit,
    viewModel: QueueViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val resumed = rememberResumed()
    val hours = state.usageHours
    LaunchedEffect(hours, resumed) {
        if (!resumed) return@LaunchedEffect
        viewModel.refreshUsage(hours)
        val every = usageRefreshSeconds(hours) ?: return@LaunchedEffect
        while (isActive) {
            delay(every * 1000)
            viewModel.refreshUsage(hours)
        }
    }
    // Slot maximum for the jobs chart's scale, from the Queue page's last read.
    LaunchedEffect(Unit) { if (state.overview == null) viewModel.refresh() }

    Column(
        Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding()
            .background(MaterialTheme.colorScheme.background)
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Rounded.ArrowBack, contentDescription = "Back") }
            Text("Mac usage", style = MaterialTheme.typography.headlineSmall, modifier = Modifier.weight(1f))
            USAGE_RANGES.forEach { (value, label) ->
                Text(
                    label,
                    color = if (value == hours) Cyan else TextMuted,
                    fontWeight = if (value == hours) FontWeight.Bold else FontWeight.Normal,
                    modifier = Modifier.clickable { viewModel.refreshUsage(value) }.padding(horizontal = 6.dp, vertical = 8.dp),
                )
            }
        }
        val series = state.usage
        when {
            state.usageLoading && series == null ->
                Box(Modifier.fillMaxWidth().height(200.dp), contentAlignment = Alignment.Center) { CircularProgressIndicator(color = Cyan) }
            series == null || !series.available ->
                Text(state.usageError ?: "No utilization recorded yet.", color = TextMuted)
            else -> UsageCharts(series, slotMax = state.overview?.slots?.max ?: 0)
        }
    }
}

@Composable
private fun UsageCharts(series: UtilizationSeries, slotMax: Int) {
    val zone = remember { ZoneId.systemDefault() }
    val buckets = series.buckets
    var selected by remember(series) { mutableStateOf<Int?>(null) }
    val total = series.memoryTotalBytes?.toDouble()?.takeIf { it > 0 }
    val jobMax = maxOf(
        slotMax.toDouble(),
        buckets.maxOfOrNull { b -> TYPE_COLORS.sumOf { (type, _) -> b.running[type] ?: 0.0 } } ?: 0.0,
        1.0,
    )

    val touch = Modifier.pointerInput(buckets.size) {
        fun index(x: Float) = ((x / size.width) * buckets.size).toInt().coerceIn(0, buckets.size - 1)
        detectDragGestures(
            onDragStart = { selected = index(it.x) },
            onDragEnd = { selected = null },
            onDragCancel = { selected = null },
            onDrag = { change, _ -> selected = index(change.position.x) },
        )
    }.pointerInput(buckets.size) {
        detectTapGestures(onPress = {
            selected = ((it.x / size.width) * buckets.size).toInt().coerceIn(0, buckets.size - 1)
            tryAwaitRelease()
            selected = null
        })
    }

    ChartCard("CPU and GPU %", listOf("CPU" to Amber, "GPU" to Cyan)) {
        Canvas(Modifier.fillMaxWidth().height(120.dp).then(touch)) {
            grid()
            band(buckets.map { b -> b.cpuAvg?.let { avg -> b.cpuMax?.let { max -> avg to max } } }, 100.0, Amber)
            line(buckets.map { it.cpuAvg }, 100.0, Amber)
            line(buckets.map { it.gpuAvg }, 100.0, Cyan)
            cursor(selected, buckets.size)
        }
    }
    ChartCard(
        "Memory used of ${gib(series.memoryTotalBytes) ?: "?"}G",
        listOf("used" to Emerald, "pressure" to Rose),
    ) {
        Canvas(Modifier.fillMaxWidth().height(120.dp).then(touch)) {
            val w = size.width / buckets.size.coerceAtLeast(1)
            buckets.forEachIndexed { i, b ->
                val shade = pressureShade(b.pressureMax)
                if (shade > 0) {
                    drawRect(Rose.copy(alpha = if (shade == 2) 0.24f else 0.12f), Offset(i * w, 0f), Size(w, size.height))
                }
            }
            grid()
            if (total != null) {
                area(buckets.map { it.memUsedAvg?.toDouble() }, total, Emerald)
            }
            cursor(selected, buckets.size)
        }
    }
    ChartCard("Queue jobs running", TYPE_COLORS.take(3)) {
        Canvas(Modifier.fillMaxWidth().height(120.dp).then(touch)) {
            val w = size.width / buckets.size.coerceAtLeast(1)
            buckets.forEachIndexed { i, b ->
                var base = 0.0
                TYPE_COLORS.forEach { (type, color) ->
                    val v = b.running[type] ?: 0.0
                    if (v > 0) {
                        val top = size.height * (1 - (base + v) / jobMax).toFloat()
                        val h = size.height * (v / jobMax).toFloat()
                        drawRect(color, Offset(i * w + w * 0.1f, top), Size(w * 0.8f, h))
                    }
                    base += v
                }
            }
            drawLine(Border, Offset(0f, size.height), Offset(size.width, size.height), 1.dp.toPx())
            cursor(selected, buckets.size)
        }
        AxisLabels(usageAxisLabels(buckets.map { it.start }, series.hours, zone), buckets.size)
    }

    selected?.let { index ->
        Surface(color = PanelElevated, shape = RoundedCornerShape(8.dp), modifier = Modifier.fillMaxWidth()) {
            Column(Modifier.padding(10.dp)) {
                bucketTooltip(buckets[index], series.bucketSeconds, zone).forEach {
                    Text(it, style = MaterialTheme.typography.bodySmall)
                }
            }
        }
    }

    val summary = usageSummaryLines(series.summary)
    if (summary.isNotEmpty()) {
        Surface(color = Panel, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth()) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                summary.forEach { Text(it, style = MaterialTheme.typography.bodyMedium) }
            }
        }
    }
    Spacer(Modifier.height(24.dp))
}

@Composable
private fun ChartCard(title: String, legend: List<Pair<String, Color>>, content: @Composable () -> Unit) {
    Surface(color = Panel, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(10.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(title, style = MaterialTheme.typography.labelLarge, modifier = Modifier.weight(1f))
                legend.forEach { (name, color) ->
                    Box(Modifier.size(8.dp).background(color, RoundedCornerShape(2.dp)))
                    Text(" $name  ", style = MaterialTheme.typography.labelSmall, color = TextSecondary)
                }
            }
            content()
        }
    }
}

@Composable
private fun AxisLabels(labels: List<Pair<Int, String>>, count: Int) {
    Box(Modifier.fillMaxWidth().height(16.dp)) {
        labels.forEach { (index, label) ->
            val fraction = if (count <= 1) 0f else index.toFloat() / (count - 1)
            Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.TopStart) {
                Text(
                    label,
                    style = MaterialTheme.typography.labelSmall,
                    color = TextMuted,
                    modifier = Modifier.fillMaxWidth(fraction.coerceAtLeast(0.001f)).padding(0.dp),
                    textAlign = androidx.compose.ui.text.style.TextAlign.End,
                )
            }
        }
    }
}

private fun DrawScope.grid() {
    listOf(0f, 0.5f, 1f).forEach { f ->
        drawLine(Border.copy(alpha = 0.5f), Offset(0f, size.height * f), Offset(size.width, size.height * f), 1f)
    }
}

private fun DrawScope.x(index: Int, count: Int): Float = (index + 0.5f) * size.width / count.coerceAtLeast(1)

private fun DrawScope.y(value: Double, max: Double): Float = size.height * (1 - (value / max).coerceIn(0.0, 1.0)).toFloat()

/** A line broken at every gap, never interpolated across it. */
private fun DrawScope.line(values: List<Double?>, max: Double, color: Color) {
    plotRuns(values).forEach { run ->
        if (run.size == 1) {
            drawCircle(color, 1.5.dp.toPx(), Offset(x(run[0].first, values.size), y(run[0].second, max)))
            return@forEach
        }
        val path = Path()
        run.forEachIndexed { i, (index, v) ->
            if (i == 0) path.moveTo(x(index, values.size), y(v, max)) else path.lineTo(x(index, values.size), y(v, max))
        }
        drawPath(path, color, style = Stroke(1.5.dp.toPx()))
    }
}

private fun DrawScope.area(values: List<Double?>, max: Double, color: Color) {
    plotRuns(values).forEach { run ->
        val path = Path()
        run.forEachIndexed { i, (index, v) ->
            if (i == 0) path.moveTo(x(index, values.size), y(v, max)) else path.lineTo(x(index, values.size), y(v, max))
        }
        val line = Path().apply { addPath(path) }
        path.lineTo(x(run.last().first, values.size), size.height)
        path.lineTo(x(run.first().first, values.size), size.height)
        path.close()
        drawPath(path, color.copy(alpha = 0.22f))
        drawPath(line, color, style = Stroke(1.5.dp.toPx()))
    }
}

/** Average-to-peak band behind a line, so short spikes stay visible. */
private fun DrawScope.band(values: List<Pair<Double, Double>?>, max: Double, color: Color) {
    plotRuns(values).forEach { run ->
        val path = Path()
        run.forEachIndexed { i, (index, pair) ->
            if (i == 0) path.moveTo(x(index, values.size), y(pair.second, max)) else path.lineTo(x(index, values.size), y(pair.second, max))
        }
        run.asReversed().forEach { (index, pair) -> path.lineTo(x(index, values.size), y(pair.first, max)) }
        path.close()
        drawPath(path, color.copy(alpha = 0.15f))
    }
}

private fun DrawScope.cursor(selected: Int?, count: Int) {
    selected ?: return
    val cx = x(selected, count)
    drawLine(TextSecondary, Offset(cx, 0f), Offset(cx, size.height), 1.dp.toPx())
}
