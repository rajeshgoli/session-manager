package li.rajeshgo.sm.ui.watch

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import java.util.UUID
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerBackendUnavailableException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SessionManagerTransientException
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.security.DeviceKeyManager

data class TerminalUiState(
    val sessionId: String,
    val title: String,
    val provider: String? = null,
    val status: String = "connecting",
    val rendererCols: Int = 0,
    val rendererRows: Int = 0,
    val rendererStatus: String = "renderer loading",
    val rendererLastAckSequence: Long = 0L,
    val rendererError: String? = null,
    val outputFrameCount: Int = 0,
    val outputByteCount: Long = 0L,
    val outputFrames: List<TerminalOutputFrame> = emptyList(),
    val outputSequence: Long = 0L,
    val copyBuffer: String = "",
    val inputDraft: String = "",
    val connectionGeneration: Int = 0,
    val error: String? = null,
)

data class TerminalOutputFrame(
    val sequence: Long,
    val data: String,
    val encoding: String = "text",
)

data class WatchUiState(
    val serverUrl: String = "",
    val userEmail: String = "",
    val bootstrap: ClientBootstrapResponse? = null,
    val sessions: List<ClientSession> = emptyList(),
    val expandedSessionIds: Set<String> = emptySet(),
    val detailsBySessionId: Map<String, SessionDetail> = emptyMap(),
    val whatBySessionId: Map<String, WhatUiState> = emptyMap(),
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    val requestingStatus: Boolean = false,
    val ensuringMaintainer: Boolean = false,
    val studioSshEnabled: Boolean = false,
    val studioSshStatus: String = "off",
    val studioSshHost: String = "",
    val studioSshBusy: Boolean = false,
    val studioSshError: String? = null,
    val terminal: TerminalUiState? = null,
    val lastSync: String? = null,
    val error: String? = null,
)

class WatchViewModel(application: Application, private val savedState: androidx.lifecycle.SavedStateHandle) : AndroidViewModel(application) {

    private val settingsRepository = SettingsRepository(application)
    private val sessionRepository = SessionManagerRepository(settingsRepository)
    private val deviceKeyManager = DeviceKeyManager()
    private var refreshJob: Job? = null
    private val whatRequestJobs = mutableMapOf<String, Job>()
    // Bumped on every Studio SSH toggle so status reads that began before the
    // latest toggle can be discarded instead of applying stale pre-toggle data.
    private var studioSshStatusGeneration = 0
    private var terminalSocket: WebSocket? = null
    private var terminalWriter: TerminalFrameWriter? = null
    private var terminalReconnect: (() -> Unit)? = null
    private var terminalReconnectJob: Job? = null
    private var terminalForeground = true
    private var terminalConnectionGeneration = 0
    private var terminalAttachToken: String? = null
    private var pendingTerminalResize: Pair<Int, Int>? = null

    private val _uiState = MutableStateFlow(WatchUiState())
    val uiState: StateFlow<WatchUiState> = _uiState

    init {
        viewModelScope.launch {
            val persistedWhat = settingsRepository.loadWhatSummaries()
                .associate { summary -> summary.targetSessionId to summary.toUiState() }
            _uiState.value = _uiState.value.copy(whatBySessionId = persistedWhat)
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            val userEmail = settingsRepository.userEmail.first()
            if (serverUrl.isBlank() || accessToken.isBlank()) {
                _uiState.value = _uiState.value.copy(loading = false, userEmail = userEmail)
                return@launch
            }
            val bootstrap = runCatching { sessionRepository.fetchBootstrap(serverUrl) }.getOrNull()
            _uiState.value = _uiState.value.copy(serverUrl = serverUrl, userEmail = userEmail, bootstrap = bootstrap)
            refresh(initial = true)
        }
    }

    override fun onCleared() {
        terminalReconnect = null
        terminalReconnectJob?.cancel()
        whatRequestJobs.values.forEach { it.cancel() }
        whatRequestJobs.clear()
        terminalAttachToken = null
        pendingTerminalResize = null
        terminalSocket?.close(1000, "viewmodel cleared")
        terminalSocket = null
        terminalWriter = null
        super.onCleared()
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
                        error = "Sign in to load sessions",
                    )
                    return@launch
                }
                _uiState.value = _uiState.value.copy(loading = initial, refreshing = !initial, error = null)
                val expandedSessionIds = _uiState.value.expandedSessionIds
                runCatching { sessionRepository.fetchSessions(serverUrl, accessToken) }
                    .onSuccess { sessions ->
                        val sessionIds = sessions.map { it.id }.toSet()
                        val preservedDetails = _uiState.value.detailsBySessionId.filterKeys { it in sessionIds }
                        val preservedWhat = _uiState.value.whatBySessionId.filterKeys { it in sessionIds }
                        _uiState.value = _uiState.value.copy(
                            sessions = sessions,
                            detailsBySessionId = preservedDetails,
                            whatBySessionId = preservedWhat,
                            loading = false,
                            refreshing = false,
                            userEmail = userEmail,
                            lastSync = java.time.OffsetDateTime.now().toString(),
                            error = null,
                        )
                        val restoreId = savedState.get<String>("terminalSessionId")
                        if (_uiState.value.terminal == null && restoreId != null) {
                            sessions.firstOrNull { it.id == restoreId }?.let { openMobileTerminal(it) {} }
                        }
                        sessions
                            .filter { it.id in expandedSessionIds && it.id !in preservedDetails }
                            .forEach { loadDetail(it) }
                    }
                    .onFailure { error ->
                        when (error) {
                            is SessionManagerAuthException -> {
                                settingsRepository.clearAuth()
                                _uiState.value = _uiState.value.copy(
                                    loading = false,
                                    refreshing = false,
                                    sessions = emptyList(),
                                    expandedSessionIds = emptySet(),
                                    detailsBySessionId = emptyMap(),
                                    lastSync = null,
                                    userEmail = "",
                                    error = error.message ?: "Session expired. Sign in again.",
                                )
                            }
                            is SessionManagerBackendUnavailableException -> {
                                _uiState.value = _uiState.value.copy(
                                    loading = false,
                                    refreshing = false,
                                    userEmail = userEmail,
                                    error = error.message
                                        ?: "Session Manager backend is unreachable from the ingress host.",
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
                                    error = error.message ?: "Failed to refresh sessions",
                                )
                            }
                        }
                    }
            } finally {
                refreshJob = null
            }
        }
    }

    fun toggleExpanded(session: ClientSession) {
        val expanded = _uiState.value.expandedSessionIds.toMutableSet()
        if (!expanded.add(session.id)) {
            expanded.remove(session.id)
        } else if (!_uiState.value.detailsBySessionId.containsKey(session.id)) {
            loadDetail(session)
        }
        _uiState.value = _uiState.value.copy(expandedSessionIds = expanded)
    }

    fun loadDetail(session: ClientSession) {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            if (serverUrl.isBlank() || accessToken.isBlank()) {
                return@launch
            }
            runCatching { sessionRepository.fetchSessionDetail(serverUrl, accessToken, session) }
                .onSuccess { detail ->
                    _uiState.value = _uiState.value.copy(
                        detailsBySessionId = _uiState.value.detailsBySessionId + (session.id to detail)
                    )
                }
                .onFailure { error ->
                    _uiState.value = _uiState.value.copy(
                        detailsBySessionId = _uiState.value.detailsBySessionId + (session.id to SessionDetail(lastError = error.message))
                    )
                }
        }
    }

    fun requestWhat(session: ClientSession) {
        if (whatRequestJobs[session.id]?.isActive == true) {
            return
        }
        val mode = if (_uiState.value.whatBySessionId[session.id]?.entries.isNullOrEmpty()) {
            WhatRequestMode.Full
        } else {
            WhatRequestMode.Update
        }
        startWhatRequest(session, mode)
    }

    fun updateWhat(session: ClientSession) {
        if (whatRequestJobs[session.id]?.isActive != true) {
            startWhatRequest(session, WhatRequestMode.Update)
        }
    }

    fun regenerateWhat(session: ClientSession) {
        whatRequestJobs.remove(session.id)?.cancel()
        startWhatRequest(session, WhatRequestMode.Full)
    }

    private fun startWhatRequest(
        session: ClientSession,
        mode: WhatRequestMode,
    ) {
        val current = _uiState.value.whatBySessionId[session.id]
            ?: WhatUiState(session.id, sessionDisplayName(session))
        val initial = current.copy(
            targetName = sessionDisplayName(session),
            requestId = null,
            status = "pending",
            activeMode = mode,
            createdAt = null,
            finishedAt = null,
            error = null,
        )
        _uiState.value = _uiState.value.copy(
            whatBySessionId = _uiState.value.whatBySessionId + (session.id to initial),
        )

        val job = viewModelScope.launch {
            try {
                val serverUrl = settingsRepository.serverUrl.first()
                val accessToken = settingsRepository.accessToken.first()
                if (serverUrl.isBlank() || accessToken.isBlank()) {
                    updateWhatFailure(session, "Sign in to request a summary")
                    return@launch
                }

                val prompt = if (mode == WhatRequestMode.Update) {
                    buildWhatUpdatePrompt(initial.entries)
                } else {
                    null
                }
                sessionRepository
                    .runWhatRequest(serverUrl, accessToken, session.id, prompt) { record ->
                        updateWhatRecord(session, record)
                    }
                    .getOrElse { error ->
                        if (error is CancellationException) {
                            throw error
                        }
                        updateWhatFailure(session, error.message ?: "Summary request failed")
                        return@launch
                    }
                _uiState.value.whatBySessionId[session.id]
                    ?.takeIf { it.entries.isNotEmpty() }
                    ?.let { settingsRepository.saveWhatSummary(it.toPersisted()) }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Throwable) {
                updateWhatFailure(session, error.message ?: "Unable to refresh summary")
            }
        }
        whatRequestJobs[session.id] = job
        job.invokeOnCompletion {
            if (whatRequestJobs[session.id] === job) {
                whatRequestJobs.remove(session.id)
            }
        }
    }

    private fun updateWhatRecord(
        session: ClientSession,
        record: li.rajeshgo.sm.data.model.WhatRequestRecord,
    ) {
        val current = _uiState.value.whatBySessionId[session.id]
            ?: WhatUiState(session.id, sessionDisplayName(session))
        _uiState.value = _uiState.value.copy(
            whatBySessionId = _uiState.value.whatBySessionId +
                (session.id to current.withRecord(record)),
        )
    }

    private fun updateWhatFailure(session: ClientSession, message: String) {
        val current = _uiState.value.whatBySessionId[session.id]
            ?: WhatUiState(session.id, sessionDisplayName(session))
        _uiState.value = _uiState.value.copy(
            whatBySessionId = _uiState.value.whatBySessionId +
                (session.id to current.copy(status = "failed", activeMode = null, error = message)),
        )
    }

    fun createSession(request: li.rajeshgo.sm.data.model.CreateSessionRequest, onComplete: (Result<String>) -> Unit) {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            if (serverUrl.isBlank() || accessToken.isBlank()) {
                onComplete(Result.failure(IllegalStateException("Sign in to create a session")))
                return@launch
            }
            val result = sessionRepository.createSession(serverUrl, accessToken, request)
            result.onSuccess { refresh() }
            onComplete(result.map { "Session created" })
        }
    }

    fun retireSession(sessionId: String, onComplete: (Result<Unit>) -> Unit) {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            val result = if (serverUrl.isBlank() || accessToken.isBlank()) {
                Result.failure(IllegalStateException(SIGN_IN_TO_RETIRE_SESSIONS_MESSAGE))
            } else {
                sessionRepository.retireSession(serverUrl, accessToken, sessionId)
            }
            if (result.exceptionOrNull() is SessionManagerAuthException) {
                settingsRepository.clearAuth()
                _uiState.value = _uiState.value.copy(
                    sessions = emptyList(),
                    expandedSessionIds = emptySet(),
                    detailsBySessionId = emptyMap(),
                    whatBySessionId = emptyMap(),
                    lastSync = null,
                    userEmail = "",
                )
            }
            if (result.isSuccess) {
                settingsRepository.clearWhatSummary(sessionId)
                _uiState.value = _uiState.value.copy(
                    sessions = _uiState.value.sessions.filterNot { it.id == sessionId },
                    expandedSessionIds = _uiState.value.expandedSessionIds - sessionId,
                    detailsBySessionId = _uiState.value.detailsBySessionId - sessionId,
                    whatBySessionId = _uiState.value.whatBySessionId - sessionId,
                )
            }
            onComplete(result)
        }
    }

    fun openMobileTerminal(session: ClientSession, onComplete: (Result<String>) -> Unit) {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            val actorEmail = settingsRepository.userEmail.first()
            if (serverUrl.isBlank() || accessToken.isBlank() || actorEmail.isBlank()) {
                onComplete(Result.failure(IllegalStateException("Sign in to attach")))
                return@launch
            }
            terminalReconnectJob?.cancel()
            terminalSocket?.cancel()
            terminalSocket = null
            terminalWriter = null
            val attachToken = UUID.randomUUID().toString()
            terminalAttachToken = attachToken
            pendingTerminalResize = null
            val path = sessionRepository.mobileAttachTicketPath(
                baseUrl = serverUrl,
                sessionId = session.id,
                advertisedEndpoint = session.mobileTerminal?.ticketEndpoint,
            )
            savedState["terminalSessionId"] = session.id
            _uiState.value = _uiState.value.copy(
                terminal = TerminalUiState(
                    sessionId = session.id,
                    title = sessionDisplayName(session),
                    provider = session.provider,
                    status = "requesting ticket",
                    inputDraft = savedState.get<String>("terminalDraft:${session.id}").orEmpty(),
                )
            )
            var completionSent = false

            fun completeOnce(result: Result<String>) {
                if (!completionSent) {
                    completionSent = true
                    onComplete(result)
                }
            }

            fun failInitialAttach(error: Throwable) {
                updateTerminalIfCurrent(attachToken) { it.copy(status = "failed", error = error.message) }
                completeOnce(Result.failure(error))
            }

            suspend fun connectSocket(attempt: Int) {
                if (terminalAttachToken != attachToken || !terminalForeground) {
                    return
                }
                val connectionGeneration = ++terminalConnectionGeneration
                updateTerminalIfCurrent(attachToken) {
                    it.copy(
                        status = if (attempt == 0) "requesting ticket" else "retrying attach",
                        error = if (attempt == 0) null else it.error,
                    )
                }
                val proof = runCatching {
                    deviceKeyManager.signTicketRequest(
                        method = "POST",
                        path = path,
                        sessionId = session.id,
                        actorEmail = actorEmail,
                    )
                }.getOrElse { error ->
                    failInitialAttach(error)
                    return
                }
                val freshAccessToken = settingsRepository.accessToken.first()
                if (freshAccessToken.isBlank() || settingsRepository.serverUrl.first() != serverUrl || settingsRepository.userEmail.first() != actorEmail) {
                    failInitialAttach(IllegalStateException("Sign in to reconnect"))
                    return
                }
                val ticketResult = sessionRepository.createMobileAttachTicket(serverUrl, freshAccessToken, session.id, proof)
                if (terminalAttachToken != attachToken || connectionGeneration != terminalConnectionGeneration || !terminalForeground) return
                ticketResult.onFailure { error ->
                    if (error is SessionManagerTransientException || error is SessionManagerBackendUnavailableException || error is java.io.IOException) {
                        updateTerminalIfCurrent(attachToken) { it.copy(status = "reconnecting", error = "Connection interrupted. Your draft is saved.") }
                        terminalReconnectJob = viewModelScope.launch {
                            delay((1000L * (attempt + 1)).coerceAtMost(15_000L))
                            if (connectionGeneration == terminalConnectionGeneration) connectSocket(attempt + 1)
                        }
                    } else failInitialAttach(error)
                    return
                }
                val ticket = ticketResult.getOrThrow()
                val wsNonce = UUID.randomUUID().toString()
                val wsSignature = runCatching {
                    deviceKeyManager.signWebSocketAuth(
                        ticketId = ticket.ticketId,
                        sessionId = session.id,
                        actorEmail = actorEmail,
                        deviceKeyId = ticket.deviceKeyId,
                        nonce = wsNonce,
                    )
                }.getOrElse { error ->
                    failInitialAttach(error)
                    return
                }
                terminalWriter = null
                terminalSocket?.close(1000, if (attempt == 0) "new attach" else "retry attach")
                updateTerminalIfCurrent(attachToken) { it.copy(outputFrames = emptyList(), copyBuffer = "", connectionGeneration = connectionGeneration) }
                terminalSocket = sessionRepository.openMobileTerminalSocket(ticket, freshAccessToken, object : WebSocketListener() {
                    override fun onOpen(webSocket: WebSocket, response: Response) {
                        val frame = JSONObject()
                            .put("type", "auth")
                            .put("ticket_id", ticket.ticketId)
                            .put("ticket_secret", ticket.ticketSecret)
                            .put("device_key_id", ticket.deviceKeyId)
                            .put("nonce", wsNonce)
                            .put("signature", wsSignature)
                        viewModelScope.launch {
                            if (terminalSocket != webSocket || terminalAttachToken != attachToken ||
                                connectionGeneration != terminalConnectionGeneration || !terminalForeground) {
                                webSocket.cancel()
                                return@launch
                            }
                            val writer = TerminalFrameWriter()
                            if (!writer.authenticate(frame.toString(), webSocket::send)) {
                                webSocket.cancel()
                                return@launch
                            }
                            terminalWriter = writer
                            sendPendingTerminalResize()
                            updateTerminalIfCurrent(attachToken) { it.copy(status = "authenticating", error = null) }
                        }
                    }

                    override fun onMessage(webSocket: WebSocket, text: String) {
                        val payload = runCatching { JSONObject(text) }.getOrNull() ?: return
                        viewModelScope.launch {
                            if (terminalSocket != webSocket) {
                                return@launch
                            }
                            when (payload.optString("type")) {
                                "output" -> updateTerminalIfCurrent(attachToken) { current ->
                                    val data = payload.optString("data")
                                    val encoding = payload.optString("encoding", "text")
                                    val mode = payload.optString("mode")
                                    val sequence = current.outputSequence + 1
                                    val byteCount = terminalOutputByteCount(data, encoding)
                                    current.copy(
                                        status = "attached",
                                        outputFrames = (
                                            current.outputFrames + TerminalOutputFrame(
                                                sequence = sequence,
                                                data = data,
                                                encoding = encoding,
                                            )
                                        ).takeLast(500),
                                        outputSequence = sequence,
                                        outputFrameCount = current.outputFrameCount + 1,
                                        outputByteCount = current.outputByteCount + byteCount,
                                        copyBuffer = if (encoding == "base64") {
                                            current.copyBuffer
                                        } else if (mode == "snapshot") {
                                            data
                                        } else {
                                            (current.copyBuffer + data).takeLast(200_000)
                                        },
                                        error = null,
                                    )
                                }
                                "status" -> updateTerminalIfCurrent(attachToken) {
                                    val status = payload.optString("state", it.status)
                                    it.copy(status = terminalStatusAfterServerEvent(it.status, status))
                                }
                                "error" -> updateTerminalIfCurrent(attachToken) {
                                    it.copy(error = "Connection interrupted. Reconnecting with your draft saved.")
                                }
                                "exit" -> updateTerminalIfCurrent(attachToken) { it.copy(status = if (payload.optString("reason") == "max_attach_seconds") "reconnecting" else "detached") }
                            }
                        }
                    }

                    override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                        viewModelScope.launch {
                            if (terminalSocket != webSocket || terminalAttachToken != attachToken) {
                                return@launch
                            }
                            val message = mobileTerminalSocketFailureMessage(t, response)
                            val retryable = response?.code == null || response.code in setOf(404, 408, 426, 429, 500, 502, 503, 504)
                            terminalSocket = null
                            terminalWriter = null
                            if (retryable && terminalForeground) {
                                updateTerminalIfCurrent(attachToken) { it.copy(status = "reconnecting", error = "Connection interrupted. Your draft is saved.") }
                                delay((1000L * (attempt + 1)).coerceAtMost(15_000L))
                                if (connectionGeneration == terminalConnectionGeneration) connectSocket(attempt + 1)
                            } else updateTerminalIfCurrent(attachToken) { it.copy(status = "failed", error = message) }
                        }
                    }

                    override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                        webSocket.close(code, reason)
                    }

                    override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                        viewModelScope.launch {
                            if (terminalSocket != webSocket || terminalAttachToken != attachToken) return@launch
                            terminalSocket = null
                            terminalWriter = null
                            if (terminalForeground && retryTerminalClose(code, reason)) {
                                updateTerminalIfCurrent(attachToken) { it.copy(status = "reconnecting", error = null) }
                                delay((1000L * (attempt + 1)).coerceAtMost(15_000L))
                                if (connectionGeneration == terminalConnectionGeneration) connectSocket(attempt + 1)
                            } else updateTerminalIfCurrent(attachToken) { it.copy(status = "detached", error = terminalCloseMessage(reason)) }
                        }
                    }
                })
                completeOnce(Result.success("Opening terminal for ${sessionDisplayName(session)}"))
            }

            terminalReconnect = {
                terminalReconnectJob?.cancel()
                terminalReconnectJob = viewModelScope.launch { connectSocket(attempt = 0) }
            }
            terminalReconnect?.invoke()
        }
    }

    suspend fun sessionModels(provider: String, workingDir: String): List<String> = sessionRepository.fetchSessionModels(
        settingsRepository.serverUrl.first(), settingsRepository.accessToken.first(), provider, workingDir,
    )

    fun setTerminalForeground(foreground: Boolean) {
        terminalForeground = foreground
        if (!foreground) {
            terminalConnectionGeneration++
            terminalReconnectJob?.cancel()
            terminalSocket?.cancel()
            terminalSocket = null
            terminalWriter = null
            _uiState.value = _uiState.value.copy(terminal = _uiState.value.terminal?.copy(status = "paused", error = null))
        } else if (terminalSocket == null && _uiState.value.terminal != null) {
            terminalReconnect?.invoke()
        }
    }

    fun reconnectTerminal() {
        terminalConnectionGeneration++
        terminalSocket?.cancel()
        terminalSocket = null
        terminalWriter = null
        terminalReconnect?.invoke()
    }

    fun updateTerminalInput(value: String) {
        _uiState.value.terminal?.let { savedState["terminalDraft:${it.sessionId}"] = value }
        _uiState.value = _uiState.value.copy(
            terminal = _uiState.value.terminal?.copy(inputDraft = value)
        )
    }

    fun markTerminalRendererStatus(message: String) {
        _uiState.value = _uiState.value.copy(
            terminal = _uiState.value.terminal?.copy(rendererStatus = message)
        )
    }

    fun markTerminalRendererReady(cols: Int, rows: Int) {
        rememberTerminalResize(cols, rows)
        sendPendingTerminalResize()
        _uiState.value = _uiState.value.copy(
            terminal = _uiState.value.terminal?.copy(
                rendererCols = cols,
                rendererRows = rows,
                rendererStatus = "renderer ready ${cols}x${rows}",
                rendererError = null,
            )
        )
    }

    fun markTerminalRendererError(message: String) {
        _uiState.value = _uiState.value.copy(
            terminal = _uiState.value.terminal?.copy(
                rendererStatus = "renderer error",
                rendererError = message,
            )
        )
    }

    fun markTerminalRendererWritten(sequence: Long, bytes: Int) {
        _uiState.value = _uiState.value.copy(
            terminal = _uiState.value.terminal?.let { terminal ->
                terminal.copy(
                    rendererStatus = "renderer wrote frame $sequence (${bytes}B)",
                    rendererLastAckSequence = maxOf(terminal.rendererLastAckSequence, sequence),
                    rendererError = null,
                )
            }
        )
    }

    fun sendTerminalInput(sendEnter: Boolean = false) {
        val terminal = _uiState.value.terminal ?: return
        val writer = terminalWriter ?: return
        if (terminal.status != "attached" || terminal.inputDraft.isBlank()) return
        // Bracketed paste keeps multiline prompts intact; Enter submits separately.
        val data = "\u001b[200~" + terminal.inputDraft + "\u001b[201~"
        val accepted = writer.send(JSONObject().put("type", "input").put("data", data).toString())
        val submitted = !sendEnter || (accepted && writer.send(JSONObject().put("type", "key").put("key", "enter").toString()))
        if (accepted && submitted) updateTerminalInput("")
    }

    fun sendTerminalData(data: String) {
        if (data.isEmpty()) {
            return
        }
        terminalWriter?.send(JSONObject().put("type", "input").put("data", data).toString())
    }

    fun sendTerminalKey(key: String) {
        terminalWriter?.send(JSONObject().put("type", "key").put("key", key).toString())
    }

    fun sendTerminalPageScroll(up: Boolean) {
        sendTerminalData(if (up) "\u001b[5~" else "\u001b[6~")
    }

    fun resizeTerminal(cols: Int, rows: Int) {
        if (!rememberTerminalResize(cols, rows)) {
            return
        }
        sendPendingTerminalResize()
    }

    fun detachTerminal() {
        savedState.remove<String>("terminalSessionId")
        terminalReconnect = null
        terminalConnectionGeneration++
        terminalReconnectJob?.cancel()
        terminalAttachToken = null
        pendingTerminalResize = null
        terminalWriter?.send(JSONObject().put("type", "detach").toString())
        terminalSocket?.close(1000, "detach")
        terminalSocket = null
        terminalWriter = null
        _uiState.value = _uiState.value.copy(terminal = null)
        refresh()
    }

    private fun updateTerminalIfCurrent(
        attachToken: String,
        transform: (TerminalUiState) -> TerminalUiState,
    ) {
        if (terminalAttachToken != attachToken) {
            return
        }
        val current = _uiState.value.terminal ?: return
        _uiState.value = _uiState.value.copy(terminal = transform(current))
    }

    private fun rememberTerminalResize(cols: Int, rows: Int): Boolean {
        if (cols !in 10..300 || rows !in 2..120) {
            return false
        }
        pendingTerminalResize = cols to rows
        return true
    }

    private fun sendPendingTerminalResize() {
        val writer = terminalWriter ?: return
        val (cols, rows) = pendingTerminalResize ?: return
        writer.send(terminalResizeFrame(cols, rows).toString())
    }

    private fun terminalResizeFrame(cols: Int, rows: Int): JSONObject {
        return JSONObject()
            .put("type", "resize")
            .put("cols", cols)
            .put("rows", rows)
    }

    private fun terminalOutputByteCount(data: String, encoding: String): Long {
        return if (encoding == "base64") {
            val padding = data.takeLastWhile { it == '=' }.length
            ((data.length * 3) / 4 - padding).coerceAtLeast(0).toLong()
        } else {
            data.length.toLong()
        }
    }

    private fun mobileTerminalSocketFailureMessage(error: Throwable, response: Response?): String {
        return when (response?.code) {
            401 -> "Sign in again in Settings to reconnect. Your draft is saved."
            403 -> "Device access was refused. Check device enrollment in Settings. Your draft is saved."
            else -> "Couldn't connect. Retry when your connection is available. Your draft is saved."
        }
    }

    fun requestStatus(onComplete: (Result<String>) -> Unit) {
        viewModelScope.launch {
            if (_uiState.value.requestingStatus) {
                return@launch
            }
            _uiState.value = _uiState.value.copy(requestingStatus = true)
            try {
                val serverUrl = settingsRepository.serverUrl.first()
                val accessToken = settingsRepository.accessToken.first()
                if (serverUrl.isBlank() || accessToken.isBlank()) {
                    onComplete(Result.failure(IllegalStateException("Sign in to request status")))
                    return@launch
                }

                val result = sessionRepository.requestStatus(serverUrl, accessToken)
                    .map { response ->
                        buildString {
                            append("Requested status from ")
                            append(response.targetedCount)
                            append(" sessions")
                            if (response.deliveredCount > 0 || response.queuedCount > 0 || response.failedCount > 0) {
                                append(" • ")
                                append(response.deliveredCount)
                                append(" now")
                                append(" • ")
                                append(response.queuedCount)
                                append(" queued")
                                if (response.failedCount > 0) {
                                    append(" • ")
                                    append(response.failedCount)
                                    append(" failed")
                                }
                            }
                        }
                    }
                result.onSuccess {
                    refresh()
                }
                onComplete(result)
            } finally {
                _uiState.value = _uiState.value.copy(requestingStatus = false)
            }
        }
    }

    fun ensureMaintainer(onComplete: (Result<String>) -> Unit) {
        viewModelScope.launch {
            if (_uiState.value.ensuringMaintainer) {
                return@launch
            }
            _uiState.value = _uiState.value.copy(ensuringMaintainer = true)
            try {
                val serverUrl = settingsRepository.serverUrl.first()
                val accessToken = settingsRepository.accessToken.first()
                if (serverUrl.isBlank() || accessToken.isBlank()) {
                    onComplete(Result.failure(IllegalStateException("Sign in to wake maintainer")))
                    return@launch
                }

                val result = sessionRepository.ensureMaintainer(serverUrl, accessToken)
                    .map { response ->
                        val session = response.session
                        val nextSessions = _uiState.value.sessions.toMutableList()
                        val existingIndex = nextSessions.indexOfFirst { it.id == session.id }
                        if (existingIndex >= 0) {
                            nextSessions[existingIndex] = session
                        } else {
                            nextSessions.add(0, session)
                        }
                        _uiState.value = _uiState.value.copy(
                            sessions = nextSessions,
                            lastSync = java.time.OffsetDateTime.now().toString(),
                            error = null,
                        )
                        "Maintainer ${if (response.created) "started" else "ready"}: ${sessionDisplayName(session)} [${session.id}]"
                    }

                if (result.exceptionOrNull() is SessionManagerAuthException) {
                    settingsRepository.clearAuth()
                    _uiState.value = _uiState.value.copy(
                        sessions = emptyList(),
                        expandedSessionIds = emptySet(),
                        detailsBySessionId = emptyMap(),
                        whatBySessionId = emptyMap(),
                        lastSync = null,
                        userEmail = "",
                    )
                }
                result.onSuccess {
                    refresh()
                }
                onComplete(result)
            } finally {
                _uiState.value = _uiState.value.copy(ensuringMaintainer = false)
            }
        }
    }

    fun refreshStudioSshStatus() {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            if (serverUrl.isBlank() || accessToken.isBlank()) {
                return@launch
            }
            if (_uiState.value.studioSshBusy) {
                return@launch
            }
            val generation = studioSshStatusGeneration
            runCatching { sessionRepository.fetchStudioSshStatus(serverUrl, accessToken) }
                .onSuccess { status ->
                    // A user toggle may have started (and possibly finished) while this
                    // read was in flight; discard responses older than the latest toggle.
                    if (_uiState.value.studioSshBusy || studioSshStatusGeneration != generation) {
                        return@onSuccess
                    }
                    _uiState.value = _uiState.value.copy(
                        studioSshEnabled = status.enabled,
                        studioSshStatus = status.status,
                        studioSshHost = status.host.ifBlank { _uiState.value.studioSshHost },
                        studioSshError = status.error,
                    )
                }
        }
    }

    fun toggleStudioSsh(enabled: Boolean, onComplete: (Result<String>) -> Unit) {
        viewModelScope.launch {
            if (_uiState.value.studioSshBusy) {
                return@launch
            }
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            if (serverUrl.isBlank() || accessToken.isBlank()) {
                onComplete(Result.failure(IllegalStateException("Sign in to toggle Studio SSH")))
                return@launch
            }
            // Invalidate any status read already in flight so its (pre-toggle) result is dropped.
            studioSshStatusGeneration++
            // Optimistic: reflect the desired state immediately.
            _uiState.value = _uiState.value.copy(
                studioSshBusy = true,
                studioSshEnabled = enabled,
                studioSshStatus = if (enabled) "starting" else "off",
                studioSshError = null,
            )
            val result = sessionRepository.setStudioSsh(serverUrl, accessToken, enabled)
                .map { status ->
                    _uiState.value = _uiState.value.copy(
                        studioSshEnabled = status.enabled,
                        studioSshStatus = status.status,
                        studioSshHost = status.host.ifBlank { _uiState.value.studioSshHost },
                        studioSshError = status.error,
                    )
                    if (enabled) "Studio SSH starting" else "Studio SSH off"
                }
            if (result.exceptionOrNull() is SessionManagerAuthException) {
                settingsRepository.clearAuth()
                _uiState.value = _uiState.value.copy(
                    sessions = emptyList(),
                    expandedSessionIds = emptySet(),
                    detailsBySessionId = emptyMap(),
                    whatBySessionId = emptyMap(),
                    lastSync = null,
                    userEmail = "",
                )
            }
            result.onFailure { error ->
                // Revert the optimistic desired state; the next poll reconciles from the server.
                _uiState.value = _uiState.value.copy(
                    studioSshEnabled = !enabled,
                    studioSshStatus = "error",
                    studioSshError = error.message,
                )
            }
            _uiState.value = _uiState.value.copy(studioSshBusy = false)
            onComplete(result)
        }
    }
}
