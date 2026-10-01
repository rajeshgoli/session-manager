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
    val error: String? = null,
    val signedOut: Boolean = false,
)

/** Open rows in their groups, in order; other filters are one untitled group. */
fun inboxSections(filter: InboxFilter, rows: List<InboxRow>): List<Pair<String?, List<InboxRow>>> {
    if (filter != InboxFilter.Open) return if (rows.isEmpty()) emptyList() else listOf(null to rows)
    return listOf("needs_you" to "NEEDS YOU", "finished" to "FINISHED", "new" to "NEW", "earlier" to "EARLIER")
        .mapNotNull { (group, label) ->
            rows.filter { it.group == group }.takeIf { it.isNotEmpty() }?.let { "$label · ${it.size}" to it }
        }
}

class InboxViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)
    private var loadJob: Job? = null

    private val _uiState = MutableStateFlow(InboxUiState())
    val uiState: StateFlow<InboxUiState> = _uiState

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            _uiState.update { it.copy(loading = false, refreshing = false, signedOut = true) }
            return null
        }
        return serverUrl to token
    }

    suspend fun docReaderAuth(): DocReaderAuth? = loadDocReaderAuth(settingsRepository)

    fun setFilter(filter: InboxFilter) {
        if (filter == _uiState.value.filter) return
        _uiState.update { it.copy(filter = filter, rows = emptyList(), loading = true, error = null) }
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
            runCatching { repository.fetchInbox(url, token, filter.query) }
                .onSuccess { response ->
                    InboxBadge.update(response)
                    _uiState.update {
                        if (it.filter != filter) it
                        else it.copy(rows = response.rows, loading = false, refreshing = false, error = null, signedOut = false)
                    }
                }
                .onFailure { error ->
                    // A filter change cancelled this load; the new one owns the state.
                    if (error is CancellationException) throw error
                    if (_uiState.value.filter != filter) return@onFailure
                    if (error is SessionManagerAuthException) {
                        settingsRepository.clearAuth()
                        _uiState.update { it.copy(loading = false, refreshing = false, signedOut = true) }
                    } else {
                        _uiState.update {
                            it.copy(loading = false, refreshing = false, error = error.message ?: "Couldn't load the Inbox")
                        }
                    }
                }
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
