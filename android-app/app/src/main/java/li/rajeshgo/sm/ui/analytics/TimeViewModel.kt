package li.rajeshgo.sm.ui.analytics

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
import li.rajeshgo.sm.data.model.TimeNode
import li.rajeshgo.sm.data.model.TimeReport
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

class TimeViewModel(application: Application) : AndroidViewModel(application) {
    private val settings = SettingsRepository(application)
    private val repository = SessionManagerRepository(settings)
    private val cache = mutableMapOf<TimeRange, TimeReport>()
    private val _state = MutableStateFlow(TimeState())
    val state: StateFlow<TimeState> = _state
    private var request: Job? = null
    private var initialized = false

    fun refresh(pull: Boolean = false) {
        request?.cancel()
        request = viewModelScope.launch {
            if (!initialized) {
                val range = TimeRange.fromKey(settings.loadTimeRange())
                _state.update { it.copy(range = range, report = cache[range]) }
                initialized = true
            }
            val range = _state.value.range
            _state.update { it.copy(loading = it.report == null, refreshing = pull, error = null) }
            try {
                val url = settings.serverUrl.first()
                val token = settings.accessToken.first()
                if (url.isBlank() || token.isBlank()) {
                    _state.update { it.copy(report = null, loading = false, refreshing = false, error = "Sign in to load Time") }
                    return@launch
                }
                val report = repository.fetchAnalyticsTime(url, token, range.key)
                cache[range] = report
                _state.update { it.received(report) }
            } catch (cancel: CancellationException) {
                throw cancel
            } catch (error: Exception) {
                val auth = error is SessionManagerAuthException
                if (auth) {
                    cache.clear()
                    settings.clearAuth()
                }
                _state.update { it.copy(report = if (auth) null else it.report, loading = false, refreshing = false, error = if (auth) "Sign in to load Time" else "Couldn't load Time. Try again.") }
            }
        }
    }

    fun select(range: TimeRange) {
        if (range == _state.value.range) return
        request?.cancel()
        initialized = true
        _state.value = TimeState(range = range, report = cache[range])
        viewModelScope.launch { settings.saveTimeRange(range.key) }
        refresh()
    }

    fun open(node: TimeNode) { _state.update { it.open(node) } }
    fun back() { _state.update { it.back() } }
    fun toLevel(depth: Int) { _state.update { it.copy(path = it.path.take(depth)) } }
}
