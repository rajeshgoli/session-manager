package li.rajeshgo.sm.ui.history

import android.widget.Toast
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
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
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material.icons.rounded.ExpandLess
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.material.icons.rounded.Restore
import androidx.compose.material.icons.rounded.Visibility
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.OffsetDateTime
import li.rajeshgo.sm.data.model.AgentHistoryRow
import li.rajeshgo.sm.data.model.AgentWorkDoc
import li.rajeshgo.sm.data.model.AgentWorkItem
import li.rajeshgo.sm.ui.guestbook.signedLabel
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.navigation.AppTopBar
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet
import li.rajeshgo.sm.ui.watch.DocReaderOverlay
import li.rajeshgo.sm.ui.watch.ReaderPage
import li.rajeshgo.sm.ui.watch.ownerReaderPage

/** Fetch the next page when this many agents remain below the last visible one. */
private const val PREFETCH_ROWS = 3

/**
 * History (sm#1661): agents that are no longer live, newest first, to bring
 * back like `sm watch --restore`. Each card restores its agent and expands
 * to the tickets, PRs and docs it worked on, which open in the reader.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HistoryScreen(
    onBack: () -> Unit,
    onOpenWatch: () -> Unit,
    menu: AppMenuActions,
    viewModel: HistoryViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val context = LocalContext.current
    val listState = rememberLazyListState()
    var openPage by remember { mutableStateOf<ReaderPage?>(null) }
    val now = remember(state.agents) { OffsetDateTime.now() }

    LaunchedEffect(Unit) { if (state.agents.isEmpty()) viewModel.refresh() }
    val nearEnd by remember {
        derivedStateOf {
            val last = listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0
            last >= listState.layoutInfo.totalItemsCount - 1 - PREFETCH_ROWS
        }
    }
    LaunchedEffect(nearEnd, state.nextBefore, state.loading) {
        if (nearEnd && !state.loading) viewModel.loadMore()
    }
    LaunchedEffect(state.query) { listState.scrollToItem(0) }

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
                state = listState,
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(16.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                item(key = "top") {
                    AppTopBar(
                        title = "History",
                        menu = menu,
                        subtitle = when {
                            state.loading && state.agents.isEmpty() -> "Agents that are no longer live"
                            state.query != null -> "${state.total} matching \"${state.query}\""
                            else -> "${state.total} agents no longer live"
                        },
                        busy = state.refreshing,
                        current = Routes.HISTORY,
                        onBack = onBack,
                        onRefresh = { viewModel.refresh(pull = true) },
                    )
                }
                item(key = "search") {
                    SearchField(query = state.query, onQuery = viewModel::setQuery)
                    if (state.signedOut) Text("Sign in to load History", color = Rose)
                    if (state.agents.isEmpty()) {
                        state.error?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall) }
                    }
                }
                if (state.loading && state.agents.isEmpty()) {
                    item(key = "loading") { Spinner() }
                } else if (state.agents.isEmpty() && !state.signedOut && state.error == null) {
                    item(key = "empty") {
                        Text(
                            if (state.query != null) "No agents match \"${state.query}\"." else "Every agent is live.",
                            color = TextMuted,
                            modifier = Modifier.padding(vertical = 24.dp),
                        )
                    }
                }
                items(state.agents, key = { it.id }) { agent ->
                    AgentCard(
                        agent = agent,
                        now = now,
                        expanded = agent.id in state.expanded,
                        restore = state.restores[agent.id],
                        onToggle = { viewModel.toggleExpanded(agent.id) },
                        onRestore = { viewModel.restore(agent.id) },
                        onOpenWatch = onOpenWatch,
                        onOpenPage = { openPage = it },
                    )
                }
                if (state.agents.isNotEmpty()) {
                    item(key = "footer") {
                        when {
                            state.loadingMore -> Spinner()
                            state.error != null -> Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
                                Text(state.error.orEmpty(), color = Amber, style = MaterialTheme.typography.bodySmall)
                                TextButton(onClick = viewModel::retry) { Text("Retry") }
                            }
                            state.nextBefore == null -> Text(
                                "The oldest agent.",
                                color = TextMuted,
                                style = MaterialTheme.typography.labelSmall,
                                modifier = Modifier.fillMaxWidth().padding(vertical = 12.dp),
                            )
                        }
                    }
                }
            }
        }

        openPage?.let { page ->
            key(page) {
                DocReaderOverlay(
                    page = page,
                    loadAuth = viewModel::docReaderAuth,
                    onClose = { openPage = null },
                    onCopyLink = { link ->
                        val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                        clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm link", link))
                        Toast.makeText(context, "Link copied", Toast.LENGTH_SHORT).show()
                    },
                )
            }
        }
    }
}

@Composable
private fun Spinner() {
    Box(Modifier.fillMaxWidth().padding(24.dp), contentAlignment = Alignment.Center) {
        CircularProgressIndicator(color = Cyan)
    }
}

/** The search: a field while none is set, the active search as a clearable chip once set. */
@Composable
private fun SearchField(query: String?, onQuery: (String?) -> Unit) {
    if (query != null) {
        Surface(
            shape = RoundedCornerShape(999.dp),
            color = Cyan.copy(alpha = 0.12f),
            border = BorderStroke(1.dp, Cyan.copy(alpha = 0.32f)),
        ) {
            Row(Modifier.padding(start = 12.dp), verticalAlignment = Alignment.CenterVertically) {
                Text("search: $query", color = Cyan, style = MaterialTheme.typography.labelMedium)
                IconButton(onClick = { onQuery(null) }, modifier = Modifier.height(32.dp)) {
                    Icon(Icons.Rounded.Close, contentDescription = "Clear search", tint = Cyan)
                }
            }
        }
        return
    }
    var draft by remember { mutableStateOf("") }
    val focus = LocalFocusManager.current
    OutlinedTextField(
        value = draft,
        onValueChange = { draft = it },
        modifier = Modifier.fillMaxWidth(),
        placeholder = { Text("Search name, repo or role") },
        singleLine = true,
        shape = RoundedCornerShape(14.dp),
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
        keyboardActions = KeyboardActions(onSearch = {
            focus.clearFocus()
            onQuery(draft)
        }),
    )
}

@Composable
private fun AgentCard(
    agent: AgentHistoryRow,
    now: OffsetDateTime,
    expanded: Boolean,
    restore: RestoreState?,
    onToggle: () -> Unit,
    onRestore: () -> Unit,
    onOpenWatch: () -> Unit,
    onOpenPage: (ReaderPage) -> Unit,
) {
    val summary = workSummary(agent.work)
    // The Board and Inbox style (spec 1782 J1): a left edge in the row's colour.
    val edge = if (restore == RestoreState.Restored) Emerald else if (agent.state == "retired") Border else Amber
    Surface(
        shape = RoundedCornerShape(12.dp),
        color = Panel,
        border = BorderStroke(1.dp, if (restore == RestoreState.Restored) Emerald.copy(alpha = 0.5f) else Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            Modifier
                .drawBehind { drawRect(edge, size = androidx.compose.ui.geometry.Size(3.dp.toPx(), size.height)) }
                .padding(start = 3.dp),
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable(enabled = summary != null, onClick = onToggle)
                    .padding(horizontal = 12.dp, vertical = 10.dp),
                verticalArrangement = Arrangement.spacedBy(3.dp),
            ) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        agent.name.ifBlank { agent.id },
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = MaterialTheme.colorScheme.onSurface,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                    Spacer(Modifier.width(8.dp))
                    StateChip(agent.state)
                }
                Text(
                    "${agent.id.take(8)} · ${providerLabel(agent)}",
                    style = MaterialTheme.typography.bodySmall,
                    color = TextSecondary,
                    fontFamily = FontFamily.Monospace,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    listOfNotNull(
                        workingDirLabel(agent.workingDir).takeIf { it.isNotBlank() },
                        agent.role?.takeIf { it.isNotBlank() },
                        "${if (agent.state == "retired") "retired" else "stopped"} ${signedLabel(agent.endedAt, now)}",
                    ).joinToString(" · "),
                    style = MaterialTheme.typography.bodySmall,
                    color = TextMuted,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
                agent.lastStatus?.let {
                    Text(it, style = MaterialTheme.typography.bodySmall, color = Cyan, maxLines = 2, overflow = TextOverflow.Ellipsis)
                }
                if (summary != null) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(summary, style = MaterialTheme.typography.labelMedium, color = Violet)
                        Icon(
                            if (expanded) Icons.Rounded.ExpandLess else Icons.Rounded.ExpandMore,
                            contentDescription = if (expanded) "Hide work" else "Show work",
                            tint = Violet,
                            modifier = Modifier.size(18.dp),
                        )
                    }
                }
            }
            RestoreBar(agent, restore, onRestore, onOpenWatch)
            if (expanded) {
                HorizontalDivider(color = Border)
                WorkList(agent, onOpenPage)
            }
        }
    }
}

@Composable
private fun StateChip(state: String) {
    val tint = if (state == "retired") TextMuted else Amber
    Text(
        state.uppercase(),
        style = MaterialTheme.typography.labelSmall,
        fontWeight = FontWeight.Bold,
        color = tint,
        modifier = Modifier
            .background(tint.copy(alpha = 0.16f), RoundedCornerShape(4.dp))
            .padding(horizontal = 5.dp, vertical = 1.dp),
    )
}

/** Restore, its progress and outcome, or why this agent cannot be restored. */
@Composable
private fun RestoreBar(agent: AgentHistoryRow, restore: RestoreState?, onRestore: () -> Unit, onOpenWatch: () -> Unit) {
    Column(Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, bottom = 10.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        when {
            !agent.restorable -> Text(
                agent.unrestorableReason ?: "Cannot be restored",
                style = MaterialTheme.typography.bodySmall,
                color = TextMuted,
            )
            restore == RestoreState.Restored -> Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Restored.", style = MaterialTheme.typography.bodyMedium, color = Emerald, modifier = Modifier.weight(1f))
                OutlinedButton(onClick = onOpenWatch, border = BorderStroke(1.dp, Emerald.copy(alpha = 0.5f))) {
                    Icon(Icons.Rounded.Visibility, contentDescription = null, tint = Emerald, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text("Open Watch", color = Emerald)
                }
            }
            else -> {
                val restoring = restore == RestoreState.Restoring
                OutlinedButton(
                    onClick = onRestore,
                    enabled = !restoring,
                    border = BorderStroke(1.dp, Cyan.copy(alpha = if (restoring) 0.2f else 0.5f)),
                    colors = ButtonDefaults.outlinedButtonColors(contentColor = Cyan),
                ) {
                    if (restoring) {
                        CircularProgressIndicator(color = Cyan, strokeWidth = 2.dp, modifier = Modifier.size(16.dp))
                    } else {
                        Icon(Icons.Rounded.Restore, contentDescription = null, modifier = Modifier.size(18.dp))
                    }
                    Spacer(Modifier.width(6.dp))
                    Text(if (restoring) "Restoring…" else if (restore is RestoreState.Failed) "Try again" else "Restore")
                }
                (restore as? RestoreState.Failed)?.let {
                    Text(it.message, style = MaterialTheme.typography.bodySmall, color = Rose)
                }
            }
        }
    }
}

@Composable
private fun WorkList(agent: AgentHistoryRow, onOpenPage: (ReaderPage) -> Unit) {
    val withRepo = (agent.work.tickets + agent.work.prs).map { it.repo }.distinct().size > 1
    Column(Modifier.fillMaxWidth().padding(vertical = 6.dp)) {
        agent.work.tickets.forEach { item -> ItemRow(item, if (withRepo) "${workingDirLabel(item.repo)}#" else "#", onOpenPage) }
        agent.work.prs.forEach { item -> ItemRow(item, if (withRepo) "${workingDirLabel(item.repo)} PR #" else "PR #", onOpenPage) }
        agent.work.docs.forEach { doc -> DocRow(doc, onOpenPage) }
    }
}

@Composable
private fun ItemRow(item: AgentWorkItem, prefix: String, onOpenPage: (ReaderPage) -> Unit) {
    val label = "$prefix${item.number}"
    WorkRow(
        label = label,
        title = item.title.ifBlank { item.url },
        state = item.state,
        onClick = { onOpenPage(ownerReaderPage(item.title.ifBlank { label }, item.historyPath)) },
    )
}

@Composable
private fun DocRow(doc: AgentWorkDoc, onOpenPage: (ReaderPage) -> Unit) {
    WorkRow(
        label = "doc",
        title = doc.title.ifBlank { doc.name },
        state = doc.state.replace('_', ' '),
        onClick = { onOpenPage(ReaderPage(title = doc.title.ifBlank { doc.name }, subtitle = doc.name, path = doc.readerPath)) },
    )
}

@Composable
private fun WorkRow(label: String, title: String, state: String, onClick: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 14.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text(label, style = MaterialTheme.typography.labelMedium, color = Cyan, fontFamily = FontFamily.Monospace)
        Text(
            title,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurface,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        if (state.isNotBlank()) Text(state, style = MaterialTheme.typography.labelSmall, color = stateTint(state))
    }
}

private fun stateTint(state: String): Color = when (state) {
    "open" -> Emerald
    "merged" -> Violet
    "review requested" -> Amber
    else -> TextMuted
}
