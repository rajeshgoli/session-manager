package li.rajeshgo.sm.ui.analytics

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.repository.SettingsRepository

/** Which Analytics section shows; remembered across visits. */
class AnalyticsViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private var opened = false

    private val _section = MutableStateFlow<AnalyticsSection?>(null)
    val section: StateFlow<AnalyticsSection?> = _section

    /** Picks the first section once per screen visit; [argument] is the route's `section`. */
    fun open(argument: String?) {
        if (opened) return
        opened = true
        viewModelScope.launch {
            val section = resolveAnalyticsSection(argument, settingsRepository.loadAnalyticsSection())
            _section.value = section
            settingsRepository.saveAnalyticsSection(section.key)
        }
    }

    fun select(section: AnalyticsSection) {
        _section.value = section
        viewModelScope.launch { settingsRepository.saveAnalyticsSection(section.key) }
    }
}
