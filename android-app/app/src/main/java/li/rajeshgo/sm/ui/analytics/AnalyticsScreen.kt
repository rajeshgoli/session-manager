package li.rajeshgo.sm.ui.analytics

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.lazy.rememberLazyListState
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.watch.DocReaderOverlay
import li.rajeshgo.sm.ui.watch.ownerReaderPage
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.OffsetDateTime
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.navigation.AppTopBar
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.queue.HeldBackCard
import li.rajeshgo.sm.ui.queue.JobRow
import li.rajeshgo.sm.ui.queue.QueueJobSheet
import li.rajeshgo.sm.ui.queue.QueueUiState
import li.rajeshgo.sm.ui.queue.QueueViewModel
import li.rajeshgo.sm.ui.queue.SectionHeader
import li.rajeshgo.sm.ui.queue.endedIcon
import li.rajeshgo.sm.ui.queue.endedLine
import li.rajeshgo.sm.ui.queue.heldBackLines
import li.rajeshgo.sm.ui.queue.jobAgentLabel
import li.rajeshgo.sm.ui.queue.jobTitle
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary

/**
 * Analytics: quota spend, agent time and the queue's look-back cards (sm#1662).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AnalyticsScreen(
    section: String?,
    onBack: () -> Unit,
    onOpenUsage: () -> Unit,
    onOpenWatch: () -> Unit,
    menu: AppMenuActions,
    viewModel: AnalyticsViewModel = viewModel(),
    queueViewModel: QueueViewModel = viewModel(),
    spendViewModel: SpendViewModel = viewModel(),
) {
    LaunchedEffect(Unit) { viewModel.open(section) }
    val context = androidx.compose.ui.platform.LocalContext.current
    val current by viewModel.section.collectAsState()
    val queue by queueViewModel.uiState.collectAsState()
    val spend by spendViewModel.state.collectAsState()
    val listState = rememberLazyListState()
    var expanded by remember(current, spend.provider, spend.range, spend.path) { mutableStateOf(false) }
    var historyPath by remember { mutableStateOf<String?>(null) }
    val drillBack = current == AnalyticsSection.SPEND && spend.path.isNotEmpty()
    val back: () -> Unit = { if (drillBack) spendViewModel.back() else onBack() }
    BackHandler(enabled = drillBack && historyPath == null) { spendViewModel.back() }
    LaunchedEffect(current, spend.provider, spend.range, spend.path) { listState.scrollToItem(0) }
    val refreshing = when (current) {
        AnalyticsSection.SPEND -> spend.refreshing
        AnalyticsSection.QUEUE -> queue.refreshing
        else -> false
    }
    var sheetJob by remember { mutableStateOf<SessionJob?>(null) }
    val now = remember(queue.lastUpdated) { OffsetDateTime.now() }

    // Fetch when a section opens and on pull-to-refresh; no timer.
    val refresh: (Boolean) -> Unit = { pull ->
        if (current == AnalyticsSection.SPEND) spendViewModel.refresh(pull)
        if (current == AnalyticsSection.QUEUE) {
            queueViewModel.refresh(pull = pull)
            queueViewModel.refreshStats()
        }
    }
    LaunchedEffect(current) { refresh(false) }

    Box(
        modifier = Modifier
            .fillMaxSize().statusBarsPadding().navigationBarsPadding()
            .background(MaterialTheme.colorScheme.background),
    ) {
        PullToRefreshBox(
            isRefreshing = refreshing,
            onRefresh = { refresh(true) },
            modifier = Modifier.fillMaxSize(),
        ) {
            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                state = listState,
                contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 32.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                item {
                    AppTopBar(
                        title = "Analytics",
                        menu = menu,
                        busy = refreshing,
                        current = Routes.ANALYTICS,
                        onBack = back,
                        onRefresh = { refresh(true) },
                        modifier = Modifier.padding(bottom = 8.dp),
                    )
                }
                item {
                    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                        AnalyticsSection.entries.forEachIndexed { index, entry ->
                            SegmentedButton(
                                selected = entry == current,
                                onClick = { viewModel.select(entry) },
                                shape = SegmentedButtonDefaults.itemShape(index, AnalyticsSection.entries.size),
                                label = { Text(entry.label) },
                            )
                        }
                    }
                }
                when (current) {
                    AnalyticsSection.SPEND -> spendSection(
                        state = spend,
                        expanded = expanded,
                        onExpand = { expanded = true },
                        onSelect = { provider, range -> spendViewModel.select(provider, range) },
                        onRetry = { spendViewModel.refresh(true) },
                        onOpen = spendViewModel::open,
                        onLevel = spendViewModel::toLevel,
                        onAgent = { node ->
                            node.sessionId?.let { id ->
                                FollowOpenRequests.pending = FollowOpen(id, null, node.label)
                                onOpenWatch()
                            }
                        },
                        onHistory = { historyPath = it },
                    )
                    AnalyticsSection.TIME -> item {
                        ComingSoon("What your agents spent their hours on: model, tools, waiting on queue jobs, reviews and you.")
                    }
                    AnalyticsSection.QUEUE -> queueSection(
                        state = queue,
                        now = now,
                        onOpenJob = { job -> sheetJob = job; queueViewModel.loadLog(job) },
                        onHeldBackHours = { queueViewModel.refreshStats(it) },
                        onOpenUsage = onOpenUsage,
                    )
                    null -> Unit
                }
            }
        }
    }

    historyPath?.let { path ->
        Box(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background).statusBarsPadding().navigationBarsPadding()) {
            DocReaderOverlay(
                page = ownerReaderPage("Ticket history", path),
                loadAuth = spendViewModel::docReaderAuth,
                onClose = { historyPath = null },
                onCopyLink = { link ->
                    context.getSystemService(android.content.ClipboardManager::class.java)
                        ?.setPrimaryClip(android.content.ClipData.newPlainText("sm link", link))
                },
            )
        }
    }

    sheetJob?.let { job ->
        QueueJobSheet(
            job = job,
            state = queue,
            now = now,
            viewModel = queueViewModel,
            onOpenWatch = onOpenWatch,
            onClose = { sheetJob = null },
        )
    }
}

@Composable
private fun ComingSoon(what: String) {
    Surface(color = Panel, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth().padding(top = 12.dp)) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text("Coming soon", style = MaterialTheme.typography.titleMedium)
            Text(what, style = MaterialTheme.typography.bodyMedium, color = TextSecondary)
        }
    }
}

/** The look-back cards that left the Queue tab (sm#1677), with a link to Mac usage. */
private fun LazyListScope.queueSection(
    state: QueueUiState,
    now: OffsetDateTime,
    onOpenJob: (SessionJob) -> Unit,
    onHeldBackHours: (Int) -> Unit,
    onOpenUsage: () -> Unit,
) {
    item {
        state.refreshError?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall) }
        if (state.signedOut) Text("Sign in to load the queue", color = Rose)
    }
    val overview = state.overview
    if (overview == null) {
        if (state.loading && !state.signedOut) {
            item {
                Box(Modifier.fillMaxWidth().padding(vertical = 32.dp), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator(color = Cyan)
                }
            }
        }
        return
    }
    item { SectionHeader("STOPPED BY THE QUEUE · LAST 24H") }
    if (overview.ended.isEmpty()) {
        item { Text("Nothing stopped in the last 24 hours.", color = TextMuted, modifier = Modifier.padding(vertical = 12.dp)) }
    }
    items(overview.ended, key = { "e-${it.id}" }) { job ->
        JobRow(
            title = jobTitle(job),
            agent = jobAgentLabel(job),
            line = endedLine(job, now),
            reason = job.endedSummary,
            prefix = endedIcon(job.endedReason),
            prefixColor = if (job.endedReason == "gave_up") Amber else Rose,
            onClick = { onOpenJob(job) },
        )
    }
    item {
        SectionHeader("HELD BACK?")
        HeldBackCard(
            lines = heldBackLines(state.stats),
            hours = state.statsHours,
            onHours = onHeldBackHours,
        )
    }
    item {
        Text(
            "Mac usage over time ›",
            style = MaterialTheme.typography.bodyMedium,
            color = Cyan,
            modifier = Modifier.fillMaxWidth().clickable(onClick = onOpenUsage).padding(vertical = 14.dp, horizontal = 2.dp),
        )
    }
}
