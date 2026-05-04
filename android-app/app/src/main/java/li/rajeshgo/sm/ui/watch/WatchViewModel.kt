package li.rajeshgo.sm.ui.watch

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
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
    val status: String = "connecting",
    val outputData: String = "",
    val outputEncoding: String = "text",
    val outputSequence: Long = 0L,
    val copyBuffer: String = "",
    val inputDraft: String = "",
    val error: String? = null,
)

data class WatchUiState(
    val serverUrl: String = "",
    val userEmail: String = "",
    val bootstrap: ClientBootstrapResponse? = null,
    val sessions: List<ClientSession> = emptyList(),
    val expandedSessionIds: Set<String> = emptySet(),
    val detailsBySessionId: Map<String, SessionDetail> = emptyMap(),
    val loading: Boolean = true,
    val refreshing: Boolean = false,
    val requestingStatus: Boolean = false,
    val ensuringMaintainer: Boolean = false,
    val terminal: TerminalUiState? = null,
    val lastSync: String? = null,
    val error: String? = null,
)

class WatchViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val sessionRepository = SessionManagerRepository()
    private val deviceKeyManager = DeviceKeyManager()
    private var refreshJob: Job? = null
    private var terminalSocket: WebSocket? = null
    private var terminalAttachToken: String? = null

    private val _uiState = MutableStateFlow(WatchUiState())
    val uiState: StateFlow<WatchUiState> = _uiState

    init {
        viewModelScope.launch {
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
        terminalAttachToken = null
        terminalSocket?.close(1000, "viewmodel cleared")
        terminalSocket = null
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
                        _uiState.value = _uiState.value.copy(
                            sessions = sessions,
                            detailsBySessionId = preservedDetails,
                            loading = false,
                            refreshing = false,
                            userEmail = userEmail,
                            lastSync = java.time.OffsetDateTime.now().toString(),
                            error = null,
                        )
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
                                    sessions = emptyList(),
                                    expandedSessionIds = emptySet(),
                                    detailsBySessionId = emptyMap(),
                                    lastSync = null,
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

    fun killSession(sessionId: String, onComplete: (Result<Unit>) -> Unit) {
        viewModelScope.launch {
            val serverUrl = settingsRepository.serverUrl.first()
            val accessToken = settingsRepository.accessToken.first()
            val result = if (serverUrl.isBlank() || accessToken.isBlank()) {
                Result.failure(IllegalStateException("Sign in to kill sessions"))
            } else {
                sessionRepository.killSession(serverUrl, accessToken, sessionId)
            }
            if (result.exceptionOrNull() is SessionManagerAuthException) {
                settingsRepository.clearAuth()
                _uiState.value = _uiState.value.copy(
                    sessions = emptyList(),
                    expandedSessionIds = emptySet(),
                    detailsBySessionId = emptyMap(),
                    lastSync = null,
                    userEmail = "",
                )
            }
            if (result.isSuccess) {
                _uiState.value = _uiState.value.copy(
                    sessions = _uiState.value.sessions.filterNot { it.id == sessionId },
                    expandedSessionIds = _uiState.value.expandedSessionIds - sessionId,
                    detailsBySessionId = _uiState.value.detailsBySessionId - sessionId,
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
            val attachToken = UUID.randomUUID().toString()
            terminalAttachToken = attachToken
            val path = sessionRepository.mobileAttachTicketPath(
                baseUrl = serverUrl,
                sessionId = session.id,
                advertisedEndpoint = session.mobileTerminal?.ticketEndpoint,
            )
            _uiState.value = _uiState.value.copy(
                terminal = TerminalUiState(
                    sessionId = session.id,
                    title = sessionDisplayName(session),
                    status = "requesting ticket",
                )
            )
            val proof = runCatching {
                deviceKeyManager.signTicketRequest(
                    method = "POST",
                    path = path,
                    sessionId = session.id,
                    actorEmail = actorEmail,
                )
            }.getOrElse { error ->
                clearTerminalIfCurrent(attachToken)
                onComplete(Result.failure(error))
                return@launch
            }
            val ticketResult = sessionRepository.createMobileAttachTicket(serverUrl, accessToken, session.id, proof)
            ticketResult.onFailure { error ->
                updateTerminalIfCurrent(attachToken) { it.copy(status = "failed", error = error.message) }
                onComplete(Result.failure(error))
                return@launch
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
                updateTerminalIfCurrent(attachToken) { it.copy(status = "failed", error = error.message) }
                onComplete(Result.failure(error))
                return@launch
            }
            terminalSocket?.close(1000, "new attach")
            terminalSocket = sessionRepository.openMobileTerminalSocket(ticket, accessToken, object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    val frame = JSONObject()
                        .put("type", "auth")
                        .put("ticket_id", ticket.ticketId)
                        .put("ticket_secret", ticket.ticketSecret)
                        .put("device_key_id", ticket.deviceKeyId)
                        .put("nonce", wsNonce)
                        .put("signature", wsSignature)
                    webSocket.send(frame.toString())
                    viewModelScope.launch {
                        updateTerminalIfCurrent(attachToken) { it.copy(status = "authenticating", error = null) }
                    }
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    val payload = runCatching { JSONObject(text) }.getOrNull() ?: return
                    viewModelScope.launch {
                        when (payload.optString("type")) {
                            "output" -> updateTerminalIfCurrent(attachToken) { current ->
                                val data = payload.optString("data")
                                val encoding = payload.optString("encoding", "text")
                                val mode = payload.optString("mode")
                                current.copy(
                                    status = "attached",
                                    outputData = data,
                                    outputEncoding = encoding,
                                    outputSequence = current.outputSequence + 1,
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
                                it.copy(status = payload.optString("state", it.status))
                            }
                            "error" -> updateTerminalIfCurrent(attachToken) {
                                it.copy(error = payload.optString("message", "Terminal error"))
                            }
                            "exit" -> updateTerminalIfCurrent(attachToken) { it.copy(status = "detached") }
                        }
                    }
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    viewModelScope.launch {
                        updateTerminalIfCurrent(attachToken) {
                            it.copy(status = "failed", error = t.message ?: "Terminal socket failed")
                        }
                    }
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    viewModelScope.launch {
                        updateTerminalIfCurrent(attachToken) { it.copy(status = "detached") }
                    }
                }
            })
            onComplete(Result.success("Opening terminal for ${sessionDisplayName(session)}"))
        }
    }

    fun updateTerminalInput(value: String) {
        _uiState.value = _uiState.value.copy(
            terminal = _uiState.value.terminal?.copy(inputDraft = value)
        )
    }

    fun sendTerminalInput(sendEnter: Boolean = false) {
        val terminal = _uiState.value.terminal ?: return
        val text = terminal.inputDraft
        if (text.isNotEmpty()) {
            sendTerminalData(text)
        }
        if (sendEnter) {
            sendTerminalKey("enter")
        }
        _uiState.value = _uiState.value.copy(
            terminal = _uiState.value.terminal?.copy(inputDraft = "")
        )
    }

    fun sendTerminalData(data: String) {
        if (data.isEmpty()) {
            return
        }
        terminalSocket?.send(JSONObject().put("type", "input").put("data", data).toString())
    }

    fun sendTerminalKey(key: String) {
        terminalSocket?.send(JSONObject().put("type", "key").put("key", key).toString())
    }

    fun resizeTerminal(cols: Int, rows: Int) {
        if (cols !in 20..300 || rows !in 10..120) {
            return
        }
        terminalSocket?.send(
            JSONObject()
                .put("type", "resize")
                .put("cols", cols)
                .put("rows", rows)
                .toString()
        )
    }

    fun detachTerminal() {
        terminalAttachToken = null
        terminalSocket?.send(JSONObject().put("type", "detach").toString())
        terminalSocket?.close(1000, "detach")
        terminalSocket = null
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

    private fun clearTerminalIfCurrent(attachToken: String) {
        if (terminalAttachToken != attachToken) {
            return
        }
        terminalAttachToken = null
        _uiState.value = _uiState.value.copy(terminal = null)
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
}
