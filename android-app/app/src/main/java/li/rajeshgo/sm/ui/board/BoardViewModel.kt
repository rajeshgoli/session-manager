package li.rajeshgo.sm.ui.board

import android.app.Application
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.time.OffsetDateTime
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.BoardResponse
import li.rajeshgo.sm.data.model.BoardStartOptions
import li.rajeshgo.sm.data.model.BoardStartRequest
import li.rajeshgo.sm.data.model.BoardTicket
import li.rajeshgo.sm.data.repository.ScreenCache
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.watch.DocReaderAuth
import li.rajeshgo.sm.ui.watch.loadDocReaderAuth

/** How long after asking for a GitHub read the board is fetched again, as the web page does. */
private const val READ_SETTLE_MS = 2_000L

/**
 * The Board count on every screen's bottom bar (sm#1665 appendix I):
 * unseen ready and lane-done alerts plus fresh Needs you tickets.
 */
object BoardBadge {
    var count by mutableStateOf(0)
}

/** A `/board` link or a tapped board alert waiting for the Board tab; the lane to show, or 0 for none. */
object BoardLinkRequests {
    var pending by mutableStateOf<Long?>(null)
}

/** The lane a `/board` path points at: `/board#lane-7` gives 7, plain `/board` gives 0. */
fun boardLaneFromPath(path: String): Long? {
    val bare = path.substringBefore('#').substringBefore('?')
    if (bare != "/board" && bare != "/board/") return null
    return path.substringAfter('#', "").removePrefix("lane-").toLongOrNull() ?: 0L
}

/** The lane an opened `https://<linkHost>/board…` link points at, or null when it is not a board link. */
fun boardLaneForLink(url: String, linkHost: String): Long? {
    val uri = runCatching { java.net.URI(url) }.getOrNull() ?: return null
    if (linkHost.isBlank() || !uri.scheme.equals("https", ignoreCase = true) ||
        !uri.host.equals(linkHost.trim(), ignoreCase = true)
    ) return null
    return boardLaneFromPath(uri.rawPath.orEmpty() + uri.rawFragment?.let { "#$it" }.orEmpty())
}

/** Refreshes [BoardBadge] for every screen; never marks the board seen. */
class BoardBadgeRefresher(application: Application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)

    suspend fun refresh() {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) return
        runCatching { repository.fetchBoardBadge(serverUrl, token) }
            .onSuccess { BoardBadge.count = it.count }
    }
}

/** Start for one ticket: the sheet's prefilled values, then the request. */
data class BoardStartState(
    val ticket: BoardTicket,
    val options: BoardStartOptions? = null,
    val busy: Boolean = false,
    val error: String? = null,
)

data class BoardUiState(
    val board: BoardResponse? = null,
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    /** Showing the last board from [ScreenCache] while the first read runs (spec 1782 J5). */
    val revalidating: Boolean = false,
    val lastUpdated: OffsetDateTime? = null,
    val error: String? = null,
    val signedOut: Boolean = false,
    /** A lane move, add or end in flight. */
    val busy: Boolean = false,
    val start: BoardStartState? = null,
    /** The queue, read with the board, so rows can show each agent's jobs. */
    val queue: li.rajeshgo.sm.data.model.QueueOverview? = null,
)

class BoardViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)
    private var refreshJob: Job? = null

    private val _uiState = MutableStateFlow(
        ScreenCache.board.let { board ->
            BoardUiState(board = board, loading = board == null, revalidating = board != null, queue = ScreenCache.queue)
        },
    )
    val uiState: StateFlow<BoardUiState> = _uiState

    /** Lanes the owner opened or closed; the rest follow the default, lane 1 open. */
    private val _expanded = MutableStateFlow<Map<Long, Boolean>>(emptyMap())
    val expanded: StateFlow<Map<Long, Boolean>> = _expanded

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            _uiState.update { it.copy(loading = false, refreshing = false, revalidating = false, signedOut = true, busy = false, start = null) }
            return null
        }
        return serverUrl to token
    }

    private suspend fun handleAuth(error: Throwable): Boolean {
        if (error !is SessionManagerAuthException) return false
        settingsRepository.clearAuth()
        // Signed out: a Start sheet mid-request would otherwise stay locked.
        _uiState.update { it.copy(loading = false, refreshing = false, revalidating = false, signedOut = true, start = null) }
        return true
    }

    suspend fun docReaderAuth(): DocReaderAuth? = loadDocReaderAuth(settingsRepository)

    fun isExpanded(laneId: Long, rank: Int): Boolean = _expanded.value[laneId] ?: (rank == 1)

    fun toggle(laneId: Long, rank: Int) {
        val open = isExpanded(laneId, rank)
        _expanded.update { it + (laneId to !open) }
    }

    fun expand(laneId: Long) {
        _expanded.update { it + (laneId to true) }
    }

    /**
     * Fetches the board, then marks it seen: only the Board tab calls this,
     * and only while it shows. A pull first asks sm to read GitHub now.
     */
    fun refresh(pull: Boolean = false) {
        if (refreshJob?.isActive == true) return
        if (pull) _uiState.update { it.copy(refreshing = true) }
        refreshJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            if (pull) {
                repository.refreshBoard(url, token).onFailure { if (handleAuth(it)) return@launch }
                delay(READ_SETTLE_MS)
            }
            load(url, token)
        }
    }

    private suspend fun load(url: String, token: String) {
        // Best effort: the rows keep the last queue if this read fails.
        runCatching { repository.fetchQueue(url, token) }
            .onSuccess { queue -> _uiState.update { it.copy(queue = queue) } }
            .onFailure { if (it is CancellationException) throw it }
        runCatching { repository.fetchBoard(url, token) }
            .onSuccess { board -> show(board) }
            .onFailure { error ->
                if (error is CancellationException) throw error
                if (!handleAuth(error)) {
                    _uiState.update {
                        it.copy(loading = false, refreshing = false, revalidating = false, error = error.message ?: "Couldn't load the board")
                    }
                }
                return
            }
        // The unseen edges stay on the lanes just fetched; the next fetch drops them.
        repository.markBoardSeen(url, token).onSuccess { BoardBadge.count = 0 }
    }

    private fun show(board: BoardResponse) {
        BoardBadge.count = board.unseen.count
        ScreenCache.board = board
        _uiState.update {
            it.copy(
                board = board,
                loading = false,
                refreshing = false,
                revalidating = false,
                error = null,
                signedOut = false,
                lastUpdated = OffsetDateTime.now(),
            )
        }
    }

    /** Swaps the lane with its neighbour ([delta] -1 up, +1 down) and sends the full order. */
    fun move(laneId: Long, delta: Int, onError: (String) -> Unit) {
        val lanes = _uiState.value.board?.lanes ?: return
        val ids = lanes.map { it.id }.toMutableList()
        val from = ids.indexOf(laneId)
        val to = from + delta
        if (from < 0 || to !in ids.indices) return
        ids[from] = ids[to].also { ids[to] = ids[from] }
        write(onError) { url, token -> repository.reorderBoard(url, token, ids).map { show(it) } }
    }

    fun endLane(laneId: Long, onError: (String) -> Unit) {
        write(onError) { url, token -> repository.endBoardLane(url, token, laneId).map { show(it) } }
    }

    fun addLane(repo: String, number: Long, onDone: (String?) -> Unit) {
        write({ onDone(it) }) { url, token ->
            repository.addBoardLane(url, token, repo, number).map { added ->
                load(url, token)
                onDone(null)
                added
            }
        }
    }

    private fun write(onError: (String) -> Unit, block: suspend (String, String) -> Result<*>) {
        if (_uiState.value.busy) return
        _uiState.update { it.copy(busy = true) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch _uiState.update { it.copy(busy = false) }
            block(url, token).onFailure { error ->
                if (!handleAuth(error)) onError(error.message ?: "Request failed")
                // A refused write means the board moved on; show what it is now.
                load(url, token)
            }
            _uiState.update { it.copy(busy = false) }
        }
    }

    fun openStart(ticket: BoardTicket) {
        _uiState.update { it.copy(start = BoardStartState(ticket)) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            runCatching { repository.fetchBoardStartOptions(url, token, ticket.repo, ticket.number) }
                .onSuccess { options -> updateStart(ticket) { it.copy(options = options) } }
                .onFailure { error ->
                    if (!handleAuth(error)) updateStart(ticket) { it.copy(error = error.message ?: "Couldn't prepare Start") }
                }
        }
    }

    fun closeStart() {
        _uiState.update { if (it.start?.busy == true) it else it.copy(start = null) }
    }

    private fun updateStart(ticket: BoardTicket, change: (BoardStartState) -> BoardStartState) {
        _uiState.update { state ->
            val start = state.start?.takeIf { it.ticket.repo == ticket.repo && it.ticket.number == ticket.number }
            if (start == null) state else state.copy(start = change(start))
        }
    }

    suspend fun sessionModels(provider: String, workingDir: String): List<String> {
        val (url, token) = credentials() ?: return emptyList()
        return repository.fetchSessionModels(url, token, provider, workingDir)
    }

    fun start(request: BoardStartRequest, onStarted: (String) -> Unit) {
        val ticket = _uiState.value.start?.ticket ?: return
        updateStart(ticket) { it.copy(busy = true, error = null) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            repository.startBoardTicket(url, token, request)
                .onSuccess { started ->
                    _uiState.update { it.copy(start = null) }
                    onStarted(started.name)
                }
                .onFailure { error ->
                    if (!handleAuth(error)) {
                        updateStart(ticket) { it.copy(busy = false, error = error.message ?: "Start failed") }
                    }
                }
            // After a refusal (the ticket was taken or is no longer ready) or a start, show the board as it is now.
            load(url, token)
        }
    }
}
