package li.rajeshgo.sm.ui.history

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.AgentHistoryResponse
import li.rajeshgo.sm.data.model.AgentHistoryRow
import li.rajeshgo.sm.data.model.AgentWork
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.watch.DocReaderAuth
import li.rajeshgo.sm.ui.watch.loadDocReaderAuth

/** Where one row's Restore stands. */
sealed interface RestoreState {
    data object Restoring : RestoreState
    data object Restored : RestoreState
    data class Failed(val message: String) : RestoreState
}

data class HistoryUiState(
    /** The search: name, alias, role, working directory or id prefix; null lists every agent. */
    val query: String? = null,
    val agents: List<AgentHistoryRow> = emptyList(),
    /** Agents matching [query] across every page. */
    val total: Int = 0,
    /** The cursor for the next older page; null once the last page is in. */
    val nextBefore: String? = null,
    val loading: Boolean = true,
    val loadingMore: Boolean = false,
    val refreshing: Boolean = false,
    val error: String? = null,
    val signedOut: Boolean = false,
    /** Rows the owner opened to see their work. */
    val expanded: Set<String> = emptySet(),
    /** Restore attempts this visit, by session id. */
    val restores: Map<String, RestoreState> = emptyMap(),
)

/** [page] after [existing]; an agent already listed keeps its place. */
fun appendHistoryPage(existing: List<AgentHistoryRow>, page: List<AgentHistoryRow>): List<AgentHistoryRow> {
    val seen = existing.mapTo(HashSet()) { it.id }
    return existing + page.filter { seen.add(it.id) }
}

/** "2 tickets · 1 PR · 3 docs", or null when the agent worked on nothing sm tracked. */
fun workSummary(work: AgentWork): String? = listOfNotNull(
    counted(work.tickets.size, "ticket"),
    counted(work.prs.size, "PR"),
    counted(work.docs.size, "doc"),
).joinToString(" · ").ifEmpty { null }

private fun counted(n: Int, noun: String): String? = if (n == 0) null else "$n $noun${if (n == 1) "" else "s"}"

/** The repo folder a working directory names: the last path segment. */
fun workingDirLabel(workingDir: String): String =
    workingDir.trimEnd('/').substringAfterLast('/').ifEmpty { workingDir }

/** "claude · claude-opus-5-5", or the provider alone. */
fun providerLabel(row: AgentHistoryRow): String =
    row.model?.takeIf { it.isNotBlank() }?.let { "${row.provider} · $it" } ?: row.provider

class HistoryViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)
    private var loadJob: Job? = null
    /** Whether the last failed load was an older page rather than the newest. */
    private var failedOlder = false

    private val _uiState = MutableStateFlow(HistoryUiState())
    val uiState: StateFlow<HistoryUiState> = _uiState

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            _uiState.update { it.copy(loading = false, loadingMore = false, refreshing = false, signedOut = true) }
            return null
        }
        return serverUrl to token
    }

    suspend fun docReaderAuth(): DocReaderAuth? = loadDocReaderAuth(settingsRepository)

    fun setQuery(query: String?) {
        val search = query?.trim()?.takeIf { it.isNotEmpty() }
        if (search == _uiState.value.query) return
        loadJob?.cancel()
        loadJob = null
        _uiState.update { HistoryUiState(query = search, restores = it.restores) }
        refresh()
    }

    fun toggleExpanded(id: String) {
        _uiState.update { it.copy(expanded = if (id in it.expanded) it.expanded - id else it.expanded + id) }
    }

    /** Reloads the newest page, replacing the list; `pull` shows the pull-to-refresh spinner. */
    fun refresh(pull: Boolean = false) {
        if (loadJob?.isActive == true && !_uiState.value.loadingMore) return
        loadJob?.cancel()
        _uiState.update { it.copy(refreshing = pull, loadingMore = false) }
        load(before = null)
    }

    /** Fetches the next older page, if there is one and nothing else is loading. */
    fun loadMore() {
        val state = _uiState.value
        val before = state.nextBefore ?: return
        if (loadJob?.isActive == true || state.error != null) return
        _uiState.update { it.copy(loadingMore = true) }
        load(before)
    }

    /** Tries the failed load again: the older page, or the newest after a failed refresh. */
    fun retry() {
        _uiState.update { it.copy(error = null) }
        if (failedOlder) loadMore() else refresh()
    }

    /**
     * Restores [id]. The row stays listed, marked restored, so the owner sees
     * it worked; the next refresh drops it, since it is live again.
     */
    fun restore(id: String) {
        if (_uiState.value.restores[id] in listOf(RestoreState.Restoring, RestoreState.Restored)) return
        _uiState.update { it.copy(restores = it.restores + (id to RestoreState.Restoring)) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: run {
                _uiState.update { it.copy(restores = it.restores - id) }
                return@launch
            }
            val result = repository.restoreSession(url, token, id)
            val error = result.exceptionOrNull()
            if (error is SessionManagerAuthException) settingsRepository.clearAuth()
            _uiState.update {
                it.copy(
                    restores = it.restores + (id to (error?.let { e -> RestoreState.Failed(e.message ?: "Restore failed") } ?: RestoreState.Restored)),
                    signedOut = it.signedOut || error is SessionManagerAuthException,
                )
            }
        }
    }

    private fun load(before: String?) {
        val query = _uiState.value.query
        loadJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            runCatching { repository.fetchAgentHistory(url, token, query, before) }
                .onSuccess { page -> apply(query, before, page) }
                .onFailure { error ->
                    if (error is CancellationException) throw error
                    failedOlder = before != null
                    if (error is SessionManagerAuthException) {
                        settingsRepository.clearAuth()
                        _uiState.update { it.copy(loading = false, loadingMore = false, refreshing = false, signedOut = true) }
                    } else {
                        _uiState.update {
                            it.copy(
                                loading = false,
                                loadingMore = false,
                                refreshing = false,
                                error = error.message ?: "Couldn't load History",
                            )
                        }
                    }
                }
        }
    }

    private fun apply(query: String?, before: String?, page: AgentHistoryResponse) {
        _uiState.update {
            if (it.query != query) return@update it
            it.copy(
                agents = if (before == null) page.agents else appendHistoryPage(it.agents, page.agents),
                total = page.total,
                nextBefore = page.nextBefore,
                loading = false,
                loadingMore = false,
                refreshing = false,
                error = null,
                signedOut = false,
                // A refresh starts over: restored agents have left the list.
                restores = if (before == null) it.restores.filterValues { r -> r is RestoreState.Restoring } else it.restores,
            )
        }
    }
}
