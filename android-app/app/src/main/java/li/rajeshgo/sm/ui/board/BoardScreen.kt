package li.rajeshgo.sm.ui.board

import android.widget.Toast
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.MoreVert
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MenuAnchorType
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.Duration
import java.time.OffsetDateTime
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import li.rajeshgo.sm.data.model.BoardLane
import li.rajeshgo.sm.data.model.BoardRef
import li.rajeshgo.sm.data.model.BoardResponse
import li.rajeshgo.sm.data.model.BoardStartRequest
import li.rajeshgo.sm.data.model.BoardTicket
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
import li.rajeshgo.sm.ui.theme.Orange
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.PanelMuted
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet
import li.rajeshgo.sm.ui.watch.CreateSessionSheet
import li.rajeshgo.sm.ui.watch.DocReaderOverlay
import li.rajeshgo.sm.ui.watch.ReaderPage
import li.rajeshgo.sm.ui.watch.TicketStart

private const val BOARD_REFRESH_MS = 30_000L

/** Items above the first lane in the list: the top bar and the status lines. */
private const val ITEMS_BEFORE_LANES = 2

/** `#N`, or `name#N` when the ticket is in another repo than [base]. */
fun boardShortRef(repo: String, number: Long, base: String): String =
    if (repo.equals(base, ignoreCase = true)) "#$number" else "${repo.substringAfter('/')}#$number"

/** The warning words the web board uses (appendix J2). */
fun boardWarningText(warning: String): String = when (warning) {
    "working_while_blocked" -> "working while blocked"
    "holder_stopped" -> "agent stopped"
    "merged_not_closed" -> "PR merged — close the ticket"
    "cycle" -> "waits in a loop"
    "stale" -> "stale"
    else -> warning.replace('_', ' ')
}

/** Start shows on ready rows, except one whose PR merged: it needs closing, not an agent. */
fun boardCanStart(ticket: BoardTicket): Boolean = ticket.state == "ready" && "merged_not_closed" !in ticket.warnings

/** The folded Blocked line: "#a #b #c …", the first three in row order, NEW after new ones. */
fun blockedPreview(tickets: List<BoardTicket>, base: String): String {
    val shown = tickets.take(3).joinToString(" ") { ticket ->
        boardShortRef(ticket.repo, ticket.number, base) + if (ticket.new) " NEW" else ""
    }
    return if (tickets.size > 3) "$shown …" else shown
}

/** The lane's counts line, as the phone mock shows it. */
fun laneCountsLine(lane: BoardLane): String {
    val c = lane.counts
    val parts = mutableListOf<String>()
    if (c.needsYou > 0) parts += "${c.needsYou} needs you"
    parts += "${c.ready} ready"
    parts += "${c.inProgress} in progress"
    parts += "${c.blocked} blocked"
    if (lane.longestChain.isNotEmpty()) parts += "chain ${lane.longestChain.size}"
    return parts.joinToString(" · ")
}

/** When sm last read GitHub: the newest successful read over every repo. */
private fun lastRead(board: BoardResponse): OffsetDateTime? =
    board.repos.mapNotNull { repo -> repo.lastOkAt?.let { runCatching { OffsetDateTime.parse(it) }.getOrNull() } }.maxOrNull()

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun BoardScreen(
    onNavigateToInbox: () -> Unit,
    onNavigateToWatch: () -> Unit,
    onNavigateToQueue: () -> Unit,
    menu: AppMenuActions,
    viewModel: BoardViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val expanded by viewModel.expanded.collectAsState()
    val resumed = rememberResumed()
    val context = LocalContext.current
    val listState = rememberLazyListState()
    var now by remember { mutableStateOf(OffsetDateTime.now()) }
    var reader by remember { mutableStateOf<ReaderPage?>(null) }
    var addingLane by remember { mutableStateOf(false) }
    var ending by remember { mutableStateOf<BoardLane?>(null) }
    val toast = { text: String -> Toast.makeText(context, text, Toast.LENGTH_SHORT).show() }

    // Read while shown; each read marks the board seen (appendix K).
    LaunchedEffect(resumed, reader == null) {
        if (!resumed || reader != null) return@LaunchedEffect
        while (isActive) {
            viewModel.refresh()
            now = OffsetDateTime.now()
            delay(BOARD_REFRESH_MS)
        }
    }

    val board = state.board
    val showLane = { laneId: Long ->
        val index = board?.lanes?.indexOfFirst { it.id == laneId } ?: -1
        if (index >= 0) {
            viewModel.expand(laneId)
            index
        } else -1
    }
    // A `/board#lane-N` link or a tapped alert scrolls to its lane once the board is here.
    val pendingLane = BoardLinkRequests.pending
    LaunchedEffect(pendingLane, board != null) {
        val laneId = pendingLane ?: return@LaunchedEffect
        if (board == null) return@LaunchedEffect
        BoardLinkRequests.pending = null
        val index = showLane(laneId)
        if (index >= 0) listState.animateScrollToItem(ITEMS_BEFORE_LANES + index)
    }
    var scrollTo by remember { mutableStateOf<Int?>(null) }
    LaunchedEffect(scrollTo) {
        scrollTo?.let { listState.animateScrollToItem(ITEMS_BEFORE_LANES + it) }
        scrollTo = null
    }

    val actions = BoardRowActions(
        onStart = viewModel::openStart,
        onOpenAgent = { ticket ->
            ticket.holder?.let { holder ->
                FollowOpenRequests.pending = FollowOpen(holder.sessionId, null, holder.name)
                onNavigateToWatch()
            }
        },
        onOpenNeedsYou = { ticket ->
            ticket.needsYou?.let { needs ->
                reader = ReaderPage(title = needs.text, subtitle = "#${ticket.number} ${ticket.title}", path = needs.url)
            }
        },
        onShowLane = { laneId -> showLane(laneId).takeIf { it >= 0 }?.let { scrollTo = it } },
    )

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
                contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 112.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                item(key = "top") {
                    AppTopBar(
                        title = "Board",
                        menu = menu,
                        subtitle = board?.let(::lastRead)?.let { "Read from GitHub ${shortDuration(Duration.between(it, now).seconds)} ago" },
                        busy = state.refreshing,
                        current = Routes.BOARD,
                        onRefresh = { viewModel.refresh(pull = true) },
                        actions = {
                            IconButton(onClick = { addingLane = true }, enabled = board != null && !state.busy) {
                                Icon(Icons.Rounded.Add, contentDescription = "Add lane", tint = TextSecondary)
                            }
                        },
                    )
                }
                item(key = "status") {
                    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        state.error?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall) }
                        if (state.signedOut) Text("Sign in to load the board", color = Rose)
                        board?.repos?.filter { it.stale }?.forEach { repo ->
                            Text(
                                "Stale: ${repo.repo} — ${repo.error.orEmpty()}",
                                color = Amber,
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                        if (state.loading && board == null) {
                            Box(Modifier.fillMaxWidth().padding(32.dp), contentAlignment = Alignment.Center) {
                                CircularProgressIndicator(color = Cyan)
                            }
                        } else if (board != null && board.lanes.isEmpty()) {
                            Text("No lanes yet. Add a goal ticket with +.", color = TextMuted, modifier = Modifier.padding(vertical = 16.dp))
                        }
                    }
                }
                board?.lanes?.let { lanes ->
                    lanes.forEachIndexed { index, lane ->
                        item(key = "lane-${lane.id}") {
                            LaneCard(
                                lane = lane,
                                expanded = expanded[lane.id] ?: (lane.rank == 1),
                                canMoveUp = index > 0,
                                canMoveDown = index < lanes.lastIndex,
                                busy = state.busy,
                                actions = actions,
                                onToggle = { viewModel.toggle(lane.id, lane.rank) },
                                onMove = { delta -> viewModel.move(lane.id, delta, toast) },
                                onEnd = { ending = lane },
                            )
                        }
                    }
                }
                board?.other?.takeIf { groups -> groups.any { it.tickets.isNotEmpty() } }?.let { groups ->
                    item(key = "other") { OtherTickets(groups, actions) }
                }
            }
        }

        Box(modifier = Modifier.align(Alignment.BottomCenter).padding(horizontal = 16.dp, vertical = 16.dp)) {
            AppBottomNav(
                currentRoute = Routes.BOARD,
                onInbox = onNavigateToInbox,
                onWatch = onNavigateToWatch,
                onBoard = {},
                onQueue = onNavigateToQueue,
            )
        }

        reader?.let { page ->
            key(page) {
                DocReaderOverlay(
                    page = page,
                    loadAuth = viewModel::docReaderAuth,
                    onClose = { reader = null },
                    onCopyLink = { link ->
                        val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                        clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm link", link))
                        toast("Link copied")
                    },
                )
            }
        }
    }

    if (addingLane && board != null) {
        AddLaneDialog(
            repos = board.repos.map { it.repo },
            busy = state.busy,
            onDismiss = { addingLane = false },
            onAdd = { repo, number ->
                viewModel.addLane(repo, number) { error ->
                    if (error == null) addingLane = false else toast(error)
                }
            },
        )
    }

    ending?.let { lane ->
        AlertDialog(
            onDismissRequest = { ending = null },
            title = { Text("End lane ${lane.rank}?") },
            text = { Text("#${lane.goal.number} ${lane.goal.title} leaves the board, and the lanes below move up. Nothing changes on GitHub.") },
            confirmButton = {
                TextButton(onClick = { ending = null; viewModel.endLane(lane.id, toast) }) { Text("End lane", color = Rose) }
            },
            dismissButton = { TextButton(onClick = { ending = null }) { Text("Cancel") } },
        )
    }

    state.start?.let { start ->
        val options = start.options
        if (options == null) {
            ModalBottomSheet(
                onDismissRequest = viewModel::closeStart,
                sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
            ) {
                Column(Modifier.fillMaxWidth().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text("Start", style = MaterialTheme.typography.headlineSmall)
                    Text("#${start.ticket.number} ${start.ticket.title}", style = MaterialTheme.typography.bodyMedium)
                    if (start.error != null) Text(start.error, color = MaterialTheme.colorScheme.error)
                    else CircularProgressIndicator(color = Cyan, modifier = Modifier.size(24.dp))
                }
            }
        } else {
            val defaults = board?.startDefaults
            CreateSessionSheet(
                source = null,
                sessions = emptyList(),
                loadModels = viewModel::sessionModels,
                busy = start.busy,
                error = start.error,
                onDismiss = viewModel::closeStart,
                ticket = TicketStart(
                    label = "#${start.ticket.number} ${start.ticket.title}",
                    workingDir = options.workingDir,
                    name = options.name,
                    brief = options.brief,
                    provider = defaults?.provider ?: "claude",
                    model = defaults?.model,
                    effort = defaults?.reasoningEffort ?: "high",
                ),
            ) { request ->
                viewModel.start(
                    BoardStartRequest(
                        repo = start.ticket.repo,
                        number = start.ticket.number,
                        provider = request.provider,
                        model = request.model,
                        reasoningEffort = request.reasoningEffort,
                        name = request.name,
                        brief = request.initialMessage,
                    ),
                ) { name -> toast("Started $name") }
            }
        }
    }
}

/** What a ticket row can open. */
private class BoardRowActions(
    val onStart: (BoardTicket) -> Unit,
    val onOpenAgent: (BoardTicket) -> Unit,
    val onOpenNeedsYou: (BoardTicket) -> Unit,
    val onShowLane: (Long) -> Unit,
)

@Composable
private fun LaneCard(
    lane: BoardLane,
    expanded: Boolean,
    canMoveUp: Boolean,
    canMoveDown: Boolean,
    busy: Boolean,
    actions: BoardRowActions,
    onToggle: () -> Unit,
    onMove: (Int) -> Unit,
    onEnd: () -> Unit,
) {
    Surface(
        color = Panel,
        shape = RoundedCornerShape(12.dp),
        border = BorderStroke(1.dp, if (lane.unseen) Orange.copy(alpha = 0.6f) else Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        // The orange edge marks a lane with an alert not yet seen, until the next refresh.
        Row(Modifier.height(IntrinsicSize.Min)) {
            if (lane.unseen) Box(Modifier.width(4.dp).fillMaxHeight().background(Orange))
            Column(Modifier.weight(1f).padding(start = 12.dp, end = 4.dp, top = 10.dp, bottom = 10.dp)) {
                LaneHeader(lane, canMoveUp, canMoveDown, busy, onToggle, onMove, onEnd)
                if (expanded) LaneBody(lane, actions)
            }
        }
    }
}

@Composable
private fun LaneHeader(
    lane: BoardLane,
    canMoveUp: Boolean,
    canMoveDown: Boolean,
    busy: Boolean,
    onToggle: () -> Unit,
    onMove: (Int) -> Unit,
    onEnd: () -> Unit,
) {
    var menuOpen by remember { mutableStateOf(false) }
    Row(verticalAlignment = Alignment.Top) {
        Row(Modifier.weight(1f).clickable(onClick = onToggle), verticalAlignment = Alignment.Top) {
            Box(
                Modifier.padding(top = 2.dp, end = 10.dp).size(24.dp).background(CyanDeep, CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                Text("${lane.rank}", color = Cyan, style = MaterialTheme.typography.labelMedium, fontWeight = FontWeight.Bold)
            }
            Column(Modifier.weight(1f)) {
                Text(
                    "#${lane.goal.number} ${lane.goal.title}",
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.Bold,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    lane.goal.repo.substringAfter('/'),
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = FontFamily.Monospace,
                    color = TextMuted,
                )
                Text(
                    laneCountsLine(lane),
                    style = MaterialTheme.typography.labelSmall,
                    color = if (lane.counts.needsYou > 0) Amber else TextSecondary,
                    modifier = Modifier.padding(top = 2.dp),
                )
                if (lane.stale) Text("stale — GitHub reads are failing", style = MaterialTheme.typography.labelSmall, color = Amber)
            }
        }
        Box {
            IconButton(onClick = { menuOpen = true }, enabled = !busy) {
                Icon(Icons.Rounded.MoreVert, contentDescription = "Lane menu", tint = TextSecondary)
            }
            DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                DropdownMenuItem(text = { Text("Move up") }, enabled = canMoveUp, onClick = { menuOpen = false; onMove(-1) })
                DropdownMenuItem(text = { Text("Move down") }, enabled = canMoveDown, onClick = { menuOpen = false; onMove(1) })
                DropdownMenuItem(text = { Text("End lane", color = Rose) }, onClick = { menuOpen = false; onEnd() })
            }
        }
    }
}

@Composable
private fun LaneBody(lane: BoardLane, actions: BoardRowActions) {
    val base = lane.goal.repo
    var blockedOpen by rememberSaveable(lane.id) { mutableStateOf(false) }
    var doneOpen by rememberSaveable(lane.id) { mutableStateOf(false) }
    var changesOpen by rememberSaveable(lane.id) { mutableStateOf(false) }
    val open = lane.tickets.filter { it.state in setOf("needs_you", "ready", "in_progress") }
    val blocked = lane.tickets.filter { it.state == "blocked" }
    val done = lane.tickets.filter { it.state == "done" }
    val head = lane.longestChain.firstOrNull()
    Column(Modifier.padding(top = 10.dp, end = 8.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        if (lane.longestChain.size > 1) {
            Text(
                "Longest chain ${lane.longestChain.size}: " +
                    lane.longestChain.joinToString(" → ") { boardShortRef(it.repo, it.number, base) },
                style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace,
                color = TextMuted,
            )
        }
        lane.cycles.forEach { cycle ->
            Text(
                cycle.joinToString(" and ") { boardShortRef(it.repo, it.number, base) } + " wait on each other",
                style = MaterialTheme.typography.labelSmall,
                color = Rose,
            )
        }
        open.forEach { TicketRow(it, base, head, actions) }
        if (blocked.isNotEmpty()) {
            FoldLine("Blocked (${blocked.size})", blockedPreview(blocked, base), blockedOpen) { blockedOpen = !blockedOpen }
            if (blockedOpen) blocked.forEach { TicketRow(it, base, head, actions) }
        }
        if (done.isNotEmpty()) {
            FoldLine("Done (${done.size})", null, doneOpen) { doneOpen = !doneOpen }
            if (doneOpen) done.forEach { TicketRow(it, base, null, actions) }
        }
        if (lane.changes.isNotEmpty()) {
            FoldLine("Recent changes", null, changesOpen) { changesOpen = !changesOpen }
            if (changesOpen) {
                lane.changes.forEach { change ->
                    Text("· ${change.text}", style = MaterialTheme.typography.bodySmall, color = TextSecondary)
                }
            }
        }
    }
}

@Composable
private fun FoldLine(label: String, preview: String?, open: Boolean, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            (if (open) "▾ " else "▸ ") + label,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.SemiBold,
            color = TextSecondary,
        )
        if (!open && !preview.isNullOrBlank()) {
            Text(
                "  $preview",
                style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace,
                color = TextMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

private data class StateChip(val label: String, val color: Color)

private fun stateChip(state: String): StateChip = when (state) {
    "needs_you" -> StateChip("NEEDS YOU", Amber)
    "ready" -> StateChip("READY", Emerald)
    "in_progress" -> StateChip("IN PROG", Cyan)
    "done" -> StateChip("DONE", TextMuted)
    else -> StateChip("BLOCKED", TextMuted)
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun TicketRow(ticket: BoardTicket, base: String, head: BoardRef?, actions: BoardRowActions) {
    val uriHandler = LocalUriHandler.current
    val chip = stateChip(ticket.state)
    val needsYou = ticket.state == "needs_you"
    Surface(
        color = PanelMuted,
        shape = RoundedCornerShape(10.dp),
        border = BorderStroke(1.dp, if (needsYou) Amber.copy(alpha = 0.55f) else Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    chip.label,
                    color = chip.color,
                    style = MaterialTheme.typography.labelSmall,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier
                        .background(chip.color.copy(alpha = 0.16f), RoundedCornerShape(4.dp))
                        .padding(horizontal = 5.dp, vertical = 1.dp),
                )
                Text(
                    boardShortRef(ticket.repo, ticket.number, base),
                    style = MaterialTheme.typography.labelMedium,
                    fontFamily = FontFamily.Monospace,
                    color = Cyan,
                    modifier = Modifier.padding(start = 8.dp).clickable { uriHandler.openUri(ticket.url) },
                )
                Text(
                    ticket.title,
                    style = MaterialTheme.typography.bodyMedium,
                    color = if (ticket.state == "done") TextSecondary else MaterialTheme.colorScheme.onSurface,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(start = 6.dp).weight(1f),
                )
                if (ticket.new) {
                    Text(
                        "NEW",
                        color = Fuchsia,
                        style = MaterialTheme.typography.labelSmall,
                        fontWeight = FontWeight.Bold,
                        modifier = Modifier.padding(start = 6.dp),
                    )
                }
                if (boardCanStart(ticket)) {
                    Surface(
                        onClick = { actions.onStart(ticket) },
                        shape = RoundedCornerShape(999.dp),
                        color = CyanDeep,
                        modifier = Modifier.padding(start = 8.dp),
                    ) {
                        Text(
                            "Start",
                            color = Cyan,
                            style = MaterialTheme.typography.labelMedium,
                            fontWeight = FontWeight.SemiBold,
                            modifier = Modifier.padding(horizontal = 12.dp, vertical = 5.dp),
                        )
                    }
                }
            }
            val details = ticketDetails(ticket, base, head, actions) { uriHandler.openUri(it) }
            if (details.isNotEmpty()) {
                FlowRow(
                    modifier = Modifier.padding(top = 4.dp),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalArrangement = Arrangement.spacedBy(2.dp),
                ) {
                    details.forEach { detail ->
                        Text(
                            detail.text,
                            style = MaterialTheme.typography.labelSmall,
                            color = detail.color,
                            modifier = detail.onClick?.let { Modifier.clickable(onClick = it) } ?: Modifier,
                        )
                    }
                }
            }
        }
    }
}

private data class Detail(val text: String, val color: Color, val onClick: (() -> Unit)? = null)

/** A row's detail line (appendix J2), in the order the web board shows it. */
private fun ticketDetails(
    ticket: BoardTicket,
    base: String,
    head: BoardRef?,
    actions: BoardRowActions,
    openUrl: (String) -> Unit,
): List<Detail> {
    val details = mutableListOf<Detail>()
    ticket.needsYou?.let { needs ->
        details += Detail(needs.text, Amber) { actions.onOpenNeedsYou(ticket) }
        if (head != null && head.repo == ticket.repo && head.number == ticket.number) {
            details += Detail("heads the longest chain", Amber)
        }
    }
    ticket.holder?.let { holder ->
        details += Detail(
            "${holder.name} (${holder.state})",
            if (holder.state == "stopped") Rose else TextSecondary,
        ) { actions.onOpenAgent(ticket) }
    }
    val waiting = ticket.waitsOn.filter { it.state != "done" }
    if (waiting.isNotEmpty()) {
        details += Detail("waits on " + waiting.joinToString(", ") { boardShortRef(it.repo, it.number, base) }, TextMuted)
    }
    ticket.prs.forEach { pr ->
        details += Detail("PR #${pr.number} · ${pr.state.lowercase()}", Violet) { openUrl(pr.url) }
    }
    if (ticket.state == "done") ticket.doneReason?.let { details += Detail(it.replace('_', ' '), TextMuted) }
    ticket.warnings.forEach { details += Detail("! " + boardWarningText(it), Rose) }
    if (ticket.subIssuesDone && ticket.state == "ready") {
        details += Detail("All sub-issues done — close on GitHub", Amber) { openUrl(ticket.url) }
    }
    ticket.alsoIn.forEach { lane ->
        details += Detail("also in lane ${lane.rank}", Cyan) { actions.onShowLane(lane.laneId) }
    }
    return details
}

@Composable
private fun OtherTickets(groups: List<li.rajeshgo.sm.data.model.BoardOtherGroup>, actions: BoardRowActions) {
    var open by rememberSaveable { mutableStateOf(false) }
    val count = groups.sumOf { it.tickets.size }
    Surface(
        color = Panel,
        shape = RoundedCornerShape(12.dp),
        border = BorderStroke(1.dp, Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            FoldLine(
                "Not in any lane",
                groups.filter { it.tickets.isNotEmpty() }.joinToString(" · ") { "${it.repo.substringAfter('/')}: ${it.tickets.size} open" },
                open,
            ) { open = !open }
            if (open && count > 0) {
                groups.filter { it.tickets.isNotEmpty() }.forEach { group ->
                    Text(
                        group.repo,
                        style = MaterialTheme.typography.labelSmall,
                        fontFamily = FontFamily.Monospace,
                        color = TextMuted,
                        modifier = Modifier.padding(top = 6.dp),
                    )
                    group.tickets.forEach { TicketRow(it, group.repo, null, actions) }
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun AddLaneDialog(
    repos: List<String>,
    busy: Boolean,
    onDismiss: () -> Unit,
    onAdd: (String, Long) -> Unit,
) {
    var repo by rememberSaveable { mutableStateOf(repos.firstOrNull().orEmpty()) }
    var number by rememberSaveable { mutableStateOf("") }
    var menuOpen by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = { if (!busy) onDismiss() },
        title = { Text("Add lane") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text("The goal ticket and everything it waits on. The lane goes to the bottom.", style = MaterialTheme.typography.bodySmall, color = TextSecondary)
                ExposedDropdownMenuBox(expanded = menuOpen, onExpandedChange = { if (!busy) menuOpen = it }) {
                    OutlinedTextField(
                        value = repo.substringAfter('/'),
                        onValueChange = {},
                        readOnly = true,
                        enabled = !busy,
                        label = { Text("Repo") },
                        trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(menuOpen) },
                        modifier = Modifier.menuAnchor(MenuAnchorType.PrimaryNotEditable, !busy).fillMaxWidth(),
                    )
                    ExposedDropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                        repos.forEach { option ->
                            DropdownMenuItem(text = { Text(option) }, onClick = { repo = option; menuOpen = false })
                        }
                    }
                }
                OutlinedTextField(
                    value = number,
                    onValueChange = { number = it.filter(Char::isDigit) },
                    label = { Text("Goal ticket number") },
                    enabled = !busy,
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            val goal = number.toLongOrNull()
            TextButton(onClick = { goal?.let { onAdd(repo, it) } }, enabled = !busy && repo.isNotBlank() && goal != null && goal > 0) {
                if (busy) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp) else Text("Add")
            }
        },
        dismissButton = { TextButton(onClick = onDismiss, enabled = !busy) { Text("Cancel") } },
    )
}
