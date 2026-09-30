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
import li.rajeshgo.sm.data.model.SpendNode
import li.rajeshgo.sm.data.model.SpendReport
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.watch.loadDocReaderAuth

class SpendViewModel(application: Application) : AndroidViewModel(application) {
    private val settings = SettingsRepository(application)
    private val repository = SessionManagerRepository(settings)
    private val cache = mutableMapOf<Pair<String?, SpendRange>, SpendReport>()
    private val _state = MutableStateFlow(SpendState())
    val state: StateFlow<SpendState> = _state
    private var request: Job? = null
    private var initialized = false

    fun refresh(pull: Boolean = false) {
        request?.cancel()
        request = viewModelScope.launch {
            if (!initialized) {
                val (provider, range) = settings.loadSpendChoices()
                _state.update { it.copy(provider = provider?.takeIf { p -> p in listOf("claude", "codex") }, range = SpendRange.fromKey(range)) }
                initialized = true
            }
            val selection = _state.value
            _state.update { it.copy(loading = it.report == null, refreshing = pull, error = null) }
            try {
                val url = settings.serverUrl.first()
                val token = settings.accessToken.first()
                if (url.isBlank() || token.isBlank()) {
                    _state.update { it.copy(report = null, loading = false, refreshing = false, error = "Sign in to load Spend") }
                    return@launch
                }
                val report = repository.fetchAnalyticsSpend(url, token, selection.provider, selection.range.key)
                cache[report.provider to selection.range] = report
                _state.update { it.received(report) }
                settings.saveSpendChoices(report.provider, selection.range.key)
            } catch (cancel: CancellationException) {
                throw cancel
            } catch (error: Exception) {
                if (error is SessionManagerAuthException) {
                    cache.clear()
                    settings.clearAuth()
                }
                _state.update { it.copy(report = if (error is SessionManagerAuthException) null else it.report, loading = false, refreshing = false, error = if (error is SessionManagerAuthException) "Sign in to load Spend" else "Couldn't load Spend. Try again.") }
            }
        }
    }

    fun select(provider: String? = _state.value.provider, range: SpendRange = _state.value.range) {
        if (provider == _state.value.provider && range == _state.value.range) return
        request?.cancel()
        initialized = true
        _state.value = SpendState(provider = provider, range = range, report = cache[provider to range])
        viewModelScope.launch { settings.saveSpendChoices(provider, range.key) }
        refresh()
    }

    fun open(node: SpendNode) { _state.update { it.open(node) } }
    fun back() { _state.update { it.back() } }
    fun toLevel(depth: Int) { _state.update { it.copy(path = it.path.take(depth)) } }
    suspend fun docReaderAuth() = loadDocReaderAuth(settings)
}
