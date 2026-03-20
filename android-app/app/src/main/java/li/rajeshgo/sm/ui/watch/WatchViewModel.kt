package li.rajeshgo.sm.ui.watch

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

data class WatchUiState(
    val serverUrl: String = "",
    val userEmail: String = "",
    val bootstrap: ClientBootstrapResponse? = null,
    val sessions: List<ClientSession> = emptyList(),
    val expandedSessionIds: Set<String> = emptySet(),
    val detailsBySessionId: Map<String, SessionDetail> = emptyMap(),
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    val lastSync: String? = null,
    val error: String? = null,
)

class WatchViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val sessionRepository = SessionManagerRepository()
    private var refreshJob: Job? = null

    private val _uiState = MutableStateFlow(WatchUiState())
    val uiState: StateFlow<WatchUiState> = _uiState

    init {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            val userEmail = settingsRepository.userEmail.first()
            if (serverUrl.isBlank() || accessToken.isBlank()) {
                _uiState.value = _uiState.value.copy(loading = false, userEmail = userEmail)
                return@launch
            }
            val bootstrap = runCatching { sessionRepository.fetchBootstrap(serverUrl) }.getOrNull()
            _uiState.value = _uiState.value.copy(serverUrl = serverUrl, userEmail = userEmail, bootstrap = bootstrap)
            refresh(initial = true)
        }
    }

    fun refresh(initial: Boolean = false) {
        if (refreshJob?.isActive == true) {
            return
        }
        refreshJob = viewModelScope.launch {
            try {
                val serverUrl = settingsRepository.serverUrl.first()
                val accessToken = settingsRepository.accessToken.first()
                val userEmail = settingsRepository.userEmail.first()
                if (serverUrl.isBlank() || accessToken.isBlank()) {
                    _uiState.value = _uiState.value.copy(
                        loading = false,
                        refreshing = false,
                        userEmail = userEmail,
                        error = "Sign in to load sessions",
                    )
                    return@launch
                }
                _uiState.value = _uiState.value.copy(loading = initial, refreshing = !initial, error = null)
                val expandedSessionIds = _uiState.value.expandedSessionIds
                runCatching { sessionRepository.fetchSessions(serverUrl, accessToken) }
                    .onSuccess { sessions ->
                        val sessionIds = sessions.map { it.id }.toSet()
                        val preservedDetails = _uiState.value.detailsBySessionId.filterKeys { it in sessionIds }
                        _uiState.value = _uiState.value.copy(
                            sessions = sessions,
                            detailsBySessionId = preservedDetails,
                            loading = false,
                            refreshing = false,
                            userEmail = userEmail,
                            lastSync = java.time.OffsetDateTime.now().toString(),
                            error = null,
                        )
                        sessions
                            .filter { it.id in expandedSessionIds && it.id !in preservedDetails }
                            .forEach { loadDetail(it) }
                    }
                    .onFailure { error ->
                        _uiState.value = _uiState.value.copy(
                            loading = false,
                            refreshing = false,
                            userEmail = userEmail,
                            error = error.message ?: "Failed to refresh sessions",
                        )
                    }
            } finally {
                refreshJob = null
            }
        }
    }

    fun toggleExpanded(session: ClientSession) {
        val expanded = _uiState.value.expandedSessionIds.toMutableSet()
        if (!expanded.add(session.id)) {
            expanded.remove(session.id)
        } else if (!_uiState.value.detailsBySessionId.containsKey(session.id)) {
            loadDetail(session)
        }
        _uiState.value = _uiState.value.copy(expandedSessionIds = expanded)
    }

    fun loadDetail(session: ClientSession) {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            if (serverUrl.isBlank() || accessToken.isBlank()) {
                return@launch
            }
            runCatching { sessionRepository.fetchSessionDetail(serverUrl, accessToken, session) }
                .onSuccess { detail ->
                    _uiState.value = _uiState.value.copy(
                        detailsBySessionId = _uiState.value.detailsBySessionId + (session.id to detail)
                    )
                }
                .onFailure { error ->
                    _uiState.value = _uiState.value.copy(
                        detailsBySessionId = _uiState.value.detailsBySessionId + (session.id to SessionDetail(lastError = error.message))
                    )
                }
        }
    }

    fun killSession(sessionId: String, onComplete: (Result<Unit>) -> Unit) {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            val result = if (serverUrl.isBlank() || accessToken.isBlank()) {
                Result.failure(IllegalStateException("Sign in to kill sessions"))
            } else {
                sessionRepository.killSession(serverUrl, accessToken, sessionId)
            }
            if (result.isSuccess) {
                _uiState.value = _uiState.value.copy(
                    sessions = _uiState.value.sessions.filterNot { it.id == sessionId },
                    expandedSessionIds = _uiState.value.expandedSessionIds - sessionId,
                    detailsBySessionId = _uiState.value.detailsBySessionId - sessionId,
                )
            }
            onComplete(result)
        }
    }
}
