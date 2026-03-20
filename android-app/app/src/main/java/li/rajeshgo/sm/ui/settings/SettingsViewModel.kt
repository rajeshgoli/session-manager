package li.rajeshgo.sm.ui.settings

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlin.Result
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

data class SettingsUiState(
    val serverUrl: String = "",
    val userEmail: String = "",
    val userName: String = "",
    val isLoggedIn: Boolean = false,
    val loading: Boolean = false,
    val bootstrap: ClientBootstrapResponse? = null,
    val error: String? = null,
)

class SettingsViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val sessionRepository = SessionManagerRepository()

    private val _uiState = MutableStateFlow(SettingsUiState())
    val uiState: StateFlow<SettingsUiState> = _uiState

    init {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(
                serverUrl = settingsRepository.serverUrl.first(),
                userEmail = settingsRepository.userEmail.first(),
                userName = settingsRepository.userName.first(),
                isLoggedIn = settingsRepository.isLoggedIn.first(),
            )
            refreshBootstrap()
        }
    }

    fun updateServerUrl(value: String) {
        _uiState.value = _uiState.value.copy(serverUrl = value, error = null)
    }

    fun refreshBootstrap() {
        val serverUrl = _uiState.value.serverUrl.trim().trimEnd('/')
        if (serverUrl.isBlank()) {
            _uiState.value = _uiState.value.copy(bootstrap = null)
            return
        }
        viewModelScope.launch {
            runCatching {
                settingsRepository.saveServerUrl(serverUrl)
                sessionRepository.fetchBootstrap(serverUrl)
            }.onSuccess { bootstrap ->
                _uiState.value = _uiState.value.copy(bootstrap = bootstrap, error = null, serverUrl = serverUrl)
            }.onFailure { error ->
                _uiState.value = _uiState.value.copy(error = error.message ?: "Failed to load bootstrap")
            }
        }
    }

    fun exchangeGoogleIdToken(idToken: String, onSuccess: () -> Unit) {
        val serverUrl = _uiState.value.serverUrl.trim().trimEnd('/')
        if (serverUrl.isBlank()) {
            _uiState.value = _uiState.value.copy(error = "Server URL is required")
            return
        }
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(loading = true, error = null)
            runCatching {
                settingsRepository.saveServerUrl(serverUrl)
                sessionRepository.exchangeGoogleIdToken(serverUrl, idToken)
            }.onSuccess { auth ->
                settingsRepository.saveAuth(auth.accessToken, auth.email, auth.name, auth.expiresAt)
                _uiState.value = _uiState.value.copy(
                    serverUrl = serverUrl,
                    userEmail = auth.email,
                    userName = auth.name.orEmpty(),
                    isLoggedIn = true,
                    loading = false,
                    error = null,
                )
                onSuccess()
            }.onFailure { error ->
                _uiState.value = _uiState.value.copy(
                    loading = false,
                    error = error.message ?: "Google sign-in failed",
                )
            }
        }
    }


    fun reportError(message: String) {
        _uiState.value = _uiState.value.copy(error = message, loading = false)
    }

    fun finishLogout() {
        viewModelScope.launch {
            settingsRepository.clearAuth()
            _uiState.value = _uiState.value.copy(
                isLoggedIn = false,
                userEmail = "",
                userName = "",
                error = null,
            )
        }
    }
}
