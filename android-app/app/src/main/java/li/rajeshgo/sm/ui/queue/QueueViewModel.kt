package li.rajeshgo.sm.ui.queue

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.time.OffsetDateTime
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.QueueOverview
import li.rajeshgo.sm.data.model.QueueStats
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.data.model.UtilizationSeries
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.repository.WhatRequestBusyException

/** What the job sheet shows for an Ask agent request. */
data class AskState(
    val jobId: String,
    val status: String,
    val answer: String? = null,
    val error: String? = null,
)

data class QueueUiState(
    val overview: QueueOverview? = null,
    val stats: QueueStats? = null,
    val statsHours: Int = 168,
    val loading: Boolean = true,
    val lastUpdated: OffsetDateTime? = null,
    val refreshError: String? = null,
    val signedOut: Boolean = false,
    val log: Pair<String, String>? = null,
    val cancelError: String? = null,
    /** Result of Follow for the open sheet: job id to message. */
    val followMessage: Pair<String, String>? = null,
    val ask: AskState? = null,
    val usage: UtilizationSeries? = null,
    val usageHours: Int = 24,
    val usageLoading: Boolean = false,
    val usageError: String? = null,
)

class QueueViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)
    private var refreshJob: Job? = null
    private var statsJob: Job? = null
    private var usageJob: Job? = null
    private var askJob: Job? = null

    private val _uiState = MutableStateFlow(QueueUiState(usageHours = rememberedUsageHours))
    val uiState: StateFlow<QueueUiState> = _uiState

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            _uiState.value = _uiState.value.copy(loading = false, signedOut = true)
            return null
        }
        return serverUrl to token
    }

    private suspend fun handleAuth(error: Throwable): Boolean {
        if (error !is SessionManagerAuthException) return false
        settingsRepository.clearAuth()
        _uiState.value = _uiState.value.copy(loading = false, signedOut = true)
        return true
    }

    /** The 5-second overview refresh; keeps the last data on failure. */
    fun refresh() {
        if (refreshJob?.isActive == true) return
        refreshJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            runCatching { repository.fetchQueue(url, token) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(
                        overview = it,
                        loading = false,
                        lastUpdated = OffsetDateTime.now(),
                        refreshError = null,
                    )
                }
                .onFailure { error ->
                    if (!handleAuth(error)) {
                        _uiState.value = _uiState.value.copy(loading = false, refreshError = "Couldn't refresh — retrying")
                    }
                }
        }
    }

    fun refreshStats(hours: Int = _uiState.value.statsHours) {
        statsJob?.cancel()
        _uiState.value = _uiState.value.copy(statsHours = hours)
        statsJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            runCatching { repository.fetchQueueStats(url, token, hours) }
                .onSuccess { _uiState.value = _uiState.value.copy(stats = it) }
                .onFailure { handleAuth(it) }
        }
    }

    fun loadLog(job: SessionJob) {
        _uiState.value = _uiState.value.copy(log = job.id to "Loading…")
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            val text = runCatching { repository.fetchQueueJobLog(url, token, job.id, 40).text }
                .getOrElse { "No log yet" }
                .ifBlank { "No log yet" }
            if (_uiState.value.log?.first == job.id) {
                _uiState.value = _uiState.value.copy(log = job.id to text)
            }
        }
    }

    /** Cancels, then refreshes at once; [onDone] runs only on success. */
    fun cancel(job: SessionJob, note: String?, onDone: () -> Unit) {
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            repository.cancelQueueJob(url, token, job.id, note)
                .onSuccess {
                    _uiState.value = _uiState.value.copy(cancelError = null)
                    onDone()
                    refresh()
                }
                .onFailure { error ->
                    if (!handleAuth(error)) {
                        _uiState.value = _uiState.value.copy(cancelError = error.message ?: "Cancel failed")
                    }
                }
        }
    }

    fun follow(job: SessionJob) {
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            val message = repository.followJob(url, token, job.id).fold(
                onSuccess = { "Following — you'll be notified when it ends" },
                onFailure = { error ->
                    if (handleAuth(error)) return@launch
                    error.message ?: "Couldn't follow this job"
                },
            )
            _uiState.value = _uiState.value.copy(followMessage = job.id to message)
        }
    }

    fun ask(job: SessionJob, question: String) {
        val sessionId = job.notifySessionId?.takeIf { it.isNotBlank() } ?: return
        askJob?.cancel()
        _uiState.value = _uiState.value.copy(ask = AskState(job.id, "sending"))
        askJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            val owner = _uiState.value.overview?.ownerName ?: "The owner"
            val prompt = askAgentPrompt(owner, job, question, OffsetDateTime.now())
            repository.runWhatRequest(url, token, sessionId, prompt, attachOnConflict = false) { record ->
                _uiState.value = _uiState.value.copy(
                    ask = AskState(job.id, record.status, answer = record.result, error = record.error),
                )
            }.onFailure { error ->
                val message = if (error is WhatRequestBusyException) {
                    "${jobAgentLabel(job)} is answering another question — try again in a minute"
                } else {
                    error.message ?: "Couldn't ask the agent"
                }
                _uiState.value = _uiState.value.copy(ask = AskState(job.id, "failed", error = message))
            }
        }
    }

    fun clearSheetState() {
        askJob?.cancel()
        _uiState.value = _uiState.value.copy(log = null, cancelError = null, ask = null, followMessage = null)
    }

    fun refreshUsage(hours: Int = _uiState.value.usageHours) {
        rememberedUsageHours = hours
        usageJob?.cancel()
        val changed = hours != _uiState.value.usageHours
        _uiState.value = _uiState.value.copy(
            usageHours = hours,
            usageLoading = changed || _uiState.value.usage == null,
            usage = if (changed) null else _uiState.value.usage,
        )
        usageJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            runCatching { repository.fetchUtilizationSeries(url, token, hours) }
                .onSuccess { _uiState.value = _uiState.value.copy(usage = it, usageLoading = false, usageError = null) }
                .onFailure { error ->
                    if (!handleAuth(error)) {
                        _uiState.value = _uiState.value.copy(usageLoading = false, usageError = "Couldn't load usage")
                    }
                }
        }
    }

    private companion object {
        /** The Mac usage range, remembered for the life of the process. */
        var rememberedUsageHours = 24
    }
}
