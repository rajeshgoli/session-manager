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
import li.rajeshgo.sm.data.repository.AppUpdateRepository
import li.rajeshgo.sm.data.repository.AvailableAppUpdate
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.security.DeviceKeyManager

data class SettingsUiState(
    val serverUrl: String = "",
    val userEmail: String = "",
    val userName: String = "",
    val isLoggedIn: Boolean = false,
    val loading: Boolean = false,
    val bootstrap: ClientBootstrapResponse? = null,
    val availableUpdate: AvailableAppUpdate? = null,
    val mobileDeviceKeyId: String = "",
    val mobileDevicePublicKey: String = "",
    val mobileDeviceKeyError: String? = null,
    val updateInstalling: Boolean = false,
    val updateError: String? = null,
    val error: String? = null,
)

class SettingsViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val sessionRepository = SessionManagerRepository()
    private val appUpdateRepository = AppUpdateRepository(application, settingsRepository)
    private val deviceKeyManager = DeviceKeyManager()

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
            loadMobileDeviceKey()
            refreshBootstrap()
            refreshUpdate()
        }
    }

    fun updateServerUrl(value: String) {
        _uiState.value = _uiState.value.copy(serverUrl = value, error = null)
    }

    fun refreshBootstrap() {
        val serverUrl = _uiState.value.serverUrl.trim().trimEnd('/')
        if (serverUrl.isBlank()) {
            _uiState.value = _uiState.value.copy(bootstrap = null, availableUpdate = null, updateError = null)
            return
        }
        viewModelScope.launch {
            runCatching {
                settingsRepository.saveServerUrl(serverUrl)
                sessionRepository.fetchBootstrap(serverUrl)
            }.onSuccess { bootstrap ->
                _uiState.value = _uiState.value.copy(bootstrap = bootstrap, error = null, serverUrl = serverUrl)
                refreshUpdate()
            }.onFailure { error ->
                _uiState.value = _uiState.value.copy(error = error.message ?: "Failed to load bootstrap")
            }
        }
    }

    fun loadMobileDeviceKey() {
        runCatching {
            deviceKeyManager.deviceKeyId() to deviceKeyManager.publicKeyPem()
        }.onSuccess { (keyId, publicKey) ->
            _uiState.value = _uiState.value.copy(
                mobileDeviceKeyId = keyId,
                mobileDevicePublicKey = publicKey,
                mobileDeviceKeyError = null,
            )
        }.onFailure { error ->
            _uiState.value = _uiState.value.copy(
                mobileDeviceKeyError = error.message ?: "Failed to load mobile attach device key",
            )
        }
    }

    fun refreshUpdate() {
        viewModelScope.launch {
            runCatching { appUpdateRepository.getAvailableUpdate() }
                .onSuccess { update ->
                    _uiState.value = _uiState.value.copy(availableUpdate = update, updateError = null)
                }
                .onFailure { error ->
                    _uiState.value = _uiState.value.copy(updateError = error.message ?: "Failed to check app update")
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
                refreshUpdate()
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

    fun dismissUpdate() {
        val update = _uiState.value.availableUpdate ?: return
        viewModelScope.launch {
            appUpdateRepository.dismissUpdate(update.artifactHash)
            _uiState.value = _uiState.value.copy(availableUpdate = null, updateError = null)
        }
    }

    fun installUpdate() {
        val update = _uiState.value.availableUpdate ?: return
        if (_uiState.value.updateInstalling) {
            return
        }
        _uiState.value = _uiState.value.copy(updateInstalling = true, updateError = null)
        viewModelScope.launch {
            runCatching {
                val apkFile = appUpdateRepository.downloadUpdate(update)
                appUpdateRepository.launchInstaller(apkFile)
            }.onFailure { error ->
                _uiState.value = _uiState.value.copy(
                    updateInstalling = false,
                    updateError = error.message ?: "Update failed",
                )
                return@launch
            }
            _uiState.value = _uiState.value.copy(updateInstalling = false)
        }
    }

    fun clearUpdateError() {
        _uiState.value = _uiState.value.copy(updateError = null)
    }
}
