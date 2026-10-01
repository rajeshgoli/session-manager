package li.rajeshgo.sm.ui.inbox

import android.widget.Toast
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.SwipeToDismissBox
import androidx.compose.material3.SwipeToDismissBoxValue
import androidx.compose.material3.Text
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.material3.rememberSwipeToDismissBoxState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.Duration
import java.time.OffsetDateTime
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import li.rajeshgo.sm.data.model.InboxRow
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.navigation.AppBottomNav
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.navigation.AppTopBar
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.queue.rememberResumed
import li.rajeshgo.sm.ui.queue.shortDuration
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.CyanDeep
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Fuchsia
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet
import li.rajeshgo.sm.ui.watch.DocReaderOverlay
import li.rajeshgo.sm.ui.watch.ReaderPage

private const val INBOX_REFRESH_MS = 30_000L

/** The page an Inbox row opens in the reader. */
fun inboxReaderPage(row: InboxRow): ReaderPage = ReaderPage(
    title = row.title,
    subtitle = if (row.kind == "doc") listOfNotNull(row.repo.ifBlank { null }, row.prNumber?.let { "PR #$it" }).joinToString(" · ")
    else row.repo,
    path = row.url,
)

/** The row's third line: repo, then what kind of thread it is. */
fun inboxRowDetail(row: InboxRow): String {
    val parts = mutableListOf<String>()
    if (row.repo.isNotBlank()) parts += row.repo
    if (row.kind == "doc") {
        row.prNumber?.let { parts += "PR #$it" }
        row.author?.takeIf { it.isNotBlank() }?.let { parts += it }
    } else {
        if (row.messageCount > 0) parts += if (row.messageCount == 1) "1 message" else "${row.messageCount} messages"
        parts += when {
            row.group == "needs_you" -> "asks you"
            row.status == "ended" -> "agent ended"
            row.preview.startsWith("You: ") -> "you replied"
            else -> "for your information"
        }
    }
    return parts.joinToString(" · ")
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun InboxScreen(
    onNavigateToWatch: () -> Unit,
    onNavigateToBoard: () -> Unit,
    onNavigateToQueue: () -> Unit,
    menu: AppMenuActions,
    viewModel: InboxViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val resumed = rememberResumed()
    val context = LocalContext.current
    var openRow by remember { mutableStateOf<InboxRow?>(null) }
    var now by remember { mutableStateOf(OffsetDateTime.now()) }

    // Reload while shown, and when a page opened from here closes.
    LaunchedEffect(resumed, openRow == null) {
        if (!resumed || openRow != null) return@LaunchedEffect
        while (isActive) {
            viewModel.refresh()
            now = OffsetDateTime.now()
            delay(INBOX_REFRESH_MS)
        }
    }
    // A tapped message or review notification opens its page here, so Back lands on the Inbox.
    val pendingOpen = FollowOpenRequests.pending
    LaunchedEffect(pendingOpen) {
        val open = pendingOpen?.takeIf { it.inbox } ?: return@LaunchedEffect
        val path = open.readerPath ?: return@LaunchedEffect
        openRow = InboxRow(
            threadKey = open.sessionId?.let { "agent:$it" }.orEmpty(),
            kind = if (path.startsWith("/docs/")) "doc" else "agent",
            title = open.title.ifBlank { "Inbox" },
            url = path,
            sessionId = open.sessionId,
        )
        FollowOpenRequests.pending = null
    }

    Box(
        modifier = Modifier
            .fillMaxSize().statusBarsPadding().navigationBarsPadding()
            .background(MaterialTheme.colorScheme.background),
    ) {
        PullToRefreshBox(
            isRefreshing = state.refreshing,
            onRefresh = { viewModel.refresh(pull = true) },
            modifier = Modifier.fillMaxSize(),
        ) {
            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 112.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                item {
                    AppTopBar(
                        title = "Inbox",
                        menu = menu,
                        busy = state.refreshing,
                        refreshBar = state.revalidating,
                        current = Routes.INBOX,
                        onRefresh = { viewModel.refresh(pull = true) },
                    )
                }
                item {
                    Row(
                        Modifier.fillMaxWidth().padding(top = 4.dp),
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        InboxFilter.entries.forEach { filter ->
                            FilterChip(filter.label, selected = filter == state.filter) { viewModel.setFilter(filter) }
                        }
                    }
                    state.error?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall) }
                    if (state.signedOut) Text("Sign in to load the Inbox", color = Rose)
                }
                val sections = inboxSections(state.filter, state.rows)
                if (state.loading && state.rows.isEmpty()) {
                    item {
                        Box(Modifier.fillMaxWidth().padding(32.dp), contentAlignment = Alignment.Center) {
                            CircularProgressIndicator(color = Cyan)
                        }
                    }
                } else if (sections.isEmpty() && !state.signedOut) {
                    item {
                        Text(
                            when (state.filter) {
                                InboxFilter.Open -> "Nothing waiting on you."
                                InboxFilter.Docs -> "No docs yet."
                                InboxFilter.Done -> "Nothing marked Done."
                            },
                            color = TextMuted,
                            modifier = Modifier.padding(vertical = 24.dp),
                        )
                    }
                }
                sections.forEach { (heading, rows) ->
                    if (heading != null) {
                        item(key = "h-$heading") {
                            Text(
                                heading,
                                style = MaterialTheme.typography.labelSmall,
                                fontWeight = FontWeight.Bold,
                                color = when {
                                    heading.startsWith("NEEDS") -> Amber
                                    heading.startsWith("NEW") -> Fuchsia
                                    else -> TextMuted
                                },
                                modifier = Modifier.padding(top = 10.dp, start = 2.dp),
                            )
                        }
                    }
                    items(rows, key = { it.threadKey }) { row ->
                        val open = { openRow = row }
                        if (row.done) {
                            InboxRowCard(row, now, onClick = open)
                        } else {
                            key(row.threadKey) {
                                SwipeToDone(onDone = {
                                    viewModel.markDone(row) { error ->
                                        if (error != null) Toast.makeText(context, error, Toast.LENGTH_SHORT).show()
                                    }
                                }) { InboxRowCard(row, now, onClick = open) }
                            }
                        }
                    }
                }
            }
        }

        Box(modifier = Modifier.align(Alignment.BottomCenter).padding(horizontal = 16.dp, vertical = 16.dp)) {
            AppBottomNav(
                currentRoute = Routes.INBOX,
                onInbox = {},
                onWatch = onNavigateToWatch,
                onBoard = onNavigateToBoard,
                onQueue = onNavigateToQueue,
            )
        }

        openRow?.let { row ->
            val page = inboxReaderPage(row)
            key(page) {
                DocReaderOverlay(
                    page = page,
                    loadAuth = viewModel::docReaderAuth,
                    onClose = { openRow = null },
                    onCopyLink = { link ->
                        val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                        clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm link", link))
                        Toast.makeText(context, "Link copied", Toast.LENGTH_SHORT).show()
                    },
                    actions = row.sessionId?.takeIf { row.kind == "agent" }?.let { sessionId ->
                        {
                            OutlinedButton(
                                onClick = {
                                    openRow = null
                                    FollowOpenRequests.pending = FollowOpen(sessionId, null, row.title)
                                    onNavigateToWatch()
                                },
                                modifier = Modifier.height(40.dp),
                                contentPadding = PaddingValues(horizontal = 10.dp, vertical = 2.dp),
                                shape = RoundedCornerShape(10.dp),
                            ) { Text("Agent") }
                        }
                    },
                )
            }
        }
    }
}

@Composable
private fun FilterChip(label: String, selected: Boolean, onClick: () -> Unit) {
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(999.dp),
        color = if (selected) CyanDeep else Color.Transparent,
        border = BorderStroke(1.dp, if (selected) CyanDeep else Border),
        modifier = Modifier.padding(start = 6.dp),
    ) {
        Text(
            label,
            color = if (selected) Cyan else TextSecondary,
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 5.dp),
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SwipeToDone(onDone: () -> Unit, content: @Composable () -> Unit) {
    // The swipe never settles dismissed: on Open the row leaves with the list update,
    // and a failed Done (or a Docs row, which stays listed) springs back.
    // One Done per swipe: the callback can repeat while the swipe settles.
    var sent by remember { mutableStateOf(false) }
    val dismissState = rememberSwipeToDismissBoxState(
        confirmValueChange = { value ->
            if (value == SwipeToDismissBoxValue.StartToEnd && !sent) {
                sent = true
                onDone()
            }
            false
        },
    )
    // A row still listed a moment later (Docs, or Done failed) can be swiped again.
    LaunchedEffect(sent) {
        if (sent) {
            delay(1_000)
            sent = false
        }
    }
    SwipeToDismissBox(
        state = dismissState,
        enableDismissFromEndToStart = false,
        backgroundContent = {
            Box(
                Modifier.fillMaxSize().background(CyanDeep, RoundedCornerShape(10.dp)).padding(start = 18.dp),
                contentAlignment = Alignment.CenterStart,
            ) { Text("Done", color = Cyan, fontWeight = FontWeight.Bold) }
        },
    ) { content() }
}

@Composable
private fun InboxRowCard(row: InboxRow, now: OffsetDateTime, onClick: () -> Unit) {
    val accent = when (row.group) {
        "needs_you" -> Amber
        "new" -> Fuchsia
        else -> Border
    }
    Surface(
        onClick = onClick,
        color = Panel,
        shape = RoundedCornerShape(10.dp),
        border = BorderStroke(1.dp, if (row.group == "needs_you") Amber.copy(alpha = 0.55f) else Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Row(Modifier.padding(12.dp), verticalAlignment = Alignment.Top) {
            Box(
                Modifier.padding(top = 6.dp, end = 10.dp).size(8.dp)
                    .background(if (row.group == "earlier") Color.Transparent else accent, CircleShape),
            )
            Column(Modifier.weight(1f)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (row.kind == "doc") {
                        Text(
                            "DOC",
                            color = Violet,
                            style = MaterialTheme.typography.labelSmall,
                            fontWeight = FontWeight.Bold,
                            modifier = Modifier
                                .background(Violet.copy(alpha = 0.18f), RoundedCornerShape(4.dp))
                                .padding(horizontal = 5.dp, vertical = 1.dp),
                        )
                        Box(Modifier.width(6.dp))
                    }
                    Text(
                        row.title,
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = if (row.group == "earlier") FontWeight.Normal else FontWeight.Bold,
                        color = if (row.group == "earlier") TextSecondary else MaterialTheme.colorScheme.onSurface,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                    Text(age(row.newestAt, now), style = MaterialTheme.typography.labelSmall, color = TextMuted)
                }
                Text(
                    row.preview,
                    style = MaterialTheme.typography.bodyMedium,
                    color = when {
                        row.preview.startsWith("Finished") || row.verdict == "approve" -> Emerald
                        row.group == "earlier" -> TextSecondary
                        else -> MaterialTheme.colorScheme.onSurface
                    },
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(top = 2.dp),
                )
                Text(
                    inboxRowDetail(row),
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = FontFamily.Monospace,
                    color = if (row.kind == "doc" && row.group == "needs_you") Violet else TextMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(top = 3.dp),
                )
            }
        }
    }
}

private fun age(timestamp: String, now: OffsetDateTime): String =
    runCatching { shortDuration(Duration.between(OffsetDateTime.parse(timestamp), now).seconds) }.getOrDefault("")
