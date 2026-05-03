package li.rajeshgo.sm.data.repository

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import java.io.IOException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import li.rajeshgo.sm.data.model.ActivityActionRow
import li.rajeshgo.sm.data.model.AnalyticsSummary
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.DeviceGoogleAuthResponse
import li.rajeshgo.sm.data.model.EnsureMaintainerResponse
import li.rajeshgo.sm.data.model.MobileAttachTicketResponse
import li.rajeshgo.sm.data.model.RequestStatusResponse
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.data.model.ToolCallRow
import li.rajeshgo.sm.data.remote.ApiService
import li.rajeshgo.sm.data.remote.AuthInterceptor
import li.rajeshgo.sm.data.security.DeviceProof
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.logging.HttpLoggingInterceptor
import retrofit2.Retrofit
import retrofit2.HttpException
import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import java.net.URI

open class SessionManagerRequestException(message: String, cause: Throwable? = null) : IllegalStateException(message, cause)
class SessionManagerAuthException(message: String, cause: Throwable? = null) : SessionManagerRequestException(message, cause)
class SessionManagerTransientException(message: String, cause: Throwable? = null) : SessionManagerRequestException(message, cause)
class SessionManagerBackendUnavailableException(message: String, cause: Throwable? = null) : SessionManagerRequestException(message, cause)

class SessionManagerRepository {
    private val json = Json { ignoreUnknownKeys = true }

    private companion object {
        private const val READ_RETRY_ATTEMPTS = 3
        private const val READ_RETRY_BASE_DELAY_MS = 600L
        private const val GENERIC_TRANSIENT_READ_MESSAGE = "Server temporarily unavailable. Retrying soon."
        private const val GENERIC_TRANSIENT_WRITE_MESSAGE = "Server temporarily unavailable. Try again."
        private const val BACKEND_UNREACHABLE_ERROR = "backend_unreachable"
    }

    private data class ServerErrorPayload(
        val code: String? = null,
        val message: String? = null,
    )

    private fun httpClient(token: String = ""): OkHttpClient {
        val logger = HttpLoggingInterceptor().apply { level = HttpLoggingInterceptor.Level.BASIC }
        return OkHttpClient.Builder()
            .addInterceptor(AuthInterceptor { token })
            .addInterceptor(logger)
            .build()
    }

    private fun api(baseUrl: String, token: String = ""): ApiService {
        require(baseUrl.isNotBlank()) { "Server URL is required" }
        val normalizedBaseUrl = baseUrl.trim().trimEnd('/') + "/"
        return Retrofit.Builder()
            .baseUrl(normalizedBaseUrl)
            .client(httpClient(token))
            .addConverterFactory(json.asConverterFactory("application/json".toMediaType()))
            .build()
            .create(ApiService::class.java)
    }

    private fun extractServerError(error: HttpException): ServerErrorPayload? {
        val body = runCatching { error.response()?.errorBody()?.string() }.getOrNull()?.trim().orEmpty()
        if (body.isBlank()) {
            return null
        }
        return runCatching {
            val obj = json.parseToJsonElement(body).jsonObject
            val code = obj["error"]
                ?.jsonPrimitive
                ?.content
                ?.trim()
                ?.takeIf { it.isNotBlank() }
            val message = listOf("detail", "message")
                .firstNotNullOfOrNull { key ->
                    obj[key]
                        ?.jsonPrimitive
                        ?.content
                        ?.trim()
                        ?.takeIf { it.isNotBlank() }
                } ?: body.takeIf { !it.startsWith("<!DOCTYPE") && !it.startsWith("<html", ignoreCase = true) }
            ServerErrorPayload(code = code, message = message)
        }.getOrNull() ?: ServerErrorPayload(
            message = body.takeIf { !it.startsWith("<!DOCTYPE") && !it.startsWith("<html", ignoreCase = true) }
        )
    }

    private suspend fun <T> executeReadRequest(
        baseUrl: String,
        token: String = "",
        block: suspend (ApiService) -> T,
    ): T {
        var lastTransient: Throwable? = null
        repeat(READ_RETRY_ATTEMPTS) { attempt ->
            try {
                return block(api(baseUrl, token))
            } catch (error: HttpException) {
                when (error.code()) {
                    401, 403 -> throw SessionManagerAuthException("Session expired. Sign in again.", error)
                    502, 503, 504 -> {
                        val serverError = extractServerError(error)
                        if (serverError?.code == BACKEND_UNREACHABLE_ERROR) {
                            throw SessionManagerBackendUnavailableException(
                                serverError.message ?: GENERIC_TRANSIENT_READ_MESSAGE,
                                error,
                            )
                        }
                        lastTransient = error
                        if (attempt < READ_RETRY_ATTEMPTS - 1) {
                            delay(READ_RETRY_BASE_DELAY_MS * (attempt + 1))
                            return@repeat
                        }
                        throw SessionManagerTransientException(
                            serverError?.message ?: GENERIC_TRANSIENT_READ_MESSAGE,
                            error,
                        )
                    }
                    else -> throw SessionManagerRequestException(
                        extractServerError(error)?.message ?: "Request failed (${error.code()})",
                        error,
                    )
                }
            } catch (error: IOException) {
                lastTransient = error
                if (attempt < READ_RETRY_ATTEMPTS - 1) {
                    delay(READ_RETRY_BASE_DELAY_MS * (attempt + 1))
                    return@repeat
                }
                throw SessionManagerTransientException("Network unavailable. Retrying soon.", error)
            }
        }
        throw SessionManagerTransientException("Server temporarily unavailable. Retrying soon.", lastTransient)
    }

    private fun classifyWriteFailure(error: Throwable): Throwable {
        if (error is HttpException) {
            return when (error.code()) {
                401, 403 -> SessionManagerAuthException("Session expired. Sign in again.", error)
                502, 503, 504 -> {
                    val serverError = extractServerError(error)
                    if (serverError?.code == BACKEND_UNREACHABLE_ERROR) {
                        SessionManagerBackendUnavailableException(
                            serverError.message ?: GENERIC_TRANSIENT_WRITE_MESSAGE,
                            error,
                        )
                    } else {
                        SessionManagerTransientException(
                            serverError?.message ?: GENERIC_TRANSIENT_WRITE_MESSAGE,
                            error,
                        )
                    }
                }
                else -> SessionManagerRequestException(
                    extractServerError(error)?.message ?: "Request failed (${error.code()})",
                    error,
                )
            }
        }
        if (error is IOException) {
            return SessionManagerTransientException("Network unavailable. Try again.", error)
        }
        return error
    }

    suspend fun fetchBootstrap(baseUrl: String): ClientBootstrapResponse = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl) { it.getBootstrap() }
    }

    suspend fun exchangeGoogleIdToken(baseUrl: String, idToken: String): DeviceGoogleAuthResponse = withContext(Dispatchers.IO) {
        api(baseUrl).exchangeGoogleToken(li.rajeshgo.sm.data.model.DeviceGoogleAuthRequest(idToken))
    }

    suspend fun fetchAuthSession(baseUrl: String, token: String) = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getAuthSession() }
    }

    suspend fun fetchSessions(baseUrl: String, token: String): List<ClientSession> = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getClientSessions().sessions }
    }

    suspend fun fetchAnalytics(baseUrl: String, token: String): AnalyticsSummary = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getAnalyticsSummary() }
    }

    suspend fun killSession(baseUrl: String, token: String, sessionId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching {
            val response = api(baseUrl, token).killSession(sessionId)
            check(response.status == "killed") { response.error ?: "Kill request failed" }
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun createMobileAttachTicket(
        baseUrl: String,
        token: String,
        sessionId: String,
        proof: DeviceProof,
    ): Result<MobileAttachTicketResponse> = withContext(Dispatchers.IO) {
        runCatching {
            api(baseUrl, token).createMobileAttachTicket(
                sessionId = sessionId,
                deviceKeyId = proof.deviceKeyId,
                timestamp = proof.timestamp,
                nonce = proof.nonce,
                signature = proof.signature,
            )
        }.mapFailure(::classifyWriteFailure)
    }

    fun mobileAttachTicketPath(
        baseUrl: String,
        sessionId: String,
        advertisedEndpoint: String? = null,
    ): String {
        val advertised = advertisedEndpoint.orEmpty().trim()
        if (advertised.isNotEmpty()) {
            val path = runCatching { URI(advertised).rawPath.orEmpty() }.getOrDefault("")
            return normalizePath(path.ifBlank { advertised.substringBefore('?').substringBefore('#') })
        }
        val prefix = runCatching {
            URI(baseUrl.trim()).rawPath.orEmpty().trimEnd('/')
        }.getOrDefault("")
        return "$prefix/client/sessions/$sessionId/attach-ticket"
    }

    private fun normalizePath(path: String): String {
        val trimmed = path.trim()
        if (trimmed.isEmpty() || trimmed == "/") {
            return "/"
        }
        return if (trimmed.startsWith("/")) trimmed else "/$trimmed"
    }

    fun openMobileTerminalSocket(
        ticket: MobileAttachTicketResponse,
        listener: WebSocketListener,
    ): WebSocket {
        val request = Request.Builder()
            .url(ticket.wsUrl)
            .build()
        return httpClient().newWebSocket(request, listener)
    }

    suspend fun requestStatus(baseUrl: String, token: String): Result<RequestStatusResponse> = withContext(Dispatchers.IO) {
        runCatching {
            api(baseUrl, token).requestStatus()
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun ensureMaintainer(baseUrl: String, token: String): Result<EnsureMaintainerResponse> = withContext(Dispatchers.IO) {
        runCatching {
            api(baseUrl, token).ensureMaintainer()
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchSessionDetail(baseUrl: String, token: String, session: ClientSession): SessionDetail = withContext(Dispatchers.IO) {
        val service = api(baseUrl, token)
        coroutineScope {
            val outputDeferred = async { runCatching { service.getSessionOutput(session.id, lines = 10).output } }
            val actionsDeferred = async {
                if (session.provider == "codex-app") {
                    runCatching { summarizeActions(service.getActivityActions(session.id, limit = 10).actions) }
                } else {
                    runCatching { summarizeToolCalls(session.provider ?: "claude", service.getToolCalls(session.id, limit = 10).toolCalls) }
                }
            }

            val outputResult = outputDeferred.await()
            val actionsResult = actionsDeferred.await()
            val lastError = listOf(outputResult.exceptionOrNull(), actionsResult.exceptionOrNull())
                .firstOrNull()?.message

            SessionDetail(
                actionLines = actionsResult.getOrElse { listOf("n/a (unavailable)") },
                tailLines = outputResult.getOrElse { "" }.lines().takeLast(10).filter { it.isNotEmpty() }.ifEmpty { listOf("-") },
                lastError = lastError,
            )
        }
    }

    private inline fun <T> Result<T>.mapFailure(transform: (Throwable) -> Throwable): Result<T> {
        val error = exceptionOrNull() ?: return this
        return Result.failure(transform(error))
    }

    private fun summarizeToolCalls(provider: String, rows: List<ToolCallRow>): List<String> {
        if (rows.isEmpty()) {
            return if (provider == "codex") listOf("n/a (no hooks)") else listOf("-")
        }
        return rows.take(10).map { row ->
            val suffix = row.timestamp?.takeIf { it.isNotBlank() }?.let { " (${it})" } ?: ""
            "${row.toolName ?: "-"}$suffix"
        }
    }

    private fun summarizeActions(rows: List<ActivityActionRow>): List<String> {
        if (rows.isEmpty()) {
            return listOf("-")
        }
        return rows.take(10).map { row ->
            val summary = row.summaryText ?: row.actionKind ?: "action"
            val statusSuffix = row.status?.takeIf { it.isNotBlank() }?.let { " [$it]" } ?: ""
            val timeSuffix = (row.endedAt ?: row.startedAt)?.takeIf { it.isNotBlank() }?.let { " ($it)" } ?: ""
            "$summary$statusSuffix$timeSuffix"
        }
    }
}
