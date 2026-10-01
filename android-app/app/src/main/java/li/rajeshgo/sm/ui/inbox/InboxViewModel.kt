package li.rajeshgo.sm.ui.inbox

import android.app.Application
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.InboxResponse
import li.rajeshgo.sm.data.model.InboxRow
import li.rajeshgo.sm.data.repository.ScreenCache
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.watch.DocReaderAuth
import li.rajeshgo.sm.ui.watch.loadDocReaderAuth

/**
 * The Inbox tab's badge, shared by every screen's bottom nav (sm#1647):
 * process-wide Compose state for the same reason as FollowOpenRequests.
 */
object InboxBadge {
    var needsYou by mutableStateOf(0)
    var hasNew by mutableStateOf(false)

    fun update(response: InboxResponse) {
        needsYou = response.needsYouCount
        hasNew = response.hasNew
    }
}

/** The Inbox filters, in chip order. */
enum class InboxFilter(val query: String, val label: String) {
    Open("open", "Open"),
    Docs("docs", "Docs"),
    Done("done", "Done"),
}

data class InboxUiState(
    val filter: InboxFilter = InboxFilter.Open,
    val rows: List<InboxRow> = emptyList(),
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    /** Showing this filter's last rows from [ScreenCache] while the first read runs (spec 1782 J5). */
    val revalidating: Boolean = false,
    val error: String? = null,
    val signedOut: Boolean = false,
    /** Review requests no reviewer could take (sm#1768 D7), shown first on Open. */
    val noReviewer: List<li.rajeshgo.sm.data.model.NoReviewerRequest> = emptyList(),
    /** A Retry now, Review it myself or Dismiss in flight, by request id. */
    val reviewBusy: String? = null,
)

/** The Inbox as last seen for [filter], or a loading state when it has never been read. */
fun cachedInboxState(filter: InboxFilter): InboxUiState {
    val cached = ScreenCache.inbox[filter.query]
    return InboxUiState(filter = filter, rows = cached?.rows.orEmpty(), loading = cached == null, revalidating = cached != null)
}

/** Open rows in their groups, in order; other filters are one untitled group. */
fun inboxSections(filter: InboxFilter, rows: List<InboxRow>): List<Pair<String?, List<InboxRow>>> {
    if (filter != InboxFilter.Open) return if (rows.isEmpty()) emptyList() else listOf(null to rows)
    return listOf("needs_you" to "NEEDS YOU", "finished" to "FINISHED", "new" to "NEW", "earlier" to "EARLIER", "folded" to "FOLDED")
        .mapNotNull { (group, label) ->
            rows.filter { it.group == group }.takeIf { it.isNotEmpty() }?.let { "$label · ${it.size}" to it }
        }
}

class InboxViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)
    private var loadJob: Job? = null

    private val _uiState = MutableStateFlow(cachedInboxState(InboxFilter.Open))
    val uiState: StateFlow<InboxUiState> = _uiState

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            _uiState.update { it.copy(loading = false, refreshing = false, revalidating = false, signedOut = true) }
            return null
        }
        return serverUrl to token
    }

    suspend fun docReaderAuth(): DocReaderAuth? = loadDocReaderAuth(settingsRepository)

    fun setFilter(filter: InboxFilter) {
        if (filter == _uiState.value.filter) return
        _uiState.value = cachedInboxState(filter)
        loadJob?.cancel()
        loadJob = null
        refresh()
    }

    /** Reloads the current filter; `pull` shows the pull-to-refresh spinner. */
    fun refresh(pull: Boolean = false) {
        if (loadJob?.isActive == true) return
        if (pull) _uiState.update { it.copy(refreshing = true) }
        val filter = _uiState.value.filter
        loadJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            // An older server has no review status; the Inbox still loads.
            val noReviewer = if (filter != InboxFilter.Open) emptyList() else runCatching {
                repository.fetchReviewStatus(url, token).needsYou
            }.getOrElse { if (it is CancellationException) throw it else _uiState.value.noReviewer }
            runCatching { repository.fetchInbox(url, token, filter.query) }
                .onSuccess { response ->
                    InboxBadge.update(response)
                    _uiState.update {
                        if (it.filter != filter) it
                        else it.copy(rows = response.rows, noReviewer = noReviewer, loading = false, refreshing = false, revalidating = false, error = null, signedOut = false)
                    }
                }
                .onFailure { error ->
                    // A filter change cancelled this load; the new one owns the state.
                    if (error is CancellationException) throw error
                    if (_uiState.value.filter != filter) return@onFailure
                    if (error is SessionManagerAuthException) {
                        settingsRepository.clearAuth()
                        _uiState.update { it.copy(loading = false, refreshing = false, revalidating = false, signedOut = true) }
                    } else {
                        _uiState.update {
                            it.copy(loading = false, refreshing = false, revalidating = false, error = error.message ?: "Couldn't load the Inbox")
                        }
                    }
                }
        }
    }

    /** The no-reviewer item's three answers that go to the server. */
    enum class NoReviewerAnswer { Retry, Owner, Dismiss }

    /** Sends [answer] for the no-reviewer item; [onResult] gets the failure's text, or null. */
    fun answerNoReviewer(requestId: String, answer: NoReviewerAnswer, onResult: (String?) -> Unit) {
        if (_uiState.value.reviewBusy != null) return
        _uiState.update { it.copy(reviewBusy = requestId) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            val result = when (answer) {
                NoReviewerAnswer.Retry -> repository.retryReviewRequest(url, token, requestId)
                NoReviewerAnswer.Owner -> repository.ownReviewRequest(url, token, requestId)
                NoReviewerAnswer.Dismiss -> repository.dismissReviewRequest(url, token, requestId)
            }
            _uiState.update { it.copy(reviewBusy = null) }
            onResult(result.exceptionOrNull()?.let { it.message ?: "Request failed" })
            loadJob?.cancel()
            loadJob = null
            refresh()
        }
    }

    /** Done: the row leaves Open at once; a failure puts it back and says why. */
    fun markDone(row: InboxRow, onResult: (String?) -> Unit) {
        val before = _uiState.value.rows
        if (_uiState.value.filter == InboxFilter.Open) {
            _uiState.update { state -> state.copy(rows = state.rows.filterNot { it.threadKey == row.threadKey }) }
        }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            repository.markInboxDone(url, token, row.threadKey)
                .onSuccess {
                    onResult(null)
                    loadJob?.cancel()
                    loadJob = null
                    refresh()
                }
                .onFailure { error ->
                    _uiState.update { it.copy(rows = before) }
                    onResult(error.message ?: "Done failed")
                }
        }
    }

    fun archive(row: InboxRow, onResult: (String?) -> Unit) {
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            repository.archiveInbox(url, token, row.threadKey, row.foldedBy == "archived")
                .onSuccess { onResult(null); loadJob?.cancel(); loadJob = null; refresh() }
                .onFailure { onResult(it.message ?: "Archive failed") }
        }
    }
}

/** Refreshes [InboxBadge] for screens other than the Inbox. */
class InboxBadgeRefresher(application: Application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)

    suspend fun refresh() {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) return
        runCatching { repository.fetchInbox(serverUrl, token, InboxFilter.Open.query) }
            .onSuccess(InboxBadge::update)
    }
}
