package li.rajeshgo.sm.debug

import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.MobileAttachTicketResponse
import li.rajeshgo.sm.data.remote.ApiService
import li.rajeshgo.sm.data.remote.HttpClientFactory
import li.rajeshgo.sm.data.repository.DeviceEnrollmentRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.security.DeviceKeyManager
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONArray
import org.json.JSONObject
import retrofit2.Retrofit
import java.io.File
import java.time.Instant
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean

class AndroidSmokeActivity : ComponentActivity() {
    private val steps = JSONArray()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        lifecycleScope.launch {
            runSmoke()
            finish()
        }
    }

    private suspend fun runSmoke() {
        val reportFileName = requiredExtra("report_file", DEFAULT_REPORT_FILE)
        val reportFile = File(filesDir, reportFileName)
        reportFile.delete()

        val serverUrl = requiredExtra("server_url").trim().trimEnd('/')
        val enrollmentUrl = requiredExtra("enrollment_url")
        val accessToken = requiredExtra("access_token")
        val userEmail = requiredExtra("user_email").trim().lowercase()
        val userName = requiredExtra("user_name", userEmail).trim().ifBlank { userEmail }
        val expiresAt = requiredExtra("expires_at")
        val includeAttachTicket = intent.getBooleanExtra("include_attach_ticket", true)

        val settingsRepository = SettingsRepository(applicationContext)
        val deviceKeyManager = DeviceKeyManager()
        val sessionRepository = SessionManagerRepository(settingsRepository)
        var sessions: List<ClientSession> = emptyList()
        var attachSession: ClientSession? = null
        var attachTicket: MobileAttachTicketResponse? = null

        step("configure_settings") {
            settingsRepository.saveServerUrl(serverUrl)
            JSONObject()
                .put("server_url_host", hostFromUrl(serverUrl))
                .put("user_email", userEmail)
        }

        step("enroll_device_certificate") {
            val result = DeviceEnrollmentRepository(settingsRepository, deviceKeyManager)
                .enrollFromQr(enrollmentUrl)
            JSONObject()
                .put("device_id", result.deviceId)
                .put("expires_at", result.expiresAt)
                .put("device_name_present", !result.deviceName.isNullOrBlank())
        }

        step("seed_device_auth") {
            settingsRepository.saveAuth(accessToken, userEmail, userName, expiresAt)
            JSONObject()
                .put("user_email", userEmail)
                .put("expires_at", expiresAt)
        }

        step("client_bootstrap") {
            val payload = retrySmokeRead { sessionRepository.fetchBootstrap(serverUrl) }
            JSONObject()
                .put("auth_mode", payload.auth.mode)
                .put("device_auth_endpoint", payload.auth.deviceAuthEndpoint)
                .put("mobile_terminal_supported", payload.externalAccess.mobileTerminalSupported)
        }

        step("auth_session") {
            val payload = sessionRepository.fetchAuthSession(serverUrl, accessToken)
            JSONObject()
                .put("authenticated", payload.authenticated)
                .put("auth_type", payload.authType)
                .put("email", payload.email)
        }

        step("client_sessions") {
            sessions = sessionRepository.fetchSessions(serverUrl, accessToken)
            JSONObject()
                .put("count", sessions.size)
                .put("mobile_terminal_supported_count", sessions.count { it.mobileTerminal?.supported == true })
        }

        step("analytics_summary") {
            val payload = sessionRepository.fetchAnalytics(serverUrl, accessToken)
            JSONObject()
                .put("generated_at_present", payload.generatedAt.isNotBlank())
                .put("active_sessions", payload.kpis.activeSessions.value)
                .put("attach_available", payload.attachAvailable)
        }

        step("app_artifact_metadata") {
            val metadata = apiService(serverUrl, settingsRepository).getAppArtifactMetadata("session-manager-android")
            JSONObject()
                .put("artifact_hash_present", !metadata.artifactHash.isNullOrBlank())
                .put("version_name", metadata.versionName)
                .put("size_bytes", metadata.sizeBytes)
        }

        if (includeAttachTicket) {
            step("mobile_attach_ticket") {
                val supported = sessions.firstOrNull { it.mobileTerminal?.supported == true }
                    ?: return@step JSONObject()
                        .put("status_override", "skipped")
                        .put("reason", "no session advertises mobile_terminal.supported")
                attachSession = supported
                val path = sessionRepository.mobileAttachTicketPath(
                    baseUrl = serverUrl,
                    sessionId = supported.id,
                    advertisedEndpoint = supported.mobileTerminal?.ticketEndpoint,
                )
                val proof = deviceKeyManager.signTicketRequest(
                    method = "POST",
                    path = path,
                    sessionId = supported.id,
                    actorEmail = userEmail,
                )
                val ticket = sessionRepository
                    .createMobileAttachTicket(serverUrl, accessToken, supported.id, proof)
                    .getOrThrow()
                attachTicket = ticket
                JSONObject()
                    .put("session_id", supported.id)
                    .put("ticket_id_present", ticket.ticketId.isNotBlank())
                    .put("device_key_id", ticket.deviceKeyId)
                    .put("ws_url_host", hostFromUrl(ticket.wsUrl))
                    .put("expires_at", ticket.expiresAt)
            }

            step("mobile_terminal_socket") {
                val supported = attachSession
                    ?: return@step JSONObject()
                        .put("status_override", "skipped")
                        .put("reason", "mobile attach ticket was skipped")
                val ticket = attachTicket
                    ?: return@step JSONObject()
                        .put("status_override", "skipped")
                        .put("reason", "mobile attach ticket was not minted")
                runMobileTerminalSocketSmoke(
                    sessionRepository = sessionRepository,
                    ticket = ticket,
                    accessToken = accessToken,
                    deviceKeyManager = deviceKeyManager,
                    session = supported,
                    actorEmail = userEmail,
                )
            }
        }

        val blockers = countSteps("blocked")
        val report = JSONObject()
            .put("schema_version", 1)
            .put("generated_at", Instant.now().toString())
            .put("server_url_host", hostFromUrl(serverUrl))
            .put("steps", steps)
            .put(
                "summary",
                JSONObject()
                    .put("passed", countSteps("passed"))
                    .put("skipped", countSteps("skipped"))
                    .put("blocked", blockers)
                    .put("status", if (blockers == 0) "passed" else "blocked"),
            )
        withContext(Dispatchers.IO) {
            reportFile.writeText(report.toString(2))
        }
        Log.i(TAG, "android_smoke_report_written path=${reportFile.name} blocked=$blockers")
    }

    private suspend fun step(id: String, block: suspend () -> JSONObject) {
        val startedAt = Instant.now()
        val payload = try {
            block()
        } catch (error: Throwable) {
            val result = JSONObject()
                .put("id", id)
                .put("status", "blocked")
                .put("started_at", startedAt.toString())
                .put("finished_at", Instant.now().toString())
                .put("error_class", error.javaClass.name)
                .put("detail", error.message.orEmpty().take(MAX_DETAIL_CHARS))
            steps.put(result)
            Log.w(TAG, "android_smoke_step_blocked id=$id", error)
            return
        }
        val status = payload.optString("status_override", "passed")
        payload.remove("status_override")
        steps.put(
            JSONObject()
                .put("id", id)
                .put("status", status)
                .put("started_at", startedAt.toString())
                .put("finished_at", Instant.now().toString())
                .put("detail", payload),
        )
    }

    private fun countSteps(status: String): Int =
        (0 until steps.length()).count { index ->
            steps.optJSONObject(index)?.optString("status") == status
        }

    private suspend fun apiService(serverUrl: String, settingsRepository: SettingsRepository): ApiService {
        val json = Json { ignoreUnknownKeys = true; coerceInputValues = true }
        return Retrofit.Builder()
            .baseUrl(serverUrl.trim().trimEnd('/') + "/")
            .client(HttpClientFactory(settingsRepository).create(includeLogging = false))
            .addConverterFactory(json.asConverterFactory("application/json".toMediaType()))
            .build()
            .create(ApiService::class.java)
    }

    private suspend fun <T> retrySmokeRead(block: suspend () -> T): T {
        var lastError: Throwable? = null
        repeat(SMOKE_READ_ATTEMPTS) { attempt ->
            try {
                return block()
            } catch (error: Throwable) {
                lastError = error
                if (attempt < SMOKE_READ_ATTEMPTS - 1) {
                    delay(SMOKE_READ_RETRY_DELAY_MS)
                }
            }
        }
        throw lastError ?: IllegalStateException("smoke read failed")
    }

    private suspend fun runMobileTerminalSocketSmoke(
        sessionRepository: SessionManagerRepository,
        ticket: MobileAttachTicketResponse,
        accessToken: String,
        deviceKeyManager: DeviceKeyManager,
        session: ClientSession,
        actorEmail: String,
    ): JSONObject = withTimeout(SOCKET_SMOKE_TIMEOUT_MS) {
        val completed = AtomicBoolean(false)
        val result = CompletableDeferred<JSONObject>()
        var socket: WebSocket? = null

        fun finish(payload: JSONObject) {
            if (completed.compareAndSet(false, true)) {
                socket?.close(1000, "android smoke complete")
                result.complete(payload)
            }
        }

        fun fail(error: Throwable) {
            if (completed.compareAndSet(false, true)) {
                socket?.close(1000, "android smoke failed")
                result.completeExceptionally(error)
            }
        }

        val wsNonce = UUID.randomUUID().toString()
        val wsSignature = deviceKeyManager.signWebSocketAuth(
            ticketId = ticket.ticketId,
            sessionId = session.id,
            actorEmail = actorEmail,
            deviceKeyId = ticket.deviceKeyId,
            nonce = wsNonce,
        )

        socket = sessionRepository.openMobileTerminalSocket(
            ticket = ticket,
            accessToken = accessToken,
            listener = object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    val authFrame = JSONObject()
                        .put("type", "auth")
                        .put("ticket_id", ticket.ticketId)
                        .put("ticket_secret", ticket.ticketSecret)
                        .put("device_key_id", ticket.deviceKeyId)
                        .put("nonce", wsNonce)
                        .put("signature", wsSignature)
                    webSocket.send(authFrame.toString())
                    webSocket.send(
                        JSONObject()
                            .put("type", "resize")
                            .put("cols", 80)
                            .put("rows", 24)
                            .toString(),
                    )
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    val payload = runCatching { JSONObject(text) }.getOrNull() ?: return
                    when (payload.optString("type")) {
                        "status" -> finish(
                            JSONObject()
                                .put("session_id", session.id)
                                .put("ws_url_host", hostFromUrl(ticket.wsUrl))
                                .put("first_frame_type", "status")
                                .put("state", payload.optString("state")),
                        )
                        "output" -> finish(
                            JSONObject()
                                .put("session_id", session.id)
                                .put("ws_url_host", hostFromUrl(ticket.wsUrl))
                                .put("first_frame_type", "output")
                                .put("encoding", payload.optString("encoding"))
                                .put("mode", payload.optString("mode"))
                                .put("data_present", payload.optString("data").isNotBlank()),
                        )
                        "error" -> fail(
                            IllegalStateException(
                                payload.optString("message", "terminal socket returned error"),
                            ),
                        )
                    }
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    val detail = buildString {
                        response?.let {
                            append(it.message.ifBlank { "HTTP error" })
                            append(" (")
                            append(it.code)
                            append("): ")
                        }
                        append(t.message ?: "Terminal socket failed")
                    }
                    fail(IllegalStateException(detail, t))
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    fail(IllegalStateException("Terminal socket closed before output ($code): $reason"))
                }
            },
        )
        try {
            result.await()
        } finally {
            if (!completed.get()) {
                socket?.close(1000, "android smoke cancelled")
            }
        }
    }

    private fun requiredExtra(name: String, defaultValue: String? = null): String {
        val value = intent.getStringExtra(name)?.trim().orEmpty()
        if (value.isNotEmpty()) {
            return value
        }
        return defaultValue ?: error("Missing required extra: $name")
    }

    private fun hostFromUrl(url: String): String =
        runCatching { java.net.URI(url).host.orEmpty() }.getOrDefault("")

    private companion object {
        const val TAG = "SM_ADB_SMOKE"
        const val DEFAULT_REPORT_FILE = "android-smoke-report.json"
        const val MAX_DETAIL_CHARS = 500
        const val SOCKET_SMOKE_TIMEOUT_MS = 10_000L
        const val SMOKE_READ_ATTEMPTS = 6
        const val SMOKE_READ_RETRY_DELAY_MS = 1_000L
    }
}
