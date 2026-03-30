package li.rajeshgo.sm.ui.analytics

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ArrowBack
import androidx.compose.material.icons.rounded.Analytics
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.Settings
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.OffsetDateTime
import kotlin.coroutines.coroutineContext
import kotlin.math.absoluteValue
import kotlin.math.max
import kotlin.math.min
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import li.rajeshgo.sm.data.model.AnalyticsDistributionItem
import li.rajeshgo.sm.data.model.AnalyticsHealthCheck
import li.rajeshgo.sm.data.model.AnalyticsKpi
import li.rajeshgo.sm.data.model.AnalyticsSummary
import li.rajeshgo.sm.data.model.AnalyticsThroughputBucket
import li.rajeshgo.sm.ui.navigation.AppBottomNav
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Fuchsia
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.PanelElevated
import li.rajeshgo.sm.ui.theme.PanelMuted
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet
import li.rajeshgo.sm.ui.update.SettingsIconButtonWithUpdate
import li.rajeshgo.sm.ui.update.UpdateAvailabilityViewModel
import li.rajeshgo.sm.ui.update.UpdateReadyBanner

private const val ANALYTICS_AUTO_REFRESH_MS = 10000L

@Composable
fun AnalyticsScreen(
    onNavigateToWatch: () -> Unit,
    onNavigateToSettings: () -> Unit,
    onOpenDetail: (String) -> Unit,
    viewModel: AnalyticsViewModel = viewModel(),
    updateViewModel: UpdateAvailabilityViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val updateState by updateViewModel.uiState.collectAsState()
    val lifecycleOwner = LocalLifecycleOwner.current
    var isResumed by remember {
        mutableStateOf(lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED))
    }

    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, _ ->
            isResumed = lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

    LaunchedEffect(isResumed, state.serverUrl, state.userEmail) {
        if (!isResumed || state.serverUrl.isBlank()) {
            return@LaunchedEffect
        }
        viewModel.refresh()
        updateViewModel.refresh()
        while (coroutineContext.isActive) {
            delay(ANALYTICS_AUTO_REFRESH_MS)
            viewModel.refresh()
        }
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background),
    ) {
        if (state.loading && state.summary == null) {
            Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(color = Cyan)
            }
            return@Box
        }

        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 112.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item {
                AnalyticsHeader(
                    userEmail = state.userEmail,
                    generatedAt = state.summary?.generatedAt,
                    refreshing = state.refreshing,
                    hasUpdate = updateState.availableUpdate != null,
                    onRefresh = { viewModel.refresh() },
                    onOpenSettings = onNavigateToSettings,
                )
            }

            updateState.availableUpdate?.let { update ->
                item {
                    UpdateReadyBanner(
                        update = update,
                        onOpenSettings = onNavigateToSettings,
                    )
                }
            }

            state.error?.takeIf { it.isNotBlank() }?.let { message ->
                item {
                    InlineErrorBanner(message = message)
                }
            }

            val summary = state.summary
            if (summary == null) {
                item {
                    EmptyAnalyticsState()
                }
            } else {
                item {
                    KpiGrid(
                        summary = summary,
                        onOpenDetail = onOpenDetail,
                    )
                }
                item {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        StateDistributionCard(
                            summary = summary,
                            modifier = Modifier.weight(1f),
                        )
                        ReliabilityCard(
                            summary = summary,
                            modifier = Modifier.weight(1f),
                            onOpenDetail = { onOpenDetail("reliability") },
                        )
                    }
                }
                item {
                    ThroughputCard(
                        summary = summary,
                        onOpenDetail = { onOpenDetail("throughput") },
                    )
                }
                item {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        CoordinationCard(
                            summary = summary,
                            modifier = Modifier.weight(1f),
                        )
                        LoadCard(
                            summary = summary,
                            modifier = Modifier.weight(1f),
                            onOpenDetail = { onOpenDetail("load") },
                        )
                    }
                }
                item {
                    DistributionCard(
                        title = "Repository Contribution",
                        subtitle = "Where the load is concentrated right now",
                        rows = summary.repoDistribution.map {
                            DistributionRowModel(
                                label = it.label,
                                valueText = "${it.sessionCount} live",
                                progress = (it.sharePct / 100f).coerceIn(0f, 1f),
                                meta = compactTokenCount(it.tokensUsed),
                                color = repoColor(it.label),
                            )
                        },
                        onClick = { onOpenDetail("repos") },
                    )
                }
                item {
                    DistributionCard(
                        title = "Provider Split",
                        subtitle = "Current runtime mix across active sessions",
                        rows = summary.providerDistribution.map {
                            DistributionRowModel(
                                label = it.label,
                                valueText = "${it.count}",
                                progress = ((it.sharePct ?: 0f) / 100f).coerceIn(0f, 1f),
                                meta = formatShare(it.sharePct),
                                color = providerColor(it.label),
                            )
                        },
                        onClick = { onOpenDetail("providers") },
                    )
                }
                if (summary.longestRunning.isNotEmpty()) {
                    item {
                        LongestRunningCard(
                            summary = summary,
                            onOpenDetail = { onOpenDetail("longest") },
                        )
                    }
                }
            }
        }

        Box(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(horizontal = 16.dp, vertical = 16.dp),
        ) {
            AppBottomNav(
                currentRoute = Routes.ANALYTICS,
                onWatch = onNavigateToWatch,
                onAnalytics = {},
            )
        }
    }
}

@Composable
fun AnalyticsDetailScreen(
    section: String,
    onBack: () -> Unit,
    onNavigateToWatch: () -> Unit,
    onNavigateToAnalytics: () -> Unit,
    onNavigateToSettings: () -> Unit,
    viewModel: AnalyticsViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val summary = state.summary

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background),
    ) {
        if (state.loading && summary == null) {
            Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(color = Cyan)
            }
            return@Box
        }

        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 112.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item {
                DetailHeader(
                    title = detailTitle(section),
                    subtitle = summary?.generatedAt?.let { "Live snapshot ${formatGeneratedAt(it)}" } ?: "No data",
                    onBack = onBack,
                    onOpenSettings = onNavigateToSettings,
                )
            }
            state.error?.takeIf { it.isNotBlank() }?.let { message ->
                item { InlineErrorBanner(message) }
            }
            if (summary == null) {
                item { EmptyAnalyticsState() }
            } else {
                when (section) {
                    "throughput" -> {
                        item {
                            ThroughputCard(summary = summary, onOpenDetail = {})
                        }
                        item {
                            DenseTableCard(
                                title = "Window Breakdown",
                                columns = listOf("Window", "Send", "Spawn", "Track"),
                                rows = summary.throughput.map {
                                    listOf(it.bucketLabel, it.sends.toString(), it.spawns.toString(), it.trackReminders.toString())
                                },
                            )
                        }
                    }
                    "reliability" -> {
                        item { ReliabilityCard(summary = summary, modifier = Modifier.fillMaxWidth(), onOpenDetail = {}) }
                        item {
                            DenseTableCard(
                                title = "Infra Checks",
                                columns = listOf("Check", "State", "Detail"),
                                rows = summary.healthChecks.map {
                                    listOf(it.label, (it.status ?: "unknown").uppercase(), it.message ?: "-")
                                },
                            )
                        }
                    }
                    "repos" -> {
                        item {
                            DenseTableCard(
                                title = "Repository Contribution",
                                columns = listOf("Repo", "Live", "Tokens", "Share"),
                                rows = summary.repoDistribution.map {
                                    listOf(it.label, it.sessionCount.toString(), compactTokenCount(it.tokensUsed), formatShare(it.sharePct))
                                },
                            )
                        }
                    }
                    "providers" -> {
                        item {
                            DenseTableCard(
                                title = "Provider Distribution",
                                columns = listOf("Provider", "Live", "Share"),
                                rows = summary.providerDistribution.map {
                                    listOf(it.label, it.count.toString(), formatShare(it.sharePct))
                                },
                            )
                        }
                    }
                    "load" -> {
                        item { LoadCard(summary = summary, modifier = Modifier.fillMaxWidth(), onOpenDetail = {}) }
                        item {
                            DenseTableCard(
                                title = "Load Snapshot",
                                columns = listOf("Metric", "Value"),
                                rows = listOf(
                                    listOf("Tokens live", compactTokenCount(summary.totals.tokensLive)),
                                    listOf("Top repo", summary.repoDistribution.firstOrNull()?.label ?: "-"),
                                    listOf("Top provider", summary.providerDistribution.firstOrNull()?.label ?: "-"),
                                    listOf("Track reminders", summary.totals.trackReminders24h.toString()),
                                ),
                            )
                        }
                    }
                    "longest" -> {
                        item {
                            DenseTableCard(
                                title = "Longest Running Sessions",
                                columns = listOf("Session", "Repo", "Prov", "Age"),
                                rows = summary.longestRunning.map {
                                    listOf(it.name, it.repo, it.provider, "${it.ageHours}h")
                                },
                            )
                        }
                    }
                    else -> {
                        item {
                            DenseTableCard(
                                title = "Live Metrics",
                                columns = listOf("Metric", "Value"),
                                rows = listOf(
                                    listOf(summary.kpis.activeSessions.label, summary.kpis.activeSessions.value.toString()),
                                    listOf(summary.kpis.sends24h.label, summary.kpis.sends24h.value.toString()),
                                    listOf(summary.kpis.spawns24h.label, summary.kpis.spawns24h.value.toString()),
                                    listOf(summary.kpis.activeTracks.label, summary.kpis.activeTracks.value.toString()),
                                    listOf(summary.kpis.overdueTracks.label, summary.kpis.overdueTracks.value.toString()),
                                    listOf(summary.kpis.incidents24h.label, summary.kpis.incidents24h.value.toString()),
                                ),
                            )
                        }
                    }
                }
            }
        }

        Box(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(horizontal = 16.dp, vertical = 16.dp),
        ) {
            AppBottomNav(
                currentRoute = Routes.ANALYTICS,
                onWatch = onNavigateToWatch,
                onAnalytics = onNavigateToAnalytics,
            )
        }
    }
}

@Composable
private fun AnalyticsHeader(
    userEmail: String,
    generatedAt: String?,
    refreshing: Boolean,
    hasUpdate: Boolean,
    onRefresh: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    Surface(
        color = Panel,
        shape = RoundedCornerShape(8.dp),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 14.dp, vertical = 12.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Icon(Icons.Rounded.Analytics, contentDescription = null, tint = Cyan)
                    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                        Text(
                            text = "sm analytics",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurface,
                            fontWeight = FontWeight.Black,
                            letterSpacing = androidx.compose.ui.unit.TextUnit.Unspecified,
                        )
                        val subtitle = listOfNotNull(
                            userEmail.takeIf { it.isNotBlank() },
                            generatedAt?.let(::formatGeneratedAt)?.let { "Sync $it" },
                        ).joinToString("  •  ")
                        Text(
                            text = subtitle.ifBlank { "Operational telemetry" },
                            style = MaterialTheme.typography.bodySmall,
                            color = TextSecondary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (refreshing) {
                        CircularProgressIndicator(
                            modifier = Modifier.size(16.dp),
                            strokeWidth = 2.dp,
                            color = Cyan,
                        )
                    } else {
                        IconButton(onClick = onRefresh) {
                            Icon(Icons.Rounded.Refresh, contentDescription = "Refresh", tint = TextSecondary)
                        }
                    }
                    SettingsIconButtonWithUpdate(hasUpdate = hasUpdate, onClick = onOpenSettings)
                }
            }
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(1.dp)
                    .background(PanelMuted),
            )
        }
    }
}

@Composable
private fun DetailHeader(
    title: String,
    subtitle: String,
    onBack: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    Surface(color = Panel, shape = RoundedCornerShape(8.dp)) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 12.dp, vertical = 12.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Rounded.ArrowBack, contentDescription = "Back", tint = TextSecondary)
                    }
                    Column {
                        Text(
                            text = title,
                            style = MaterialTheme.typography.labelLarge,
                            color = MaterialTheme.colorScheme.onSurface,
                            fontWeight = FontWeight.Black,
                        )
                        Text(
                            text = subtitle,
                            style = MaterialTheme.typography.bodySmall,
                            color = TextSecondary,
                        )
                    }
                }
                IconButton(onClick = onOpenSettings) {
                    Icon(Icons.Rounded.Settings, contentDescription = "Settings", tint = TextSecondary)
                }
            }
        }
    }
}

@Composable
private fun InlineErrorBanner(message: String) {
    Surface(color = Rose.copy(alpha = 0.12f), shape = RoundedCornerShape(8.dp)) {
        Text(
            text = message,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp),
            style = MaterialTheme.typography.bodySmall,
            color = Rose,
        )
    }
}

@Composable
private fun EmptyAnalyticsState() {
    Surface(color = PanelElevated, shape = RoundedCornerShape(12.dp)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(
                text = "Analytics unavailable",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
                fontWeight = FontWeight.Bold,
            )
            Text(
                text = "Sign in and wait for a live backend snapshot to populate charts and trend tables.",
                style = MaterialTheme.typography.bodySmall,
                color = TextSecondary,
            )
        }
    }
}

@Composable
private fun KpiGrid(
    summary: AnalyticsSummary,
    onOpenDetail: (String) -> Unit,
) {
    val throughput = summary.throughput
    val kpis = listOf(
        KpiCardModel(summary.kpis.activeSessions, throughput.map { maxOf(it.sends, it.spawns, it.trackReminders) }, "Overview", Cyan, "overview"),
        KpiCardModel(summary.kpis.sends24h, throughput.map { it.sends }, "vs prev", Emerald, "throughput"),
        KpiCardModel(summary.kpis.spawns24h, throughput.map { it.spawns }, "dispatch", Amber, "throughput"),
        KpiCardModel(summary.kpis.activeTracks, throughput.map { it.trackReminders }, "armed", Violet, "throughput"),
        KpiCardModel(summary.kpis.overdueTracks, throughput.map { if (it.trackReminders > 0) 1 else 0 }, "late", Rose, "reliability"),
        KpiCardModel(summary.kpis.incidents24h, throughput.map { 0 }, "events", Fuchsia, "reliability"),
    )
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        kpis.chunked(2).forEach { row ->
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                row.forEach { model ->
                    KpiCard(model = model, modifier = Modifier.weight(1f), onClick = { onOpenDetail(model.section) })
                }
                if (row.size == 1) {
                    Spacer(modifier = Modifier.weight(1f))
                }
            }
        }
    }
}

private data class KpiCardModel(
    val kpi: AnalyticsKpi,
    val series: List<Int>,
    val suffix: String,
    val accent: Color,
    val section: String,
)

@Composable
private fun KpiCard(
    model: KpiCardModel,
    modifier: Modifier = Modifier,
    onClick: () -> Unit,
) {
    Surface(
        modifier = modifier.clickable(onClick = onClick),
        color = PanelElevated,
        shape = RoundedCornerShape(8.dp),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .height(96.dp)
                .padding(12.dp),
            verticalArrangement = Arrangement.SpaceBetween,
        ) {
            Row(horizontalArrangement = Arrangement.SpaceBetween, modifier = Modifier.fillMaxWidth()) {
                Text(
                    text = model.kpi.label.uppercase(),
                    style = MaterialTheme.typography.labelSmall,
                    color = TextMuted,
                    fontWeight = FontWeight.Black,
                )
                model.kpi.deltaPct?.let {
                    Text(
                        text = formatDelta(it),
                        style = MaterialTheme.typography.labelSmall,
                        color = if (it >= 0f) Emerald else Rose,
                        fontWeight = FontWeight.Bold,
                    )
                }
            }
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.Bottom,
            ) {
                Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text(
                        text = compactNumber(model.kpi.value),
                        style = MaterialTheme.typography.titleLarge,
                        color = MaterialTheme.colorScheme.onSurface,
                        fontWeight = FontWeight.Black,
                        fontFamily = FontFamily.Monospace,
                    )
                    Text(
                        text = model.suffix,
                        style = MaterialTheme.typography.labelSmall,
                        color = TextSecondary,
                    )
                }
                Sparkline(
                    values = model.series,
                    lineColor = model.accent,
                    modifier = Modifier
                        .width(58.dp)
                        .height(22.dp),
                )
            }
        }
    }
}

@Composable
private fun StateDistributionCard(
    summary: AnalyticsSummary,
    modifier: Modifier = Modifier,
) {
    Surface(modifier = modifier, color = PanelElevated, shape = RoundedCornerShape(8.dp)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                text = "LIVE LOAD",
                style = MaterialTheme.typography.labelSmall,
                color = TextMuted,
                fontWeight = FontWeight.Black,
            )
            StackedLoadBar(summary.stateDistribution)
            summary.stateDistribution.forEach { item ->
                LegendRow(label = item.label, value = item.count.toString(), color = stateColor(item.key))
            }
        }
    }
}

@Composable
private fun ReliabilityCard(
    summary: AnalyticsSummary,
    modifier: Modifier = Modifier,
    onOpenDetail: () -> Unit,
) {
    val bars = listOf(
        ReliabilityBar("BKD", countUnhealthyChecks(summary.healthChecks), Rose),
        ReliabilityBar("ATT", if (summary.attachAvailable) 0 else 1, if (summary.attachAvailable) Emerald else Rose),
        ReliabilityBar("RST", summary.reliability.restartCount24h, Amber),
        ReliabilityBar("HEAL", summary.reliability.selfHealCount24h, Cyan),
    )
    Surface(modifier = modifier.clickable(onClick = onOpenDetail), color = PanelElevated, shape = RoundedCornerShape(8.dp)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Text(
                    text = "RELIABILITY",
                    style = MaterialTheme.typography.labelSmall,
                    color = TextMuted,
                    fontWeight = FontWeight.Black,
                )
                Text(
                    text = if (summary.attachAvailable) "ATTACH READY" else "ATTACH DEGRADED",
                    style = MaterialTheme.typography.labelSmall,
                    color = if (summary.attachAvailable) Emerald else Rose,
                    fontWeight = FontWeight.Bold,
                )
            }
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalAlignment = Alignment.Bottom,
            ) {
                bars.forEach { bar ->
                    ReliabilityBarView(bar = bar, modifier = Modifier.weight(1f))
                }
            }
            summary.healthChecks.take(3).forEach { check ->
                StatusRow(check = check)
            }
        }
    }
}

private data class ReliabilityBar(
    val label: String,
    val value: Int,
    val color: Color,
)

@Composable
private fun ReliabilityBarView(bar: ReliabilityBar, modifier: Modifier = Modifier) {
    val normalized = min(1f, max(0.08f, if (bar.value <= 0) 0.08f else bar.value / 6f))
    Column(modifier = modifier, horizontalAlignment = Alignment.CenterHorizontally) {
        Box(
            modifier = Modifier
                .height(72.dp)
                .fillMaxWidth(),
            contentAlignment = Alignment.BottomCenter,
        ) {
            Box(
                modifier = Modifier
                    .fillMaxWidth(0.7f)
                    .height((72f * normalized).dp)
                    .background(bar.color.copy(alpha = 0.85f), RoundedCornerShape(topStart = 2.dp, topEnd = 2.dp)),
            )
        }
        Text(
            text = bar.label,
            style = MaterialTheme.typography.labelSmall,
            color = TextSecondary,
            fontWeight = FontWeight.Bold,
        )
    }
}

@Composable
private fun StatusRow(check: AnalyticsHealthCheck) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = check.label,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurface,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        Text(
            text = (check.status ?: "unknown").uppercase(),
            style = MaterialTheme.typography.labelSmall,
            color = healthStatusColor(check.status),
            fontWeight = FontWeight.Bold,
            textAlign = TextAlign.End,
        )
    }
}

@Composable
private fun ThroughputCard(
    summary: AnalyticsSummary,
    onOpenDetail: () -> Unit,
) {
    Surface(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onOpenDetail),
        color = PanelElevated,
        shape = RoundedCornerShape(8.dp),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                Column {
                    Text(
                        text = "THROUGHPUT",
                        style = MaterialTheme.typography.labelSmall,
                        color = TextMuted,
                        fontWeight = FontWeight.Black,
                    )
                    Text(
                        text = "Sends, dispatches, and track nudges over the last 24h",
                        style = MaterialTheme.typography.bodySmall,
                        color = TextSecondary,
                    )
                }
                Text(
                    text = "${summary.windowHours}H",
                    style = MaterialTheme.typography.labelSmall,
                    color = TextSecondary,
                    fontWeight = FontWeight.Bold,
                )
            }
            MultiSeriesChart(
                buckets = summary.throughput,
                modifier = Modifier
                    .fillMaxWidth()
                    .height(180.dp),
            )
            Row(
                modifier = Modifier.horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                LegendRow(label = "Sends", value = summary.kpis.sends24h.value.toString(), color = Emerald)
                LegendRow(label = "Dispatches", value = summary.kpis.spawns24h.value.toString(), color = Cyan)
                LegendRow(label = "Track remind", value = summary.totals.trackReminders24h.toString(), color = Amber)
            }
        }
    }
}

@Composable
private fun CoordinationCard(
    summary: AnalyticsSummary,
    modifier: Modifier = Modifier,
) {
    Surface(modifier = modifier, color = PanelElevated, shape = RoundedCornerShape(8.dp)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                text = "COORDINATION",
                style = MaterialTheme.typography.labelSmall,
                color = TextMuted,
                fontWeight = FontWeight.Black,
            )
            MetricLine("Tracks active", summary.kpis.activeTracks.value.toString(), Violet)
            MetricLine("Overdue", summary.kpis.overdueTracks.value.toString(), Rose)
            MetricLine("Track reminders", summary.totals.trackReminders24h.toString(), Amber)
            MetricLine("Attach", if (summary.attachAvailable) "ready" else "blocked", if (summary.attachAvailable) Emerald else Rose)
        }
    }
}

@Composable
private fun LoadCard(
    summary: AnalyticsSummary,
    modifier: Modifier = Modifier,
    onOpenDetail: () -> Unit,
) {
    Surface(modifier = modifier.clickable(onClick = onOpenDetail), color = PanelElevated, shape = RoundedCornerShape(8.dp)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                text = "LOAD",
                style = MaterialTheme.typography.labelSmall,
                color = TextMuted,
                fontWeight = FontWeight.Black,
            )
            MetricLine("Tokens live", compactTokenCount(summary.totals.tokensLive), Cyan)
            MetricLine("Top repo", summary.repoDistribution.firstOrNull()?.label ?: "-", Amber)
            MetricLine("Top provider", summary.providerDistribution.firstOrNull()?.label ?: "-", Fuchsia)
            MetricLine("Longest run", summary.longestRunning.firstOrNull()?.let { "${it.ageHours}h" } ?: "-", Emerald)
        }
    }
}

private data class DistributionRowModel(
    val label: String,
    val valueText: String,
    val progress: Float,
    val meta: String,
    val color: Color,
)

@Composable
private fun DistributionCard(
    title: String,
    subtitle: String,
    rows: List<DistributionRowModel>,
    onClick: () -> Unit,
) {
    Surface(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onClick),
        color = PanelElevated,
        shape = RoundedCornerShape(8.dp),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(
                    text = title.uppercase(),
                    style = MaterialTheme.typography.labelSmall,
                    color = TextMuted,
                    fontWeight = FontWeight.Black,
                )
                Text(
                    text = subtitle,
                    style = MaterialTheme.typography.bodySmall,
                    color = TextSecondary,
                )
            }
            rows.take(5).forEach { row ->
                DistributionRow(row)
            }
        }
    }
}

@Composable
private fun DistributionRow(row: DistributionRowModel) {
    Column(verticalArrangement = Arrangement.spacedBy(5.dp)) {
        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
            Text(
                text = row.label,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurface,
                fontWeight = FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            Spacer(modifier = Modifier.width(12.dp))
            Text(
                text = row.valueText,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurface,
                fontWeight = FontWeight.Bold,
            )
        }
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(8.dp)
                .background(PanelMuted, RoundedCornerShape(999.dp)),
        ) {
            Box(
                modifier = Modifier
                    .fillMaxWidth(max(0.06f, row.progress))
                    .height(8.dp)
                    .background(row.color, RoundedCornerShape(999.dp)),
            )
        }
        Text(
            text = row.meta,
            style = MaterialTheme.typography.labelSmall,
            color = TextSecondary,
        )
    }
}

@Composable
private fun LongestRunningCard(
    summary: AnalyticsSummary,
    onOpenDetail: () -> Unit,
) {
    Surface(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onOpenDetail),
        color = PanelElevated,
        shape = RoundedCornerShape(8.dp),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                text = "LONGEST RUNNING",
                style = MaterialTheme.typography.labelSmall,
                color = TextMuted,
                fontWeight = FontWeight.Black,
            )
            summary.longestRunning.take(5).forEach { item ->
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(modifier = Modifier.weight(1f)) {
                        Text(
                            text = item.name,
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurface,
                            fontWeight = FontWeight.Medium,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                        Text(
                            text = "${item.repo} • ${item.provider}",
                            style = MaterialTheme.typography.labelSmall,
                            color = TextSecondary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    Text(
                        text = "${item.ageHours}h",
                        style = MaterialTheme.typography.labelMedium,
                        color = Amber,
                        fontWeight = FontWeight.Bold,
                        fontFamily = FontFamily.Monospace,
                    )
                }
            }
        }
    }
}

@Composable
private fun DenseTableCard(
    title: String,
    columns: List<String>,
    rows: List<List<String>>,
) {
    Surface(color = PanelElevated, shape = RoundedCornerShape(8.dp)) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                text = title.uppercase(),
                style = MaterialTheme.typography.labelSmall,
                color = TextMuted,
                fontWeight = FontWeight.Black,
            )
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                columns.forEach { column ->
                    Text(
                        text = column,
                        modifier = Modifier.weight(1f),
                        style = MaterialTheme.typography.labelSmall,
                        color = TextSecondary,
                        fontWeight = FontWeight.Bold,
                    )
                }
            }
            rows.forEach { row ->
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    row.forEach { cell ->
                        Text(
                            text = cell,
                            modifier = Modifier.weight(1f),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurface,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun MetricLine(label: String, value: String, accent: Color) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Box(
                modifier = Modifier
                    .size(8.dp)
                    .background(accent, RoundedCornerShape(2.dp)),
            )
            Text(
                text = label,
                style = MaterialTheme.typography.bodySmall,
                color = TextSecondary,
            )
        }
        Text(
            text = value,
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.onSurface,
            fontWeight = FontWeight.Bold,
            fontFamily = FontFamily.Monospace,
        )
    }
}

@Composable
private fun LegendRow(label: String, value: String, color: Color) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        Box(
            modifier = Modifier
                .size(9.dp)
                .background(color, RoundedCornerShape(2.dp)),
        )
        Text(
            text = "$label $value",
            style = MaterialTheme.typography.labelSmall,
            color = TextSecondary,
        )
    }
}

@Composable
private fun StackedLoadBar(items: List<AnalyticsDistributionItem>) {
    val total = items.sumOf { it.count }.coerceAtLeast(1)
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .height(28.dp)
            .background(PanelMuted, RoundedCornerShape(4.dp)),
    ) {
        items.forEach { item ->
            val fraction = (item.count.toFloat() / total.toFloat()).coerceIn(0f, 1f)
            if (fraction <= 0f) return@forEach
            Box(
                modifier = Modifier
                    .weight(item.count.toFloat())
                    .height(28.dp)
                    .background(stateColor(item.key)),
                contentAlignment = Alignment.Center,
            ) {
                if (fraction > 0.18f) {
                    Text(
                        text = "${item.count}",
                        style = MaterialTheme.typography.labelSmall,
                        color = if (item.key == "idle") InkOnDark else Color.Black,
                        fontWeight = FontWeight.Black,
                    )
                }
            }
        }
    }
}

private val InkOnDark = Color(0xFFD9DCE2)

@Composable
private fun Sparkline(
    values: List<Int>,
    lineColor: Color,
    modifier: Modifier = Modifier,
) {
    Canvas(modifier = modifier) {
        val filtered = values.ifEmpty { listOf(0, 0, 0) }
        val maxValue = filtered.maxOrNull()?.coerceAtLeast(1) ?: 1
        val stepX = if (filtered.size <= 1) size.width else size.width / (filtered.size - 1)
        val path = Path()
        filtered.forEachIndexed { index, value ->
            val x = stepX * index
            val y = size.height - ((value.toFloat() / maxValue.toFloat()) * (size.height - 2.dp.toPx()))
            if (index == 0) path.moveTo(x, y) else path.lineTo(x, y)
        }
        drawPath(path = path, color = lineColor, style = Stroke(width = 2.dp.toPx(), cap = StrokeCap.Round))
    }
}

@Composable
private fun MultiSeriesChart(
    buckets: List<AnalyticsThroughputBucket>,
    modifier: Modifier = Modifier,
) {
    val sends = buckets.map { it.sends }
    val spawns = buckets.map { it.spawns }
    val tracks = buckets.map { it.trackReminders }
    val maxValue = max(1, max(sends.maxOrNull() ?: 0, max(spawns.maxOrNull() ?: 0, tracks.maxOrNull() ?: 0)))

    Surface(color = Panel, shape = RoundedCornerShape(6.dp)) {
        Column(
            modifier = modifier.padding(10.dp),
            verticalArrangement = Arrangement.SpaceBetween,
        ) {
            Box(modifier = Modifier.weight(1f).fillMaxWidth()) {
                Canvas(modifier = Modifier.fillMaxSize()) {
                    val chartHeight = size.height
                    val chartWidth = size.width
                    val horizontalLines = 4
                    repeat(horizontalLines) { index ->
                        val y = chartHeight * index / (horizontalLines - 1)
                        drawLine(
                            color = Border.copy(alpha = 0.45f),
                            start = Offset(0f, y),
                            end = Offset(chartWidth, y),
                            strokeWidth = 1.dp.toPx(),
                        )
                    }
                    drawSeries(sends, chartWidth, chartHeight, maxValue, Emerald)
                    drawSeries(spawns, chartWidth, chartHeight, maxValue, Cyan)
                    drawSeries(tracks, chartWidth, chartHeight, maxValue, Amber)
                }
            }
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(top = 8.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                buckets.filterIndexed { index, _ -> index % 3 == 0 }.forEach { bucket ->
                    Text(
                        text = bucket.bucketLabel,
                        style = MaterialTheme.typography.labelSmall,
                        color = TextMuted,
                    )
                }
            }
        }
    }
}

private fun androidx.compose.ui.graphics.drawscope.DrawScope.drawSeries(
    values: List<Int>,
    width: Float,
    height: Float,
    maxValue: Int,
    color: Color,
) {
    if (values.isEmpty()) return
    val path = Path()
    val fillPath = Path()
    val stepX = if (values.size <= 1) width else width / (values.size - 1)
    values.forEachIndexed { index, value ->
        val x = stepX * index
        val y = height - ((value.toFloat() / maxValue.toFloat()) * (height - 8.dp.toPx()))
        if (index == 0) {
            path.moveTo(x, y)
            fillPath.moveTo(x, height)
            fillPath.lineTo(x, y)
        } else {
            path.lineTo(x, y)
            fillPath.lineTo(x, y)
        }
    }
    fillPath.lineTo(width, height)
    fillPath.close()
    drawPath(
        path = fillPath,
        brush = Brush.verticalGradient(listOf(color.copy(alpha = 0.18f), Color.Transparent)),
    )
    drawPath(path = path, color = color, style = Stroke(width = 2.dp.toPx(), cap = StrokeCap.Round))
}

private fun stateColor(key: String): Color = when (key.lowercase()) {
    "working" -> Emerald
    "thinking" -> Cyan
    "waiting" -> Amber
    else -> PanelMuted
}

private fun providerColor(key: String): Color = when (key.lowercase()) {
    "claude" -> Violet
    "codex-fork", "codex" -> Cyan
    else -> Fuchsia
}

private fun repoColor(label: String): Color {
    val palette = listOf(Cyan, Emerald, Amber, Violet, Fuchsia)
    return palette[(label.hashCode().absoluteValue) % palette.size]
}

private fun healthStatusColor(status: String?): Color = when ((status ?: "").lowercase()) {
    "ok" -> Emerald
    "warning" -> Amber
    else -> Rose
}

private fun compactNumber(value: Int): String = when {
    value >= 1_000_000 -> String.format("%.1fm", value / 1_000_000f)
    value >= 1_000 -> String.format("%.1fk", value / 1_000f)
    else -> value.toString()
}

private fun compactTokenCount(value: Int): String = when {
    value >= 1_000_000 -> String.format("%.1fM tok", value / 1_000_000f)
    value >= 1_000 -> String.format("%.1fk tok", value / 1_000f)
    else -> "$value tok"
}

private fun formatDelta(delta: Float): String = if (delta >= 0f) "+${delta.toInt()}%" else "${delta.toInt()}%"

private fun formatShare(share: Float?): String = share?.let { String.format("%.0f%%", it) } ?: "-"

private fun formatGeneratedAt(value: String): String = runCatching {
    OffsetDateTime.parse(value).toLocalTime().withSecond(0).withNano(0).toString()
}.getOrElse { value }

private fun detailTitle(section: String): String = when (section) {
    "throughput" -> "Throughput Detail"
    "reliability" -> "Reliability Detail"
    "repos" -> "Repository Contribution"
    "providers" -> "Provider Split"
    "load" -> "Load Detail"
    "longest" -> "Longest Running"
    else -> "Analytics Detail"
}

private fun countUnhealthyChecks(checks: List<AnalyticsHealthCheck>): Int =
    checks.count { (it.status ?: "").lowercase() !in setOf("ok", "warning") }
