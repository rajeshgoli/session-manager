package li.rajeshgo.sm.ui.guestbook

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
import li.rajeshgo.sm.data.model.GuestbookClaim
import li.rajeshgo.sm.data.model.GuestbookEntry
import li.rajeshgo.sm.data.model.GuestbookResponse
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.watch.DocReaderAuth
import li.rajeshgo.sm.ui.watch.loadDocReaderAuth

data class GuestbookUiState(
    /** The repo filter, `owner/name` or bare `name`; null shows every repo. */
    val repo: String? = null,
    val entries: List<GuestbookEntry> = emptyList(),
    /** The cursor for the next older page; null once the last page is in. */
    val nextBefore: Long? = null,
    val loading: Boolean = true,
    val loadingMore: Boolean = false,
    val refreshing: Boolean = false,
    val error: String? = null,
    val signedOut: Boolean = false,
)

/** [page] after [existing]; an entry already listed keeps its place. */
fun appendGuestbookPage(existing: List<GuestbookEntry>, page: List<GuestbookEntry>): List<GuestbookEntry> {
    val seen = existing.mapTo(HashSet()) { it.id }
    return existing + page.filter { seen.add(it.id) }
}

/** The ticket page a claim opens in the reader: `/t/<repo name>/<number>`. */
fun guestbookClaimPath(claim: GuestbookClaim): String = "/t/${repoName(claim.repo)}/${claim.number}"

/** "#12" for a ticket, "PR #12" for a pull request. */
fun guestbookClaimLabel(claim: GuestbookClaim): String = (if (claim.kind == "pr") "PR #" else "#") + claim.number

/** "claude · claude-opus-5-5", or the provider alone. */
fun guestbookModelLabel(entry: GuestbookEntry): String =
    entry.model?.takeIf { it.isNotBlank() }?.let { "${entry.provider} · $it" } ?: entry.provider

fun repoName(repo: String): String = repo.substringAfterLast('/')

class GuestbookViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)
    private var loadJob: Job? = null

    private val _uiState = MutableStateFlow(GuestbookUiState())
    val uiState: StateFlow<GuestbookUiState> = _uiState

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

    fun setRepo(repo: String?) {
        val filter = repo?.trim()?.takeIf { it.isNotEmpty() }
        if (filter == _uiState.value.repo) return
        loadJob?.cancel()
        loadJob = null
        _uiState.update { GuestbookUiState(repo = filter) }
        refresh()
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

    /** After a failed older page, tries it again. */
    fun retryMore() {
        _uiState.update { it.copy(error = null) }
        loadMore()
    }

    private fun load(before: Long?) {
        val repo = _uiState.value.repo
        loadJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            runCatching { repository.fetchGuestbook(url, token, repo, before) }
                .onSuccess { page -> apply(repo, before, page) }
                .onFailure { error ->
                    if (error is CancellationException) throw error
                    if (error is SessionManagerAuthException) {
                        settingsRepository.clearAuth()
                        _uiState.update { it.copy(loading = false, loadingMore = false, refreshing = false, signedOut = true) }
                    } else {
                        _uiState.update {
                            it.copy(
                                loading = false,
                                loadingMore = false,
                                refreshing = false,
                                error = error.message ?: "Couldn't load the Guestbook",
                            )
                        }
                    }
                }
        }
    }

    private fun apply(repo: String?, before: Long?, page: GuestbookResponse) {
        _uiState.update {
            if (it.repo != repo) return@update it
            it.copy(
                entries = if (before == null) page.entries else appendGuestbookPage(it.entries, page.entries),
                nextBefore = page.nextBefore,
                loading = false,
                loadingMore = false,
                refreshing = false,
                error = null,
                signedOut = false,
            )
        }
    }
}
