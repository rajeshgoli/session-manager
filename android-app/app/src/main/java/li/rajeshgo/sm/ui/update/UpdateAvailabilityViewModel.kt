package li.rajeshgo.sm.ui.update

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.repository.AppUpdateRepository
import li.rajeshgo.sm.data.repository.AvailableAppUpdate
import li.rajeshgo.sm.data.repository.SettingsRepository

data class UpdateAvailabilityUiState(
    val availableUpdate: AvailableAppUpdate? = null,
    val checking: Boolean = false,
    val error: String? = null,
)

class UpdateAvailabilityViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val appUpdateRepository = AppUpdateRepository(application, settingsRepository)
    private var refreshJob: Job? = null

    private val _uiState = MutableStateFlow(UpdateAvailabilityUiState())
    val uiState: StateFlow<UpdateAvailabilityUiState> = _uiState

    init {
        refresh()
    }

    fun refresh() {
        if (refreshJob?.isActive == true) {
            return
        }
        refreshJob = viewModelScope.launch {
            _uiState.value = _uiState.value.copy(checking = true, error = null)
            runCatching { appUpdateRepository.getAvailableUpdate() }
                .onSuccess { update ->
                    _uiState.value = UpdateAvailabilityUiState(
                        availableUpdate = update,
                        checking = false,
                        error = null,
                    )
                }
                .onFailure { error ->
                    _uiState.value = UpdateAvailabilityUiState(
                        availableUpdate = null,
                        checking = false,
                        error = error.message,
                    )
                }
            refreshJob = null
        }
    }
}
