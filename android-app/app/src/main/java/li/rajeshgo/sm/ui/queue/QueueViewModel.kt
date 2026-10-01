package li.rajeshgo.sm.ui.queue

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.time.OffsetDateTime
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.QueueOverview
import li.rajeshgo.sm.data.model.QueueStartCheck
import li.rajeshgo.sm.data.model.QueueStats
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.data.model.UtilizationSeries
import li.rajeshgo.sm.data.repository.ScreenCache
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.repository.WhatRequestBusyException

/** What the job sheet shows for an Ask agent request. */
@Serializable
data class AskState(
    val jobId: String,
    val question: String,
    val status: String,
    val answer: String? = null,
    val error: String? = null,
    /** When the question was sent, epoch milliseconds. */
    val askedAtMs: Long = 0,
)

/** Start now for one job: the check, then the start (sm#1627). */
data class StartNowState(
    val jobId: String,
    val check: QueueStartCheck? = null,
    val starting: Boolean = false,
    val error: String? = null,
)

data class QueueUiState(
    val overview: QueueOverview? = null,
    val stats: QueueStats? = null,
    val statsHours: Int = 168,
    val loading: Boolean = true,
    /** A pull-to-refresh in Analytics › Queue is in flight. */
    val refreshing: Boolean = false,
    /** Showing the last overview from [ScreenCache] while the first read runs (spec 1782 J5). */
    val revalidating: Boolean = false,
    val lastUpdated: OffsetDateTime? = null,
    val refreshError: String? = null,
    val signedOut: Boolean = false,
    val log: Pair<String, String>? = null,
    val cancelError: String? = null,
    val startNow: StartNowState? = null,
    /** Result of Follow for the open sheet: job id to message. */
    val followMessage: Pair<String, String>? = null,
    /** The latest Ask agent request per job id. */
    val asks: Map<String, AskState> = emptyMap(),
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

    private val _uiState = MutableStateFlow(
        ScreenCache.queue.let { overview ->
            QueueUiState(usageHours = rememberedUsageHours, overview = overview, loading = overview == null, revalidating = overview != null)
        },
    )
    val uiState: StateFlow<QueueUiState> = _uiState

    init {
        QueueAsks.attach(settingsRepository)
        viewModelScope.launch {
            QueueAsks.state.collect { asks -> _uiState.update { it.copy(asks = asks) } }
        }
    }

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            _uiState.value = _uiState.value.copy(loading = false, revalidating = false, signedOut = true)
            return null
        }
        return serverUrl to token
    }

    private suspend fun handleAuth(error: Throwable): Boolean {
        if (error !is SessionManagerAuthException) return false
        settingsRepository.clearAuth()
        _uiState.value = _uiState.value.copy(loading = false, revalidating = false, signedOut = true)
        return true
    }

    /** The overview refresh (every 5 s on the Queue tab); keeps the last data on failure. */
    fun refresh(pull: Boolean = false) {
        if (refreshJob?.isActive == true) return
        if (pull) _uiState.update { it.copy(refreshing = true) }
        refreshJob = viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch _uiState.update { it.copy(refreshing = false) }
            runCatching { repository.fetchQueue(url, token) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(
                        overview = it,
                        loading = false,
                        refreshing = false,
                        revalidating = false,
                        lastUpdated = OffsetDateTime.now(),
                        refreshError = null,
                    )
                }
                .onFailure { error ->
                    if (!handleAuth(error)) {
                        _uiState.value = _uiState.value.copy(loading = false, revalidating = false, refreshError = "Couldn't refresh — retrying")
                    }
                    _uiState.update { it.copy(refreshing = false) }
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

    /** Loads what Start now would override, for the confirmation. */
    fun checkStartNow(job: SessionJob) {
        _uiState.value = _uiState.value.copy(startNow = StartNowState(job.id))
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            val next = runCatching { repository.fetchQueueStartCheck(url, token, job.id) }.fold(
                onSuccess = { StartNowState(job.id, check = it) },
                onFailure = { error ->
                    if (handleAuth(error)) return@launch
                    StartNowState(job.id, error = error.message ?: "Couldn't check this job")
                },
            )
            if (_uiState.value.startNow?.jobId == job.id) _uiState.value = _uiState.value.copy(startNow = next)
        }
    }

    /** Starts the job past every queue rule, then refreshes. */
    fun startNow(job: SessionJob) {
        val current = _uiState.value.startNow?.takeIf { it.jobId == job.id } ?: StartNowState(job.id)
        _uiState.value = _uiState.value.copy(startNow = current.copy(starting = true, error = null))
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            repository.forceStartQueueJob(url, token, job.id)
                .onSuccess {
                    _uiState.value = _uiState.value.copy(startNow = null)
                    refresh()
                }
                .onFailure { error ->
                    if (!handleAuth(error)) {
                        _uiState.value = _uiState.value.copy(
                            startNow = current.copy(starting = false, error = error.message ?: "Couldn't start the job"),
                        )
                    }
                }
        }
    }

    fun dismissStartNow() {
        _uiState.value = _uiState.value.copy(startNow = null)
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
        val owner = _uiState.value.overview?.ownerName ?: "The owner"
        QueueAsks.start(job.id, question) { update ->
            val (url, token) = credentials() ?: return@start update(AskState(job.id, question, "failed", error = "Sign in to ask the agent"))
            val prompt = askAgentPrompt(owner, job, question, OffsetDateTime.now())
            repository.runWhatRequest(url, token, sessionId, prompt, attachOnConflict = false) { record ->
                update(AskState(job.id, question, record.status, answer = record.result, error = record.error))
            }.onFailure { error ->
                val message = when (error) {
                    is WhatRequestBusyException -> "${jobAgentLabel(job)} is answering another question — try again in a minute"
                    is SessionManagerAuthException -> "Signed out — sign in again to ask the agent"
                    else -> error.message ?: "Couldn't ask the agent"
                }
                update(AskState(job.id, question, "failed", error = message))
            }
        }
    }

    /** Resets what belongs to one opening of the sheet; Ask agent answers are kept. */
    fun clearSheetState() {
        _uiState.value = _uiState.value.copy(log = null, cancelError = null, followMessage = null, startNow = null)
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

/**
 * Ask agent requests by job id, kept for the life of the process. Polling runs
 * outside any screen so an answer that lands after the sheet or the Queue tab
 * closes, or after the job itself ends, is there when the job is opened again.
 * Finished answers are saved, newest [MAX_SAVED], for [KEEP_MS].
 */
internal object QueueAsks {
    private const val MAX_SAVED = 20
    private const val KEEP_MS = 24 * 60 * 60 * 1000L
    private val FINISHED = setOf("completed", "failed", "timed_out")
    private val codec = ListSerializer(AskState.serializer())
    private val json = Json { ignoreUnknownKeys = true }
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val saveLock = Mutex()
    private val jobs = mutableMapOf<String, Job>()
    private var store: SettingsRepository? = null
    val state = MutableStateFlow<Map<String, AskState>>(emptyMap())

    /** Loads answers saved by an earlier run of the app, once per process. */
    @Synchronized
    fun attach(settings: SettingsRepository) {
        if (store != null) return
        store = settings
        scope.launch {
            val saved = runCatching { json.decodeFromString(codec, settings.loadQueueAsksJson()) }.getOrDefault(emptyList())
            val cutoff = System.currentTimeMillis() - KEEP_MS
            // Anything asked since launch is newer than what was saved.
            state.update { current -> saved.filter { it.askedAtMs >= cutoff }.associateBy { it.jobId } + current }
        }
    }

    @Synchronized
    fun start(jobId: String, question: String, run: suspend (update: (AskState) -> Unit) -> Unit) {
        jobs.remove(jobId)?.cancel()
        val askedAtMs = System.currentTimeMillis()
        set(AskState(jobId, question, "sending", askedAtMs = askedAtMs))
        jobs[jobId] = scope.launch { run { set(it.copy(askedAtMs = askedAtMs)) } }
    }

    private fun set(ask: AskState) {
        state.update { it + (ask.jobId to ask) }
        if (ask.status in FINISHED) save()
    }

    /** One writer at a time, each saving the state as it is when it writes. */
    private fun save() {
        val settings = store ?: return
        scope.launch {
            saveLock.withLock {
                val cutoff = System.currentTimeMillis() - KEEP_MS
                val finished = state.value.values
                    .filter { it.status in FINISHED && it.askedAtMs >= cutoff }
                    .sortedBy { it.askedAtMs }
                    .takeLast(MAX_SAVED)
                runCatching { settings.saveQueueAsksJson(json.encodeToString(codec, finished)) }
            }
        }
    }
}
