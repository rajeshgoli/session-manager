package li.rajeshgo.sm.ui.watch

import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.OpenInNew
import androidx.compose.material.icons.rounded.ContentCopy
import androidx.compose.material.icons.rounded.Campaign
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.Settings
import androidx.compose.material.icons.rounded.Terminal
import androidx.compose.material.icons.rounded.UnfoldLess
import androidx.compose.material.icons.rounded.UnfoldMore
import androidx.compose.material3.AssistChip
import androidx.compose.material3.AssistChipDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlin.coroutines.coroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.ui.navigation.AppBottomNav
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.BorderStrong
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
import li.rajeshgo.sm.util.launchTermuxAttach
import li.rajeshgo.sm.util.termuxAttachCommand

private const val WATCH_AUTO_REFRESH_MS = 5000L

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun WatchScreen(
    onNavigateToSettings: () -> Unit,
    onNavigateToAnalytics: () -> Unit,
    viewModel: WatchViewModel = viewModel(),
    updateViewModel: UpdateAvailabilityViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val updateState by updateViewModel.uiState.collectAsState()
    var query by remember { mutableStateOf("") }
    var filter by remember { mutableStateOf("all") }
    var toast by remember { mutableStateOf<String?>(null) }

    val sections = remember(state.sessions, filter, query) {
        filterSections(buildSections(state.sessions), filter, query)
    }
    val activeSections = remember(sections) { sliceSections(sections, TreeSlice.Active) }
    val idleSections = remember(sections) { sliceSections(sections, TreeSlice.Idle) }
    val sessionsById = remember(state.sessions) { state.sessions.associateBy { it.id } }
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    var isResumed by remember {
        mutableStateOf(lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED))
    }

    androidx.compose.runtime.DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, _ ->
            isResumed = lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
        }
    }

    LaunchedEffect(isResumed, state.serverUrl, state.userEmail) {
        if (!isResumed || state.serverUrl.isBlank()) {
            return@LaunchedEffect
        }
        viewModel.refresh()
        updateViewModel.refresh()
        while (coroutineContext.isActive) {
            delay(WATCH_AUTO_REFRESH_MS)
            viewModel.refresh()
        }
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background),
    ) {
        if (state.loading) {
            Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(color = Cyan)
            }
            return@Box
        }

        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(start = 16.dp, top = 16.dp, end = 16.dp, bottom = 128.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item {
                HeaderBar(
                    userEmail = state.userEmail,
                    lastSync = state.lastSync,
                    refreshing = state.refreshing,
                    requestingStatus = state.requestingStatus,
                    hasUpdate = updateState.availableUpdate != null,
                    onRefresh = { viewModel.refresh() },
                    onRequestStatus = {
                        viewModel.requestStatus { result ->
                            toast = result.exceptionOrNull()?.message ?: result.getOrNull()
                        }
                    },
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

            if (state.error != null) {
                item {
                    Text(
                        text = state.error.orEmpty(),
                        color = Rose,
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }

            if (sections.isEmpty()) {
                item {
                    EmptyState(query = query, filter = filter)
                }
            } else {
                activeSections.forEach { section ->
                    item(key = "repo-${section.repoKey}") {
                        RepoHeader(title = "${section.repoLabel} (${section.repoKey})")
                    }
                    items(section.roots, key = { "active-${it.session.id}" }) { root ->
                        WatchTree(
                            node = root,
                            depth = 0,
                            slice = TreeSlice.Active,
                            sessionsById = sessionsById,
                            expandedSessionIds = state.expandedSessionIds,
                            detailsById = state.detailsBySessionId,
                            onToggleExpanded = { viewModel.toggleExpanded(it) },
                            onOpenAttach = { session ->
                                val attach = session.termuxAttach
                                if (attach == null) {
                                    toast = "Attach metadata unavailable"
                                } else {
                                    launchTermuxAttach(context, attach)
                                        .onSuccess { toast = "Opening Termux for ${sessionDisplayName(session)}" }
                                        .onFailure { error -> toast = error.message ?: "Attach failed" }
                                }
                            },
                            onCopyAttach = { session ->
                                val command = session.termuxAttach?.let(::termuxAttachCommand)
                                if (command == null) {
                                    toast = "Attach command unavailable"
                                } else {
                                    val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                                    clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm attach", command))
                                    toast = "Attach command copied"
                                }
                            },
                            onOpenTelegram = { session ->
                                val link = telegramLink(session)
                                if (link == null) {
                                    toast = "Telegram thread unavailable"
                                } else {
                                    runCatching {
                                        context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(link)).apply {
                                            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                                        })
                                    }.onFailure { error ->
                                        toast = error.message ?: "Unable to open Telegram"
                                    }
                                }
                            },
                            onKill = { session ->
                                viewModel.killSession(session.id) { result ->
                                    toast = result.exceptionOrNull()?.message ?: "Killed ${session.id}"
                                }
                            },
                        )
                    }
                }
                idleSections.forEach { section ->
                    item(key = "idle-repo-${section.repoKey}") {
                        RepoHeader(title = "${section.repoLabel} (${section.repoKey})")
                    }
                    items(section.roots, key = { "idle-${it.session.id}" }) { root ->
                        WatchTree(
                            node = root,
                            depth = 0,
                            slice = TreeSlice.Idle,
                            sessionsById = sessionsById,
                            expandedSessionIds = state.expandedSessionIds,
                            detailsById = state.detailsBySessionId,
                            onToggleExpanded = { viewModel.toggleExpanded(it) },
                            onOpenAttach = { session ->
                                val attach = session.termuxAttach
                                if (attach == null) {
                                    toast = "Attach metadata unavailable"
                                } else {
                                    launchTermuxAttach(context, attach)
                                        .onSuccess { toast = "Opening Termux for ${sessionDisplayName(session)}" }
                                        .onFailure { error -> toast = error.message ?: "Attach failed" }
                                }
                            },
                            onCopyAttach = { session ->
                                val command = session.termuxAttach?.let(::termuxAttachCommand)
                                if (command == null) {
                                    toast = "Attach command unavailable"
                                } else {
                                    val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                                    clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm attach", command))
                                    toast = "Attach command copied"
                                }
                            },
                            onOpenTelegram = { session ->
                                val link = telegramLink(session)
                                if (link == null) {
                                    toast = "Telegram thread unavailable"
                                } else {
                                    runCatching {
                                        context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(link)).apply {
                                            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                                        })
                                    }.onFailure { error ->
                                        toast = error.message ?: "Unable to open Telegram"
                                    }
                                }
                            },
                            onKill = { session ->
                                viewModel.killSession(session.id) { result ->
                                    toast = result.exceptionOrNull()?.message ?: "Killed ${session.id}"
                                }
                            },
                        )
                    }
                }
            }

            item {
                FooterControls(
                    sessions = state.sessions,
                    query = query,
                    filter = filter,
                    onQueryChange = { query = it },
                    onFilterChange = { filter = it },
                )
            }
        }

        Box(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(horizontal = 16.dp, vertical = 16.dp),
        ) {
            AppBottomNav(
                currentRoute = Routes.WATCH,
                onWatch = {},
                onAnalytics = onNavigateToAnalytics,
            )
        }

        if (toast != null) {
            Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.BottomCenter) {
                Surface(
                    modifier = Modifier.padding(start = 16.dp, end = 16.dp, bottom = 88.dp),
                    shape = RoundedCornerShape(999.dp),
                    color = PanelElevated,
                    border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong),
                ) {
                    Text(
                        text = toast.orEmpty(),
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 10.dp),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurface,
                    )
                }
            }
        }
    }
}

@Composable
private fun HeaderBar(
    userEmail: String,
    lastSync: String?,
    refreshing: Boolean,
    requestingStatus: Boolean,
    hasUpdate: Boolean,
    onRefresh: () -> Unit,
    onRequestStatus: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    Surface(
        shape = RoundedCornerShape(18.dp),
        color = Panel,
        border = androidx.compose.foundation.BorderStroke(1.dp, Border),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 10.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column {
                Text("sm watch", style = MaterialTheme.typography.titleLarge, color = MaterialTheme.colorScheme.onSurface)
                val statusLine = buildString {
                    append("Last sync ")
                    append(formatDateTime(lastSync))
                    if (userEmail.isNotBlank()) {
                        append(" • ")
                        append(userEmail)
                    }
                }
                Spacer(Modifier.height(2.dp))
                Text(
                    text = statusLine,
                    style = MaterialTheme.typography.labelSmall,
                    color = if (refreshing) Cyan else TextMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                IconButton(onClick = onRequestStatus) {
                    Icon(
                        Icons.Rounded.Campaign,
                        contentDescription = "Request status",
                        tint = if (requestingStatus) Cyan else TextSecondary,
                    )
                }
                IconButton(onClick = onRefresh) {
                    Icon(Icons.Rounded.Refresh, contentDescription = "Refresh", tint = if (refreshing) Cyan else TextSecondary)
                }
                SettingsIconButtonWithUpdate(hasUpdate = hasUpdate, onClick = onOpenSettings)
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun FooterControls(
    sessions: List<ClientSession>,
    query: String,
    filter: String,
    onQueryChange: (String) -> Unit,
    onFilterChange: (String) -> Unit,
) {
    val running = sessions.count { it.status == "running" }
    val working = sessions.count { it.activityState == "working" }
    val thinking = sessions.count { it.activityState == "thinking" }
    val maintainers = sessions.count { it.isMaintainer }

    Surface(
        shape = RoundedCornerShape(20.dp),
        color = Panel,
        tonalElevation = 0.dp,
        shadowElevation = 0.dp,
        border = androidx.compose.foundation.BorderStroke(1.dp, Border),
    ) {
        Column(modifier = Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            OutlinedTextField(
                value = query,
                onValueChange = onQueryChange,
                modifier = Modifier.fillMaxWidth(),
                label = { Text("Search") },
                placeholder = { Text("name, id, role, alias, worktree") },
                singleLine = true,
            )
            FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                listOf("all", "running", "idle", "stopped").forEach { candidate ->
                    val selected = candidate == filter
                    AssistChip(
                        onClick = { onFilterChange(candidate) },
                        label = { Text(candidate.uppercase()) },
                        colors = AssistChipDefaults.assistChipColors(
                            containerColor = if (selected) Cyan.copy(alpha = 0.18f) else PanelMuted,
                            labelColor = if (selected) Color.White else TextSecondary,
                        ),
                    )
                }
            }
            FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                SummaryBadge(label = "${sessions.size} sessions", tint = MaterialTheme.colorScheme.onSurface)
                SummaryBadge(label = "$running running", tint = Emerald)
                SummaryBadge(label = "$working working", tint = Emerald)
                SummaryBadge(label = "$thinking thinking", tint = Cyan)
                if (maintainers > 0) {
                    SummaryBadge(label = "$maintainers maintainer", tint = Violet)
                }
            }
        }
    }
}

@Composable
private fun SummaryBadge(label: String, tint: Color) {
    Surface(
        shape = RoundedCornerShape(999.dp),
        color = tint.copy(alpha = 0.12f),
        border = androidx.compose.foundation.BorderStroke(1.dp, tint.copy(alpha = 0.24f)),
    ) {
        Text(
            text = label,
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp),
            style = MaterialTheme.typography.labelSmall,
            color = tint,
            fontWeight = FontWeight.Medium,
        )
    }
}

@Composable
private fun RepoHeader(title: String) {
    Text(
        text = title,
        style = MaterialTheme.typography.labelSmall,
        color = Cyan,
        fontFamily = FontFamily.Monospace,
    )
}

@Composable
private fun WatchTree(
    node: WatchSessionNode,
    depth: Int,
    slice: TreeSlice,
    sessionsById: Map<String, ClientSession>,
    expandedSessionIds: Set<String>,
    detailsById: Map<String, SessionDetail>,
    onToggleExpanded: (ClientSession) -> Unit,
    onOpenAttach: (ClientSession) -> Unit,
    onCopyAttach: (ClientSession) -> Unit,
    onOpenTelegram: (ClientSession) -> Unit,
    onKill: (ClientSession) -> Unit,
) {
    if (!nodeMatchesSlice(node, slice)) {
        return
    }

    val renderNode = shouldRenderNode(node, slice)
    if (renderNode) {
        SessionRow(
            session = node.session,
            depth = depth,
            parentLabel = parentLabel(node.session, sessionsById),
            expanded = expandedSessionIds.contains(node.session.id),
            detail = detailsById[node.session.id],
            onToggleExpanded = { onToggleExpanded(node.session) },
            onOpenAttach = { onOpenAttach(node.session) },
            onCopyAttach = { onCopyAttach(node.session) },
            onOpenTelegram = { onOpenTelegram(node.session) },
            onKill = { onKill(node.session) },
        )
    }

    val childDepth = if (renderNode) depth + 1 else depth

    node.sameRepoChildren
        .filter { nodeMatchesSlice(it, slice) }
        .forEach { child ->
            WatchTree(child, childDepth, slice, sessionsById, expandedSessionIds, detailsById, onToggleExpanded, onOpenAttach, onCopyAttach, onOpenTelegram, onKill)
    }

    node.crossRepoGroups.forEach { group ->
        val visibleChildren = group.children.filter { nodeMatchesSlice(it, slice) }
        if (visibleChildren.isEmpty()) {
            return@forEach
        }
        val groupDepth = if (renderNode) depth + 1 else depth
        Text(
            text = "${group.repoLabel} (${group.repoKey})",
            modifier = Modifier.padding(start = (groupDepth * 18).dp, top = 2.dp, bottom = 6.dp),
            style = MaterialTheme.typography.labelSmall,
            color = TextMuted,
            fontFamily = FontFamily.Monospace,
        )
        visibleChildren.forEach { child ->
            WatchTree(child, groupDepth + 1, slice, sessionsById, expandedSessionIds, detailsById, onToggleExpanded, onOpenAttach, onCopyAttach, onOpenTelegram, onKill)
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun SessionRow(
    session: ClientSession,
    depth: Int,
    parentLabel: String,
    expanded: Boolean,
    detail: SessionDetail?,
    onToggleExpanded: () -> Unit,
    onOpenAttach: () -> Unit,
    onCopyAttach: () -> Unit,
    onOpenTelegram: () -> Unit,
    onKill: () -> Unit,
) {
    val attachSupported = session.termuxAttach?.supported == true
    Surface(
        modifier = Modifier.padding(start = (depth * 14).dp),
        shape = RoundedCornerShape(22.dp),
        color = Panel,
        border = androidx.compose.foundation.BorderStroke(1.dp, Border),
    ) {
        Column(modifier = Modifier.fillMaxWidth()) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable { if (attachSupported) onOpenAttach() else onToggleExpanded() }
                    .padding(14.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(modifier = Modifier.weight(1f), verticalAlignment = Alignment.CenterVertically) {
                    Box(modifier = Modifier.size(9.dp).background(statusDot(session), CircleShape))
                    Spacer(Modifier.width(10.dp))
                    Column(modifier = Modifier.weight(1f)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(
                                text = sessionDisplayName(session),
                                style = MaterialTheme.typography.titleMedium,
                                color = MaterialTheme.colorScheme.onSurface,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                            if (session.isMaintainer) {
                                Spacer(Modifier.width(8.dp))
                                InlineBadge("maintainer", Cyan)
                            }
                        }
                        Spacer(Modifier.height(3.dp))
                        Text(
                            text = "${session.id} • ${session.role ?: if (session.isEm) "em" else session.provider ?: "-"}",
                            style = MaterialTheme.typography.bodySmall,
                            color = TextSecondary,
                            fontFamily = FontFamily.Monospace,
                        )
                        Spacer(Modifier.height(6.dp))
                        val secondaryLine = buildString {
                            if (parentLabel != "-") {
                                append("Parent ")
                                append(parentLabel)
                                append(" • ")
                            }
                            append(lastSummary(session))
                            append(" • ")
                            append(formatAge(session.lastActivity, session.activityState))
                        }
                        Text(
                            text = secondaryLine,
                            style = MaterialTheme.typography.bodySmall,
                            color = TextMuted,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                Spacer(Modifier.width(10.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (attachSupported) {
                        IconButton(onClick = onOpenAttach) {
                            Icon(Icons.Rounded.Terminal, contentDescription = "Attach", tint = Emerald)
                        }
                    }
                    IconButton(onClick = onToggleExpanded) {
                        Icon(
                            if (expanded) Icons.Rounded.UnfoldLess else Icons.Rounded.UnfoldMore,
                            contentDescription = if (expanded) "Collapse" else "Expand",
                            tint = TextSecondary,
                        )
                    }
                }
            }

            if (expanded) {
                HorizontalDivider(color = Border)
                Column(modifier = Modifier.fillMaxWidth().padding(14.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        StatusChip(label = activityLabel(session.activityState), tint = activityTint(session.activityState))
                        StatusChip(label = session.status, tint = statusTint(session.status))
                        StatusChip(label = session.provider ?: "claude", tint = providerTint(session.provider))
                        if (session.role != null) StatusChip(label = session.role, tint = Violet)
                    }
                    detailLines(session, detail).forEach { line ->
                        Text(
                            text = line,
                            style = MaterialTheme.typography.bodySmall,
                            color = TextSecondary,
                            fontFamily = FontFamily.Monospace,
                        )
                    }
                    Row(
                        modifier = Modifier.horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        if (attachSupported) {
                            ActionPill(label = "Attach", icon = Icons.Rounded.Terminal, onClick = onOpenAttach)
                            ActionPill(label = "Copy", icon = Icons.Rounded.ContentCopy, onClick = onCopyAttach)
                        }
                        if (telegramLink(session) != null) {
                            ActionPill(label = "TG", icon = Icons.AutoMirrored.Rounded.OpenInNew, onClick = onOpenTelegram, tint = Cyan)
                        }
                        ActionPill(label = "Kill", icon = Icons.Rounded.UnfoldLess, onClick = onKill, tint = Rose)
                    }
                    if (session.termuxAttach?.supported == false) {
                        StatusChip(label = session.termuxAttach.reason ?: "attach unavailable", tint = TextMuted)
                    }
                }
            }
        }
    }
}

@Composable
private fun StatusChip(label: String, tint: Color) {
    Surface(shape = RoundedCornerShape(999.dp), color = tint.copy(alpha = 0.18f), border = androidx.compose.foundation.BorderStroke(1.dp, tint.copy(alpha = 0.32f))) {
        Text(label.uppercase(), modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp), style = MaterialTheme.typography.labelSmall, color = tint)
    }
}

@Composable
private fun ActionPill(label: String, icon: androidx.compose.ui.graphics.vector.ImageVector, onClick: () -> Unit, tint: Color = Emerald) {
    AssistChip(
        onClick = onClick,
        label = { Text(label, maxLines = 1) },
        leadingIcon = { Icon(icon, contentDescription = null, tint = tint) },
        colors = AssistChipDefaults.assistChipColors(containerColor = PanelMuted, labelColor = MaterialTheme.colorScheme.onSurface),
        border = AssistChipDefaults.assistChipBorder(enabled = true, borderColor = tint.copy(alpha = 0.32f)),
    )
}

@Composable
private fun InlineBadge(label: String, tint: Color) {
    Surface(shape = RoundedCornerShape(999.dp), color = tint.copy(alpha = 0.16f)) {
        Text(label.uppercase(), modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp), style = MaterialTheme.typography.labelSmall, color = tint)
    }
}

@Composable
private fun EmptyState(query: String, filter: String) {
    Card(colors = CardDefaults.cardColors(containerColor = Panel), border = androidx.compose.foundation.BorderStroke(1.dp, Border)) {
        Column(modifier = Modifier.fillMaxWidth().padding(32.dp), horizontalAlignment = Alignment.CenterHorizontally) {
            Text("No sessions matched", style = MaterialTheme.typography.titleLarge, color = MaterialTheme.colorScheme.onSurface)
            Spacer(Modifier.height(8.dp))
            Text(
                text = if (query.isNotBlank() || filter != "all") "Adjust the filter or search query." else "Waiting for session-manager to report sessions.",
                style = MaterialTheme.typography.bodyMedium,
                color = TextSecondary,
            )
        }
    }
}

private fun telegramLink(session: ClientSession): String? {
    val chatId = session.telegramChatId ?: return null
    val threadId = session.telegramThreadId ?: return null
    val normalizedChatId = chatId.toString().removePrefix("-").removePrefix("100")
    return "https://t.me/c/$normalizedChatId/$threadId"
}

private enum class TreeSlice {
    Active,
    Idle,
}

private fun nodeMatchesSlice(node: WatchSessionNode, slice: TreeSlice): Boolean {
    return when (slice) {
        TreeSlice.Active -> hasActiveBranch(node)
        TreeSlice.Idle -> hasIdleBranch(node)
    }
}

private fun shouldRenderNode(node: WatchSessionNode, slice: TreeSlice): Boolean {
    return when (slice) {
        TreeSlice.Active -> hasActiveBranch(node)
        TreeSlice.Idle -> !isActiveSession(node.session) && hasIdleBranch(node)
    }
}

private fun sliceSections(sections: List<WatchSection>, slice: TreeSlice): List<WatchSection> {
    return sections.mapNotNull { section ->
        val roots = section.roots
            .filter { nodeMatchesSlice(it, slice) }
            .sortedWith(
                compareByDescending<WatchSessionNode> { nodeSliceFreshness(it, slice) }
                    .thenBy { sessionDisplayName(it.session).lowercase() }
                    .thenBy { it.session.id }
            )
        if (roots.isEmpty()) null else section.copy(roots = roots)
    }.sortedWith(
        compareByDescending<WatchSection> { sectionSliceFreshness(it, slice) }
            .thenBy { it.repoLabel.lowercase() }
            .thenBy { it.repoKey }
    )
}

private fun sectionSliceFreshness(section: WatchSection, slice: TreeSlice): Long {
    return section.roots.maxOfOrNull { nodeSliceFreshness(it, slice) } ?: Long.MIN_VALUE
}

private fun nodeSliceFreshness(node: WatchSessionNode, slice: TreeSlice): Long {
    val ownFreshness = when (slice) {
        TreeSlice.Active -> if (isActiveSession(node.session)) sessionLastActivityEpoch(node.session) else Long.MIN_VALUE
        TreeSlice.Idle -> if (!isActiveSession(node.session)) sessionLastActivityEpoch(node.session) else Long.MIN_VALUE
    }
    val childFreshness = node.sameRepoChildren.maxOfOrNull { nodeSliceFreshness(it, slice) } ?: Long.MIN_VALUE
    val crossRepoFreshness = node.crossRepoGroups
        .flatMap { it.children }
        .maxOfOrNull { nodeSliceFreshness(it, slice) } ?: Long.MIN_VALUE
    return maxOf(ownFreshness, childFreshness, crossRepoFreshness)
}

private fun sessionLastActivityEpoch(session: ClientSession): Long {
    return parseIso(session.lastActivity)?.toEpochSecond() ?: Long.MIN_VALUE
}

private fun statusDot(session: ClientSession): Color = when (session.status) {
    "running" -> Emerald
    "stopped" -> Rose
    else -> TextMuted
}

private fun statusTint(status: String?): Color = when (status) {
    "running" -> Emerald
    "stopped" -> Rose
    else -> TextSecondary
}

private fun activityTint(state: String?): Color = when (activityLabel(state)) {
    "working" -> Emerald
    "thinking" -> Cyan
    "waiting" -> Amber
    "stopped" -> Rose
    else -> TextSecondary
}

private fun providerTint(provider: String?): Color = when (provider) {
    "codex-fork" -> Cyan
    "claude" -> Fuchsia
    "codex-app" -> Violet
    else -> TextSecondary
}
