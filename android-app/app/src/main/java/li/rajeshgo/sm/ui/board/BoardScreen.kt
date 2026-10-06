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
import li.rajeshgo.sm.data.model.BoardDoc
import li.rajeshgo.sm.data.model.BoardLane
import li.rajeshgo.sm.data.model.BoardRef
import li.rajeshgo.sm.data.model.BoardResponse
import li.rajeshgo.sm.data.model.BoardStartRequest
import li.rajeshgo.sm.data.model.BoardTicket
import li.rajeshgo.sm.data.model.QueueOverview
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.inbox.ThreadScreen
import li.rajeshgo.sm.ui.inbox.ThreadTarget
import li.rajeshgo.sm.ui.links.LinkChip
import li.rajeshgo.sm.ui.links.LinksRow
import li.rajeshgo.sm.ui.links.agentChipText
import li.rajeshgo.sm.ui.links.jobChips
import li.rajeshgo.sm.ui.links.prChipColor
import li.rajeshgo.sm.ui.links.prChipText
import li.rajeshgo.sm.ui.links.threadChipText
import li.rajeshgo.sm.ui.navigation.TerminalOpenRequests
import li.rajeshgo.sm.ui.navigation.AppBottomNav
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.navigation.AppTopBar
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.queue.rememberResumed
import li.rajeshgo.sm.ui.queue.secondsBetween
import li.rajeshgo.sm.ui.queue.shortDuration
import li.rajeshgo.sm.ui.reviews.ReviewPolicySheet
import li.rajeshgo.sm.ui.reviews.StartReviewerRow
import li.rajeshgo.sm.ui.reviews.reviewerLabel
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
/** The standing Bugs ticket's line (spec 1859 C5): `Standing lane · 2 open bugs`; null for any other ticket. */
fun boardStandingText(ticket: BoardTicket): String? {
    if (ticket.state != "standing") return null
    val open = ticket.waitsOn.count { it.state != "done" }
    return "Standing lane · $open open bug" + if (open == 1) "" else "s"
}

fun boardCanStart(ticket: BoardTicket): Boolean = ticket.state == "ready" && "merged_not_closed" !in ticket.warnings

/**
 * Start anyway (spec 1782 H3): a blocked ticket the server would start with `start_blocked`.
 * A ticket an agent already holds is refused whatever the flag says.
 */
fun boardCanStartAnyway(ticket: BoardTicket): Boolean =
    ticket.state == "blocked" && ticket.holder == null &&
        ticket.warnings.none { it in setOf("stale", "cycle", "merged_not_closed") }

/** The row's own Inbox thread: the ticket's work thread (sm#1835), else the holder's newest thread. */
fun boardThreadTarget(ticket: BoardTicket): ThreadTarget? {
    val holder = ticket.holder ?: return null
    return ThreadTarget("ticket:${ticket.repo.lowercase()}#${ticket.number}", holder.sessionId, "#${ticket.number} ${ticket.title}")
}

/** Blocked rows show while a lane has at most this many non-done tickets (spec 1782 H1). */
const val BLOCKED_ROW_LIMIT = 12

/** Most recently closed tickets shown as rows; the rest fold. */
const val DONE_ROW_LIMIT = 3

/** "Not in any lane" rows per repo before "{n} more ›"; needs-you rows always show. */
const val OTHER_ROW_LIMIT = 10

/** A lane's tickets as the Board draws them (the same rules as the web Board). */
data class LaneGroups(
    /** Needs you, all parts done, ready and in progress: always rows. */
    val active: List<BoardTicket>,
    val blocked: List<BoardTicket>,
    /** Newest closed first. */
    val done: List<BoardTicket>,
) {
    /** Blocked tickets are slim rows, else one fold line. */
    val showBlocked: Boolean get() = active.size + blocked.size <= BLOCKED_ROW_LIMIT

    /** A lane with one active ticket draws it in its header. */
    val short: Boolean get() = active.size == 1
}

fun laneGroups(tickets: List<BoardTicket>): LaneGroups = LaneGroups(
    active = tickets.filter { it.state !in setOf("blocked", "done") },
    blocked = tickets.filter { it.state == "blocked" },
    done = tickets.filter { it.state == "done" }.sortedByDescending { it.closedAt.orEmpty() },
)

/** One repo's "Not in any lane" rows: every needs-you ticket, then others up to [OTHER_ROW_LIMIT]. */
fun visibleOther(tickets: List<BoardTicket>): List<BoardTicket> {
    val urgent = tickets.filter { it.state == "needs_you" }
    val rest = tickets.filter { it.state != "needs_you" }
    return urgent + rest.take((OTHER_ROW_LIMIT - urgent.size).coerceAtLeast(0))
}

/** A ticket's open blockers, as refs: "#1776", "#1777". */
fun openBlockers(ticket: BoardTicket, base: String): List<String> =
    ticket.waitsOn.filter { it.state != "done" }.map { boardShortRef(it.repo, it.number, base) }

/** "#a", "#a and #b", "#a, #b and #c". */
fun refList(refs: List<String>): String = when (refs.size) {
    0 -> ""
    1 -> refs[0]
    else -> refs.dropLast(1).joinToString(", ") + " and " + refs.last()
}

/** The Start anyway confirmation: "#1777 waits on #1776, which is not done." */
fun startAnywayText(ticket: BoardTicket, base: String): String {
    val blockers = openBlockers(ticket, base)
    if (blockers.isEmpty()) return "#${ticket.number} is blocked. Start it anyway?"
    val verb = if (blockers.size == 1) "is" else "are"
    return "#${ticket.number} waits on ${refList(blockers)}, which $verb not done."
}

/** The folded Blocked line: "14 blocked · waiting on #1776, #1777". */
fun blockedFoldCaption(tickets: List<BoardTicket>, base: String): String {
    val blockers = tickets.flatMap { openBlockers(it, base) }.distinct()
    return "${tickets.size} blocked" + if (blockers.isEmpty()) "" else " · waiting on " + blockers.joinToString(", ")
}

/** The lane's counts line, as the phone mock shows it. */
fun laneCountsLine(lane: BoardLane): String {
    val c = lane.counts
    val parts = mutableListOf<String>()
    if (c.needsYou > 0) parts += "${c.needsYou} needs you"
    if (c.closeReady > 0) parts += "${c.closeReady} all parts done"
    parts += "${c.ready} ready"
    parts += "${c.inProgress} in progress"
    parts += "${c.blocked} blocked"
    if (lane.longestChain.isNotEmpty()) parts += "chain ${lane.longestChain.size}"
    return parts.joinToString(" · ")
}

/** The lane's jobs across its agents: "Queue: 4 running · 6 waiting", or null when none. */
fun laneQueueLine(lane: BoardLane, overview: QueueOverview?): String? {
    if (overview == null) return null
    val holders = lane.tickets.mapNotNull { it.holder?.sessionId?.takeIf(String::isNotBlank) }.toSet()
    val running = overview.running.count { job -> holders.any(job::isAwaitedBy) }
    val waiting = overview.queued.count { job -> holders.any(job::isAwaitedBy) }
    if (running == 0 && waiting == 0) return null
    return "Queue: $running running · $waiting waiting"
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
    val uriHandler = LocalUriHandler.current
    val listState = rememberLazyListState()
    var now by remember { mutableStateOf(OffsetDateTime.now()) }
    var reader by remember { mutableStateOf<ReaderPage?>(null) }
    var thread by remember { mutableStateOf<ThreadTarget?>(null) }
    var startingAnyway by remember { mutableStateOf<BoardTicket?>(null) }
    var closing by remember { mutableStateOf<BoardTicket?>(null) }
    var addingLane by remember { mutableStateOf(false) }
    var ending by remember { mutableStateOf<BoardLane?>(null) }
    val toast = { text: String -> Toast.makeText(context, text, Toast.LENGTH_SHORT).show() }

    // Read while shown; each read marks the board seen (appendix K).
    val overlay = reader != null || thread != null
    LaunchedEffect(resumed, overlay) {
        if (!resumed || overlay) return@LaunchedEffect
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
        onStart = { viewModel.openStart(it) },
        onStartWhenReady = { viewModel.openStart(it, whenReady = true) },
        onCancelStart = { viewModel.cancelStart(it, toast) },
        onStartAnyway = { startingAnyway = it },
        onClose = { closing = it },
        onOpenTerminal = { sessionId ->
            TerminalOpenRequests.pending = sessionId
            onNavigateToWatch()
        },
        onOpenThread = { ticket -> boardThreadTarget(ticket)?.let { thread = it } },
        onOpenDoc = { ticket, doc ->
            reader = ReaderPage(title = doc.title, subtitle = "#${ticket.number} ${ticket.title}", path = doc.readerPath)
        },
        onOpenAgent = { ticket ->
            ticket.holder?.let { holder ->
                FollowOpenRequests.pending = FollowOpen(holder.sessionId, null, holder.name)
                onNavigateToWatch()
            }
        },
        onOpenNeedsYou = { ticket ->
            ticket.needsYou?.let { needs ->
                if (needs.kind == "elsewhere") uriHandler.openUri(needs.url)
                else reader = ReaderPage(title = needs.text, subtitle = "#${ticket.number} ${ticket.title}", path = needs.url)
            }
        },
        onShowLane = { laneId -> showLane(laneId).takeIf { it >= 0 }?.let { scrollTo = it } },
        onOpenQueue = onNavigateToQueue,
        onReviewPolicy = { ticket -> viewModel.openReviewPolicy(ReviewPolicyEdit.ticket(ticket)) },
        queue = state.queue,
        busy = state.busy,
        now = now,
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
                        refreshBar = state.revalidating,
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
                                expanded = expanded[lane.id] ?: true,
                                canMoveUp = index > 0,
                                canMoveDown = index < lanes.lastIndex,
                                busy = state.busy,
                                actions = actions,
                                onToggle = { viewModel.toggle(lane.id) },
                                onMove = { delta -> viewModel.move(lane.id, delta, toast) },
                                onEnd = { ending = lane },
                                onReviewPolicy = { viewModel.openReviewPolicy(ReviewPolicyEdit.lane(lane)) },
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

        thread?.let { target ->
            ThreadScreen(
                target = target,
                onClose = { thread = null },
                onOpenTerminal = { sessionId ->
                    thread = null
                    TerminalOpenRequests.pending = sessionId
                    onNavigateToWatch()
                },
                onDone = { thread = null },
                onMessage = toast,
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

    startingAnyway?.let { ticket ->
        AlertDialog(
            onDismissRequest = { startingAnyway = null },
            title = { Text("Start anyway?") },
            text = { Text(startAnywayText(ticket, ticket.repo) + " The agent is told to build on what is done so far.") },
            confirmButton = {
                TextButton(onClick = { startingAnyway = null; viewModel.openStart(ticket, startBlocked = true) }) { Text("Start anyway") }
            },
            dismissButton = { TextButton(onClick = { startingAnyway = null }) { Text("Cancel") } },
        )
    }

    closing?.let { ticket ->
        AlertDialog(
            onDismissRequest = { closing = null },
            title = { Text("Close #${ticket.number}?") },
            text = {
                Text("All ${ticket.subIssues.total} parts are done. sm closes the ticket on GitHub with a comment listing them.")
            },
            confirmButton = {
                TextButton(onClick = { closing = null; viewModel.close(ticket) { error -> toast(error ?: "Closed #${ticket.number}") } }) {
                    Text("Close")
                }
            },
            dismissButton = { TextButton(onClick = { closing = null }) { Text("Cancel") } },
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

    state.reviewPolicy?.let { edit ->
        ReviewPolicySheet(
            title = edit.title,
            inheritLabel = edit.inheritLabel,
            ownLabel = edit.ownLabel,
            current = edit.current,
            allowPaired = edit.allowPaired,
            busy = edit.busy,
            error = edit.error,
            onDismiss = viewModel::closeReviewPolicy,
            onSave = viewModel::saveReviewPolicy,
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
            val policy = options.reviewPolicy
            // The row starts on the ticket's own reviewer if it has one, else on the lane's.
            val ticketOwn = policy?.source?.startsWith("ticket") == true
            var reviewer by remember(start.ticket.repo, start.ticket.number, policy) {
                mutableStateOf(if (ticketOwn) policy?.resolved else null)
            }
            val selectedAgentType = if (start.whenReady) {
                state.agentTypes.firstOrNull { it.name == (start.ticket.autoStart?.agentType ?: start.ticket.tier) }
                    ?: state.agentTypes.firstOrNull {
                        it.provider == defaults?.provider && it.model == defaults?.model && it.effort == defaults?.reasoningEffort
                    }
            } else null
            CreateSessionSheet(
                source = null,
                sessions = emptyList(),
                loadModels = viewModel::sessionModels,
                loadAgentTypes = viewModel::sessionAgentTypes,
                busy = start.busy,
                error = start.error,
                onDismiss = viewModel::closeStart,
                ticket = TicketStart(
                    label = "#${start.ticket.number} ${start.ticket.title}",
                    workingDir = options.workingDir,
                    name = options.name,
                    brief = if (start.whenReady) start.ticket.autoStart?.brief ?: options.brief else options.brief,
                    provider = selectedAgentType?.provider ?: start.ticket.autoStart?.provider ?: defaults?.provider ?: "claude",
                    model = selectedAgentType?.model ?: start.ticket.autoStart?.model ?: defaults?.model,
                    effort = selectedAgentType?.effort ?: start.ticket.autoStart?.effort ?: defaults?.reasoningEffort ?: "high",
                    whenReady = start.whenReady,
                    agentTypes = state.agentTypes,
                ),
                extra = { enabled ->
                    StartReviewerRow(
                        laneDefault = policy?.takeUnless { ticketOwn },
                        value = reviewer,
                        onChange = { reviewer = it },
                        enabled = enabled,
                    )
                },
            ) { request ->
                if (start.whenReady) {
                    val type = state.agentTypes.firstOrNull {
                        it.provider == request.provider && it.model == request.model && it.effort == request.reasoningEffort
                    }
                    viewModel.authorizeStart(
                        li.rajeshgo.sm.data.model.BoardAutoStartChoice(start.ticket.repo, start.ticket.number,
                            type?.name, request.provider, request.model, request.reasoningEffort, request.initialMessage),
                    ) { toast(it) }
                } else {
                viewModel.start(
                    BoardStartRequest(
                        repo = start.ticket.repo,
                        number = start.ticket.number,
                        provider = request.provider,
                        model = request.model,
                        reasoningEffort = request.reasoningEffort,
                        name = request.name,
                        brief = request.initialMessage,
                        startBlocked = start.startBlocked,
                        // Unchanged from the ticket's stored policy: leave it as it was set.
                        reviewer = reviewer?.takeUnless { ticketOwn && it == policy?.resolved },
                    ),
                    clearTicketPolicy = ticketOwn && reviewer == null,
                ) { name -> toast("Started $name") }
                }
            }
        }
    }
}

/** What a ticket row can open. */
private class BoardRowActions(
    val onStart: (BoardTicket) -> Unit,
    val onStartWhenReady: (BoardTicket) -> Unit,
    val onCancelStart: (BoardTicket) -> Unit,
    /** Start anyway: a confirm, then Start with `start_blocked` (spec 1782 H3). */
    val onStartAnyway: (BoardTicket) -> Unit,
    /** Close a ticket whose parts are all done, after a confirm (spec 1782 H2). */
    val onClose: (BoardTicket) -> Unit,
    val onOpenAgent: (BoardTicket) -> Unit,
    val onOpenTerminal: (String) -> Unit,
    val onOpenNeedsYou: (BoardTicket) -> Unit,
    val onOpenThread: (BoardTicket) -> Unit,
    val onOpenDoc: (BoardTicket, BoardDoc) -> Unit,
    val onShowLane: (Long) -> Unit,
    val onOpenQueue: () -> Unit,
    /** Opens the ticket's review policy sheet (sm#1768 Figure 7B). */
    val onReviewPolicy: (BoardTicket) -> Unit,
    /** The queue as last read, for each lane's job line; null until the first read. */
    val queue: QueueOverview?,
    val busy: Boolean,
    val now: OffsetDateTime,
)

/** The lane goal as a ticket, for the header's Close. */
private fun goalTicket(lane: BoardLane): BoardTicket = BoardTicket(
    repo = lane.goal.repo,
    number = lane.goal.number,
    title = lane.goal.title,
    url = lane.goal.url,
    state = lane.goal.state,
    subIssues = lane.goal.subIssues,
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
    onReviewPolicy: () -> Unit,
) {
    val groups = laneGroups(lane.tickets)
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
                LaneHeader(lane, laneQueueLine(lane, actions.queue), canMoveUp, canMoveDown, busy, onToggle, onMove, onEnd, actions.onOpenQueue, onReviewPolicy) {
                    actions.onClose(goalTicket(lane))
                }
                // A lane with one active ticket shows it under its header, open or not.
                if (groups.short) {
                    Box(Modifier.padding(top = 8.dp, end = 8.dp)) {
                        TicketRow(groups.active[0], lane.goal.repo, lane.longestChain.firstOrNull(), actions)
                    }
                }
                if (expanded) LaneBody(lane, groups, actions)
            }
        }
    }
}

@Composable
private fun LaneHeader(
    lane: BoardLane,
    queueLine: String?,
    canMoveUp: Boolean,
    canMoveDown: Boolean,
    busy: Boolean,
    onToggle: () -> Unit,
    onMove: (Int) -> Unit,
    onEnd: () -> Unit,
    onOpenQueue: () -> Unit,
    onReviewPolicy: () -> Unit,
    onCloseGoal: () -> Unit,
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
                if (lane.goal.state == "standing") {
                    val goal = lane.tickets.firstOrNull { it.repo == lane.goal.repo && it.number == lane.goal.number }
                    goal?.let(::boardStandingText)?.let {
                        Text(it, style = MaterialTheme.typography.labelSmall, color = Cyan, modifier = Modifier.padding(top = 2.dp))
                    }
                }
                Text(
                    laneCountsLine(lane),
                    style = MaterialTheme.typography.labelSmall,
                    color = if (lane.counts.needsYou > 0) Amber else TextSecondary,
                    modifier = Modifier.padding(top = 2.dp),
                )
                if (lane.goal.state == "close_ready") {
                    Text(
                        "${lane.goal.subIssues.done} of ${lane.goal.subIssues.total} parts done · All parts done",
                        style = MaterialTheme.typography.labelSmall,
                        color = Cyan,
                        modifier = Modifier.padding(top = 2.dp),
                    )
                }
                queueLine?.let {
                    Text(
                        it,
                        style = MaterialTheme.typography.labelSmall,
                        color = Cyan,
                        modifier = Modifier.padding(top = 2.dp).clickable(onClick = onOpenQueue),
                    )
                }
                if (lane.stale) Text("stale — GitHub reads are failing", style = MaterialTheme.typography.labelSmall, color = Amber)
                lane.reviewPolicy?.let { policy ->
                    Text(
                        "Reviews: ${reviewerLabel(policy.reviewer)}",
                        style = MaterialTheme.typography.labelSmall,
                        color = Cyan,
                        fontWeight = FontWeight.SemiBold,
                        modifier = Modifier.padding(top = 2.dp).clickable(onClick = onReviewPolicy),
                    )
                }
            }
        }
        if (lane.goal.state == "close_ready") RowButton("Close", primary = true, enabled = !busy, onClick = onCloseGoal)
        Box {
            IconButton(onClick = { menuOpen = true }, enabled = !busy) {
                Icon(Icons.Rounded.MoreVert, contentDescription = "Lane menu", tint = TextSecondary)
            }
            DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                DropdownMenuItem(text = { Text("Move up") }, enabled = canMoveUp, onClick = { menuOpen = false; onMove(-1) })
                DropdownMenuItem(text = { Text("Move down") }, enabled = canMoveDown, onClick = { menuOpen = false; onMove(1) })
                DropdownMenuItem(text = { Text("Review policy…") }, onClick = { menuOpen = false; onReviewPolicy() })
                DropdownMenuItem(text = { Text("End lane", color = Rose) }, onClick = { menuOpen = false; onEnd() })
            }
        }
    }
}

@Composable
private fun LaneBody(lane: BoardLane, groups: LaneGroups, actions: BoardRowActions) {
    val base = lane.goal.repo
    var blockedOpen by rememberSaveable(lane.id) { mutableStateOf(false) }
    var doneOpen by rememberSaveable(lane.id) { mutableStateOf(false) }
    var changesOpen by rememberSaveable(lane.id) { mutableStateOf(false) }
    val head = lane.longestChain.firstOrNull()
    val moreDone = groups.done.drop(DONE_ROW_LIMIT)
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
        if (!groups.short) groups.active.forEach { TicketRow(it, base, head, actions) }
        if (groups.showBlocked) {
            groups.blocked.forEach { TicketRow(it, base, head, actions, slim = true) }
        } else if (groups.blocked.isNotEmpty()) {
            FoldLine(blockedFoldCaption(groups.blocked, base), null, blockedOpen) { blockedOpen = !blockedOpen }
            if (blockedOpen) groups.blocked.forEach { TicketRow(it, base, head, actions, slim = true) }
        }
        groups.done.take(DONE_ROW_LIMIT).forEach { TicketRow(it, base, null, actions, slim = true) }
        if (moreDone.isNotEmpty()) {
            FoldLine("${moreDone.size} more done", null, doneOpen) { doneOpen = !doneOpen }
            if (doneOpen) moreDone.forEach { TicketRow(it, base, null, actions, slim = true) }
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
    "close_ready" -> StateChip("ALL PARTS DONE", Cyan)
    "ready" -> StateChip("READY", Emerald)
    "in_progress" -> StateChip("IN PROG", Cyan)
    "done" -> StateChip("DONE", TextMuted)
    "standing" -> StateChip("STANDING", Cyan)
    else -> StateChip("BLOCKED", TextMuted)
}

@Composable
private fun RowButton(label: String, primary: Boolean, enabled: Boolean = true, onClick: () -> Unit) {
    Surface(
        onClick = onClick,
        enabled = enabled,
        shape = RoundedCornerShape(999.dp),
        color = if (primary) CyanDeep else Color.Transparent,
        border = if (primary) null else BorderStroke(1.dp, Border),
        modifier = Modifier.padding(start = 8.dp),
    ) {
        Text(
            label,
            color = if (primary) Cyan else TextSecondary,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.SemiBold,
            maxLines = 1,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 5.dp),
        )
    }
}

/**
 * One ticket. A [slim] row (blocked and done tickets, spec 1782 H1) holds the
 * ref, title, what it waits on and Start anyway; a full row adds the details
 * line and the Links line.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun TicketRow(ticket: BoardTicket, base: String, head: BoardRef?, actions: BoardRowActions, slim: Boolean = false) {
    val uriHandler = LocalUriHandler.current
    val chip = stateChip(ticket.state)
    val needsYou = ticket.state == "needs_you"
    Surface(
        color = PanelMuted,
        shape = RoundedCornerShape(10.dp),
        border = BorderStroke(1.dp, if (needsYou) Amber.copy(alpha = 0.55f) else Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(horizontal = 10.dp, vertical = if (slim) 6.dp else 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (!slim) {
                    Text(
                        chip.label,
                        color = chip.color,
                        style = MaterialTheme.typography.labelSmall,
                        fontWeight = FontWeight.Bold,
                        modifier = Modifier
                            .padding(end = 8.dp)
                            .background(chip.color.copy(alpha = 0.16f), RoundedCornerShape(4.dp))
                            .padding(horizontal = 5.dp, vertical = 1.dp),
                    )
                }
                Text(
                    boardShortRef(ticket.repo, ticket.number, base),
                    style = MaterialTheme.typography.labelMedium,
                    fontFamily = FontFamily.Monospace,
                    color = Cyan,
                    modifier = Modifier.clickable { uriHandler.openUri(ticket.url) },
                )
                Text(
                    ticket.title,
                    style = if (slim) MaterialTheme.typography.bodySmall else MaterialTheme.typography.bodyMedium,
                    color = if (slim) TextSecondary else MaterialTheme.colorScheme.onSurface,
                    maxLines = if (slim) 1 else 2,
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
                if (boardCanStart(ticket)) RowButton("Start", primary = true) { actions.onStart(ticket) }
                if (boardCanStartAnyway(ticket)) RowButton("Start anyway", primary = false) { actions.onStartAnyway(ticket) }
                if (ticket.state == "close_ready") RowButton("Close", primary = true, enabled = !actions.busy) { actions.onClose(ticket) }
                if (ticket.state != "done") {
                    var menuOpen by remember { mutableStateOf(false) }
                    Box {
                        IconButton(onClick = { menuOpen = true }, modifier = Modifier.size(28.dp)) {
                            Icon(Icons.Rounded.MoreVert, contentDescription = "Ticket menu", tint = TextMuted, modifier = Modifier.size(18.dp))
                        }
                        DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                            if (ticket.state in setOf("blocked", "ready") && ticket.holder == null) {
                                DropdownMenuItem(text = { Text(if (ticket.autoStart == null) "Start when ready…" else "Change automatic start…") }, onClick = {
                                    menuOpen = false; actions.onStartWhenReady(ticket)
                                })
                            }
                            if (ticket.autoStart != null) DropdownMenuItem(text = { Text("Cancel automatic start") }, onClick = {
                                menuOpen = false; actions.onCancelStart(ticket)
                            })
                            if (ticket.state == "close_ready") {
                                DropdownMenuItem(text = { Text("Start instead") }, onClick = { menuOpen = false; actions.onStart(ticket) })
                            }
                            DropdownMenuItem(text = { Text("Review policy…") }, onClick = { menuOpen = false; actions.onReviewPolicy(ticket) })
                        }
                    }
                }
            }
            ticket.autoStart?.let { planned ->
                Text("⏵ when ready · ${planned.agentType ?: "Custom"} · ${planned.model.orEmpty()} ${planned.effort.orEmpty()}" +
                    if (planned.state == "failed") " · Failed: ${planned.lastError ?: "Retry"}" else "",
                    style = MaterialTheme.typography.labelSmall, color = if (planned.state == "failed") Rose else Amber,
                    modifier = Modifier.clickable { actions.onStartWhenReady(ticket) }.padding(top = 3.dp))
            }
            if (slim) {
                val line = when (ticket.state) {
                    "blocked" -> openBlockers(ticket, base).takeIf { it.isNotEmpty() }?.let { "waits on " + it.joinToString(", ") }
                    "done" -> ticket.doneReason?.replace('_', ' ')
                    else -> null
                }
                val extra = listOfNotNull(line, "started early".takeIf { ticket.startedEarly }) +
                    ticket.warnings.map { "! " + boardWarningText(it) }
                if (extra.isNotEmpty()) {
                    Text(
                        extra.joinToString(" · "),
                        style = MaterialTheme.typography.labelSmall,
                        color = if (ticket.warnings.isNotEmpty()) Rose else TextMuted,
                        modifier = Modifier.padding(top = 2.dp),
                    )
                }
                return@Column
            }
            if (ticket.state == "close_ready") {
                Text(
                    "${ticket.subIssues.done} of ${ticket.subIssues.total} parts done · All parts done",
                    style = MaterialTheme.typography.labelSmall,
                    color = Cyan,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            val details = ticketDetails(ticket, base, head, actions)
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
            LinksRow(boardLinks(ticket, actions) { uriHandler.openUri(it) }, Modifier.padding(top = 6.dp))
        }
    }
}

private data class Detail(val text: String, val color: Color, val onClick: (() -> Unit)? = null)

/** A ticket PR's active review: `Review: Codex run (gpt-6-sol, medium), 6 min`. */
fun boardReviewText(review: li.rajeshgo.sm.data.model.BoardTicketReview, now: OffsetDateTime): String {
    val since = review.since?.let { runCatching { OffsetDateTime.parse(it) }.getOrNull() }
    val age = since?.let { ", " + shortDuration(Duration.between(it, now).seconds.coerceAtLeast(0)) }.orEmpty()
    val who = review.reviewerLabel?.takeIf { it.isNotBlank() } ?: "a reviewer"
    return if (review.state == "owner") "You are reviewing$age" else "Review: $who$age"
}

/** A row's detail line: what needs you, what it waits on, reviews, warnings. Links carry the rest. */
private fun ticketDetails(
    ticket: BoardTicket,
    base: String,
    head: BoardRef?,
    actions: BoardRowActions,
): List<Detail> {
    val details = mutableListOf<Detail>()
    ticket.needsYou?.let { needs ->
        details += Detail(if (needs.kind == "elsewhere") "${needs.text} · Open" else needs.text, Amber) { actions.onOpenNeedsYou(ticket) }
        if (head != null && head.repo == ticket.repo && head.number == ticket.number) {
            details += Detail("heads the longest chain", Amber)
        }
    }
    val waiting = openBlockers(ticket, base)
    if (waiting.isNotEmpty()) details += Detail("waits on " + waiting.joinToString(", "), TextMuted)
    if (ticket.startedEarly) details += Detail("started early", TextMuted)
    ticket.review?.let { review ->
        details += Detail(boardReviewText(review, actions.now), Amber)
    }
    ticket.reviewPolicy?.let { policy ->
        details += Detail("Review: ${reviewerLabel(policy.reviewer)}", Cyan) { actions.onReviewPolicy(ticket) }
    }
    if (ticket.state == "done") ticket.doneReason?.let { details += Detail(it.replace('_', ' '), TextMuted) }
    ticket.warnings.forEach { details += Detail("! " + boardWarningText(it), Rose) }
    boardStandingText(ticket)?.let { details += Detail(it, Cyan) }
    ticket.alsoIn.forEach { lane ->
        details += Detail("also in lane ${lane.rank}", Cyan) { actions.onShowLane(lane.laneId) }
    }
    return details
}

/** The ticket's Links line (spec 1782 H4); GitHub opens outside the app. */
private fun boardLinks(ticket: BoardTicket, actions: BoardRowActions, openUrl: (String) -> Unit): List<LinkChip> = buildList {
    add(LinkChip("#${ticket.number} ↗", Cyan) { openUrl(ticket.url) })
    ticket.prs.forEach { pr -> add(LinkChip(prChipText(pr, actions.now), prChipColor(pr)) { openUrl(pr.url) }) }
    ticket.holder?.let { holder ->
        val color = when (holder.state) {
            "working" -> Emerald
            "stopped" -> Rose
            else -> TextSecondary
        }
        add(LinkChip(agentChipText(holder, actions.now), color, onTerminal = { actions.onOpenTerminal(holder.sessionId) }) { actions.onOpenAgent(ticket) })
    }
    addAll(jobChips(ticket.jobs, actions.now) { actions.onOpenQueue() })
    ticket.thread?.let { thread ->
        add(LinkChip(threadChipText(thread), if (thread.needsYou) Fuchsia else TextSecondary) { actions.onOpenThread(ticket) })
    }
    ticket.docs.forEach { doc -> add(LinkChip(doc.title, Violet) { actions.onOpenDoc(ticket, doc) }) }
}

/** Tickets outside every lane: an open section per repo (spec 1782 H1). */
@Composable
private fun OtherTickets(groups: List<li.rajeshgo.sm.data.model.BoardOtherGroup>, actions: BoardRowActions) {
    Surface(
        color = Panel,
        shape = RoundedCornerShape(12.dp),
        border = BorderStroke(1.dp, Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text("Not in any lane", style = MaterialTheme.typography.labelMedium, fontWeight = FontWeight.SemiBold, color = TextSecondary)
            groups.filter { it.tickets.isNotEmpty() }.forEach { group ->
                var moreOpen by rememberSaveable(group.repo) { mutableStateOf(false) }
                val shown = visibleOther(group.tickets)
                val more = group.tickets.filterNot { it in shown }
                Text(
                    group.repo,
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = FontFamily.Monospace,
                    color = TextMuted,
                    modifier = Modifier.padding(top = 6.dp),
                )
                shown.forEach { TicketRow(it, group.repo, null, actions) }
                if (more.isNotEmpty()) {
                    FoldLine("${more.size} more", null, moreOpen) { moreOpen = !moreOpen }
                    if (moreOpen) more.forEach { TicketRow(it, group.repo, null, actions) }
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
