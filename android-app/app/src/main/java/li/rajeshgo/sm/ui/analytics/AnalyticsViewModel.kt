package li.rajeshgo.sm.ui.analytics

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.AnalyticsSummary
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerBackendUnavailableException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SessionManagerTransientException
import li.rajeshgo.sm.data.repository.SettingsRepository

data class AnalyticsUiState(
    val serverUrl: String = "",
    val userEmail: String = "",
    val summary: AnalyticsSummary? = null,
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    val error: String? = null,
)

class AnalyticsViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val sessionRepository = SessionManagerRepository()
    private var refreshJob: Job? = null

    private val _uiState = MutableStateFlow(AnalyticsUiState())
    val uiState: StateFlow<AnalyticsUiState> = _uiState

    init {
        refresh(initial = true)
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
                        error = "Sign in to load analytics",
                    )
                    return@launch
                }
                _uiState.value = _uiState.value.copy(loading = initial, refreshing = !initial, error = null)
                runCatching { sessionRepository.fetchAnalytics(serverUrl, accessToken) }
                    .onSuccess { summary ->
                        _uiState.value = _uiState.value.copy(
                            serverUrl = serverUrl,
                            userEmail = userEmail,
                            summary = summary,
                            loading = false,
                            refreshing = false,
                            error = null,
                        )
                    }
                    .onFailure { error ->
                        when (error) {
                            is SessionManagerAuthException -> {
                                settingsRepository.clearAuth()
                                _uiState.value = _uiState.value.copy(
                                    summary = null,
                                    loading = false,
                                    refreshing = false,
                                    userEmail = "",
                                    error = error.message ?: "Session expired. Sign in again.",
                                )
                            }
                            is SessionManagerBackendUnavailableException -> {
                                _uiState.value = _uiState.value.copy(
                                    summary = null,
                                    loading = false,
                                    refreshing = false,
                                    userEmail = userEmail,
                                    error = error.message ?: "Session Manager backend is unreachable from ingress.",
                                )
                            }
                            is SessionManagerTransientException -> {
                                _uiState.value = _uiState.value.copy(
                                    loading = false,
                                    refreshing = false,
                                    userEmail = userEmail,
                                    error = error.message ?: "Server temporarily unavailable. Retrying soon.",
                                )
                            }
                            else -> {
                                _uiState.value = _uiState.value.copy(
                                    loading = false,
                                    refreshing = false,
                                    userEmail = userEmail,
                                    error = error.message ?: "Failed to load analytics",
                                )
                            }
                        }
                    }
            } finally {
                refreshJob = null
            }
        }
    }
}
