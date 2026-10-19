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
import li.rajeshgo.sm.data.model.QueueStats
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.data.model.UtilizationSeries
import li.rajeshgo.sm.data.repository.ScreenCache
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.repository.StartNowResult
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

/**
 * Start now (sm#2383): the jobs being started, the memory risk waiting for
 * the owner's yes, and the last outcome to show once.
 */
data class StartNowState(
    val starting: Set<String> = emptySet(),
    val risk: StartRisk? = null,
    val message: StartMessage? = null,
    /** Job id to why its last start failed, for the job sheet, which hides snackbars. */
    val failed: Pair<String, String>? = null,
)

data class StartRisk(val job: SessionJob, val text: String)

/** [id] makes each outcome a new snackbar, even with the same text. */
data class StartMessage(val text: String, val error: Boolean, val id: Long = System.nanoTime())

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
    val startNow: StartNowState = StartNowState(),
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

    /**
     * One tap starts [job] past every queue rule (sm#2383). Only a memory
     * risk stops to ask, through [StartNowState.risk]; [acceptRisk] is the
     * owner's yes. A started job moves to Running at once.
     */
    fun startNow(job: SessionJob, acceptRisk: Boolean = false) {
        if (job.id in _uiState.value.startNow.starting) return
        _uiState.update {
            it.copy(startNow = it.startNow.copy(starting = it.startNow.starting + job.id, risk = null, failed = null))
        }
        viewModelScope.launch {
            val outcome = credentials()?.let { (url, token) -> repository.startQueueJobNow(url, token, job.id, acceptRisk) }
            val done = { change: (StartNowState) -> StartNowState ->
                _uiState.update { it.copy(startNow = change(it.startNow.copy(starting = it.startNow.starting - job.id))) }
            }
            outcome ?: return@launch done { it }
            outcome.onSuccess { result ->
                when (result) {
                    is StartNowResult.Started -> {
                        _uiState.update { state ->
                            state.copy(
                                overview = state.overview?.let { overview ->
                                    overview.copy(
                                        queued = overview.queued.filterNot { it.id == job.id },
                                        running = overview.running.filterNot { it.id == job.id } + result.job,
                                    )
                                },
                            )
                        }
                        done { it.copy(message = StartMessage("Started ${jobTitle(job)}", error = false)) }
                        refresh()
                    }
                    is StartNowResult.Risky -> done { it.copy(risk = StartRisk(job, result.risk)) }
                }
            }.onFailure { error ->
                if (handleAuth(error)) return@onFailure done { it }
                val reason = error.message ?: "Couldn't start ${jobTitle(job)}"
                done { it.copy(message = StartMessage(reason, error = true), failed = job.id to reason) }
            }
        }
    }

    fun dismissStartRisk() {
        _uiState.update { it.copy(startNow = it.startNow.copy(risk = null)) }
    }

    fun startMessageShown(message: StartMessage) {
        _uiState.update { state ->
            if (state.startNow.message?.id != message.id) state
            else state.copy(startNow = state.startNow.copy(message = null))
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
        _uiState.value = _uiState.value.copy(log = null, cancelError = null, followMessage = null)
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
