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
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.DeviceGoogleAuthResponse
import li.rajeshgo.sm.data.model.MobileAttachTicketResponse
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.data.model.StudioSshStatusResponse
import li.rajeshgo.sm.data.model.ToolCallRow
import li.rajeshgo.sm.data.model.WhatRequestBody
import li.rajeshgo.sm.data.model.WhatRequestRecord
import li.rajeshgo.sm.data.remote.ApiService
import li.rajeshgo.sm.data.remote.HttpClientFactory
import li.rajeshgo.sm.data.security.DeviceProof
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import retrofit2.Retrofit
import retrofit2.HttpException
import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import java.net.URI

open class SessionManagerRequestException(message: String, cause: Throwable? = null) : IllegalStateException(message, cause)
class SessionManagerAuthException(message: String, cause: Throwable? = null) : SessionManagerRequestException(message, cause)
class SessionManagerTransientException(message: String, cause: Throwable? = null) : SessionManagerRequestException(message, cause)
class SessionManagerBackendUnavailableException(message: String, cause: Throwable? = null) : SessionManagerRequestException(message, cause)

const val RETIRE_REQUEST_FAILED_MESSAGE = "Retire request failed"
private val ACTIVE_WHAT_REQUEST_ID = Regex("""\bbtw-[A-Za-z0-9_-]+\b""")

fun activeWhatRequestId(detail: String?): String? {
    return detail?.let { ACTIVE_WHAT_REQUEST_ID.find(it)?.value }
}

fun stripTerminalControls(text: String): String {
    val output = StringBuilder(text.length)
    var index = 0
    while (index < text.length) {
        val char = text[index++]
        if (char == '\u001B') {
            if (index >= text.length) continue
            when (text[index++]) {
                '[' -> {
                    while (index < text.length) {
                        if (text[index++] in '@'..'~') break
                    }
                }
                ']' -> {
                    while (index < text.length) {
                        val next = text[index++]
                        if (next == '\u0007') break
                        if (next == '\u001B' && index < text.length && text[index] == '\\') {
                            index++
                            break
                        }
                    }
                }
                'P', 'X', '^', '_' -> {
                    while (index < text.length) {
                        if (text[index++] == '\u001B' && index < text.length && text[index] == '\\') {
                            index++
                            break
                        }
                    }
                }
            }
            continue
        }
        if (char == '\u009B') {
            while (index < text.length) {
                if (text[index++] in '@'..'~') break
            }
            continue
        }
        if (char == '\n' || char == '\t' || !char.isISOControl()) {
            output.append(char)
        }
    }
    return output.toString()
}

/**
 * The last payload each tab drew, kept in memory for the life of the process
 * (spec 1782 J5). A screen whose view model is recreated starts from it and
 * refreshes in the background instead of showing a spinner. Sign-out clears it.
 */
/** The thread key in a `/inbox/thread/{key}?…` redirect target, decoded; null for anything else. */
fun threadKeyFromLocation(location: String?): String? {
    val path = location?.substringAfter("/inbox/thread/", "")?.substringBefore('?')?.substringBefore('#')
    return path?.takeIf { it.isNotBlank() && '/' !in it }?.let { java.net.URLDecoder.decode(it, "UTF-8") }
}

object ScreenCache {
    @Volatile var watch: List<ClientSession>? = null
    @Volatile var board: li.rajeshgo.sm.data.model.BoardResponse? = null
    @Volatile var queue: li.rajeshgo.sm.data.model.QueueOverview? = null
    /** Inbox rows per filter query. */
    val inbox = java.util.concurrent.ConcurrentHashMap<String, li.rajeshgo.sm.data.model.InboxResponse>()

    /** Bumped by every sign-out, so a read that started before it cannot refill the cache. */
    @Volatile var generation = 0L
        private set

    @Synchronized
    fun clear() {
        generation++
        watch = null
        board = null
        queue = null
        inbox.clear()
    }

    /** Runs [read] and stores its result with [store], unless a sign-out happened meanwhile. */
    suspend fun <T> remember(read: suspend () -> T, store: (T) -> Unit): T {
        val started = generation
        val value = read()
        synchronized(this) { if (generation == started) store(value) }
        return value
    }
}

class SessionManagerRepository(
    private val settingsRepository: SettingsRepository? = null,
) {
    private val json = Json { ignoreUnknownKeys = true }
    private val httpClientFactory = HttpClientFactory(settingsRepository)

    private companion object {
        private const val READ_RETRY_ATTEMPTS = 3
        private const val READ_RETRY_BASE_DELAY_MS = 600L
        private const val WHAT_REQUEST_POLL_INTERVAL_MS = 500L
        private const val GENERIC_TRANSIENT_READ_MESSAGE = "Server temporarily unavailable. Retrying soon."
        private const val GENERIC_TRANSIENT_WRITE_MESSAGE = "Server temporarily unavailable. Try again."
        private const val BACKEND_UNREACHABLE_ERROR = "backend_unreachable"
    }

    private data class ServerErrorPayload(
        val code: String? = null,
        val message: String? = null,
    )

    private suspend fun httpClient(token: String = ""): OkHttpClient = httpClientFactory.create(token = token)

    private suspend fun api(baseUrl: String, token: String = "", readTimeoutSeconds: Long = 30): ApiService {
        require(baseUrl.isNotBlank()) { "Server URL is required" }
        val normalizedBaseUrl = baseUrl.trim().trimEnd('/') + "/"
        return Retrofit.Builder()
            .baseUrl(normalizedBaseUrl)
            .client(httpClientFactory.create(token = token, readTimeoutSeconds = readTimeoutSeconds))
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
    ): T = executeReadRequest(api(baseUrl, token), block)

    private suspend fun <T> executeReadRequest(
        service: ApiService,
        block: suspend (ApiService) -> T,
    ): T {
        var lastTransient: Throwable? = null
        repeat(READ_RETRY_ATTEMPTS) { attempt ->
            try {
                return block(service)
            } catch (error: HttpException) {
                when (error.code()) {
                    401 -> throw SessionManagerAuthException("Session expired. Sign in again.", error)
                    403 -> throw forbiddenRequestFailure(error)
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

    internal fun classifyWriteFailure(error: Throwable): Throwable {
        if (error is HttpException) {
            return when (error.code()) {
                401 -> SessionManagerAuthException("Session expired. Sign in again.", error)
                403 -> forbiddenRequestFailure(error)
                429 -> SessionManagerTransientException("Connection busy. Retrying soon.", error)
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

    suspend fun fetchHostStatus(baseUrl: String, token: String): li.rajeshgo.sm.data.model.HostStatus = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getHostStatus() }
    }

    suspend fun fetchSessionModelCatalog(baseUrl: String, token: String, provider: String, workingDir: String): li.rajeshgo.sm.data.model.SessionModelsResponse = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getSessionModels(provider, workingDir) }
    }

    suspend fun fetchSessionModels(baseUrl: String, token: String, provider: String, workingDir: String): List<String> = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getSessionModels(provider, workingDir).models }
    }

    suspend fun fetchSession(baseUrl: String, token: String, sessionId: String): ClientSession = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getClientSession(sessionId) }
    }

    suspend fun fetchSessions(baseUrl: String, token: String): List<ClientSession> = withContext(Dispatchers.IO) {
        ScreenCache.remember({ coroutineScope {
            val sessions = async { executeReadRequest(baseUrl, token) { it.getClientSessions().sessions } }
            val obligations = async { executeReadRequest(baseUrl, token) { it.getSessionObligations().sessions }.associateBy { it.sessionId } }
            val jobs = async { executeReadRequest(baseUrl, token) { it.getSessionJobs().jobs } }
            val watch = async { executeReadRequest(baseUrl, token) { it.getWatchState().sessions }.associateBy { it.id } }
            val obligationsById = obligations.await()
            val allJobs = jobs.await()
            val watchById = watch.await()
            sessions.await().map { session -> session.copy(
                obligations = obligationsById[session.id],
                jobs = allJobs.filter { it.isAwaitedBy(session.id) },
                facts = watchById[session.id]?.facts,
                lastTicket = watchById[session.id]?.lastTicket,
                attention = watchById[session.id]?.attention,
            ) }
        } }) { ScreenCache.watch = it }
    }

    /** ✓ Answered (spec 1782 C4): clears the agent's open questions without telling it. */
    suspend fun answerNeedsYou(baseUrl: String, token: String, sessionId: String): Result<li.rajeshgo.sm.data.model.AgentFacts?> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).answerNeedsYou(sessionId).facts }.mapFailure(::classifyWriteFailure)
    }

    suspend fun createSession(baseUrl: String, token: String, request: li.rajeshgo.sm.data.model.CreateSessionRequest): Result<li.rajeshgo.sm.data.model.CreatedSession> = withContext(Dispatchers.IO) {
        runCatching {
            val service = api(baseUrl, token, readTimeoutSeconds = 180)
            service.createSession(request)
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchHandoffPolicy(baseUrl: String, token: String, sessionId: String) = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getHandoffPolicy(sessionId) }
    }

    suspend fun setHandoffPolicy(baseUrl: String, token: String, sessionId: String, patch: kotlinx.serialization.json.JsonObject) = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).setHandoffPolicy(sessionId, patch) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchHandoffDefaults(baseUrl: String, token: String) = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getHandoffDefaults() }
    }

    suspend fun setHandoffDefaults(baseUrl: String, token: String, patch: kotlinx.serialization.json.JsonObject) = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).setHandoffDefaults(patch) }.mapFailure(::classifyWriteFailure)
    }

    /** Owner settings shared with the web (`GET /client/settings`). */
    suspend fun fetchOwnerSettings(baseUrl: String, token: String) = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getOwnerSettings() }
    }

    suspend fun setOwnerSettings(baseUrl: String, token: String, patch: kotlinx.serialization.json.JsonObject) = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).setOwnerSettings(patch) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchLatestHostRestart(baseUrl: String, token: String) = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getLatestHostRestart() }
    }

    suspend fun restoreHostRestart(
        baseUrl: String,
        token: String,
        restartId: String,
        body: kotlinx.serialization.json.JsonObject,
    ) = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token, readTimeoutSeconds = 300).restoreHostRestart(restartId, body) }
            .mapFailure(::classifyWriteFailure)
    }

    suspend fun leaveHostRestartMember(baseUrl: String, token: String, restartId: String, sessionId: String) =
        withContext(Dispatchers.IO) {
            runCatching { api(baseUrl, token, readTimeoutSeconds = 120).leaveHostRestartMember(restartId, sessionId) }
                .mapFailure(::classifyWriteFailure)
        }

    suspend fun retireSession(baseUrl: String, token: String, sessionId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching {
            val response = api(baseUrl, token).retireSession(sessionId)
            check(response.status == "retired") { response.error ?: RETIRE_REQUEST_FAILED_MESSAGE }
        }.mapFailure(::classifyWriteFailure)
    }

    /**
     * Ask an agent a side question. With [attachOnConflict] false (Ask agent on
     * the Queue page) an already-active request fails with [WhatRequestBusyException]
     * instead of showing that request's unrelated answer.
     */
    suspend fun runWhatRequest(
        baseUrl: String,
        token: String,
        sessionId: String,
        prompt: String? = null,
        attachOnConflict: Boolean = true,
        onUpdate: (WhatRequestRecord) -> Unit,
    ): Result<WhatRequestRecord> = withContext(Dispatchers.IO) {
        runCatching {
            runWhatRequestWith(api(baseUrl, token), sessionId, prompt, attachOnConflict, onUpdate)
        }.mapFailure { if (it is WhatRequestBusyException) it else classifyWriteFailure(it) }
    }

    internal suspend fun runWhatRequestWith(
        service: ApiService,
        sessionId: String,
        prompt: String?,
        attachOnConflict: Boolean,
        onUpdate: (WhatRequestRecord) -> Unit,
    ): WhatRequestRecord {
        var current = try {
            service.createWhatRequest(
                sessionId = sessionId,
                request = WhatRequestBody(deliveryMode = "poll", prompt = prompt),
            )
        } catch (error: HttpException) {
            if (error.code() != 409) {
                throw classifyWriteFailure(error)
            }
            val detail = extractServerError(error)?.message
            if (!attachOnConflict) {
                throw WhatRequestBusyException(detail, error)
            }
            val activeRequestId = activeWhatRequestId(detail)
                ?: throw SessionManagerRequestException(
                    detail ?: "Another summary request is already active.",
                    error,
                )
            executeReadRequest(service) {
                it.getWhatRequest(activeRequestId)
            }
        } catch (error: WhatRequestBusyException) {
            throw error
        } catch (error: Throwable) {
            throw classifyWriteFailure(error)
        }

        onUpdate(current)
        while (current.status !in setOf("completed", "failed", "timed_out")) {
            delay(WHAT_REQUEST_POLL_INTERVAL_MS)
            current = executeReadRequest(service) {
                it.getWhatRequest(current.requestId)
            }
            onUpdate(current)
        }
        return current
    }

    suspend fun fetchAnalyticsSpend(baseUrl: String, token: String, provider: String?, range: String): li.rajeshgo.sm.data.model.SpendReport = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getAnalyticsSpend(provider, range) }
    }

    suspend fun fetchAnalyticsTime(baseUrl: String, token: String, range: String): li.rajeshgo.sm.data.model.TimeReport = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getAnalyticsTime(range) }
    }

    suspend fun fetchQueue(baseUrl: String, token: String): li.rajeshgo.sm.data.model.QueueOverview = withContext(Dispatchers.IO) {
        ScreenCache.remember({ executeReadRequest(baseUrl, token) { it.getQueue() } }) { ScreenCache.queue = it }
    }

    suspend fun fetchQueueStats(baseUrl: String, token: String, hours: Int): li.rajeshgo.sm.data.model.QueueStats = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getQueueStats(hours) }
    }

    suspend fun fetchUtilizationSeries(baseUrl: String, token: String, hours: Int): li.rajeshgo.sm.data.model.UtilizationSeries = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getUtilizationSeries(hours) }
    }

    suspend fun fetchQueueJobLog(baseUrl: String, token: String, jobId: String, lines: Int): li.rajeshgo.sm.data.model.QueueJobLog = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getQueueJobLog(jobId, lines) }
    }

    suspend fun cancelQueueJob(baseUrl: String, token: String, jobId: String, note: String?): Result<li.rajeshgo.sm.data.model.SessionJob> = withContext(Dispatchers.IO) {
        runCatching {
            api(baseUrl, token).cancelQueueJob(
                jobId,
                li.rajeshgo.sm.data.model.CancelQueueJobBody(note?.trim()?.takeIf { it.isNotEmpty() }),
            )
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchQueueStartCheck(baseUrl: String, token: String, jobId: String): li.rajeshgo.sm.data.model.QueueStartCheck = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getQueueStartCheck(jobId) }
    }

    suspend fun forceStartQueueJob(baseUrl: String, token: String, jobId: String): Result<li.rajeshgo.sm.data.model.SessionJob> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).forceStartQueueJob(jobId) }.mapFailure(::classifyWriteFailure)
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

    suspend fun openMobileTerminalSocket(
        ticket: MobileAttachTicketResponse,
        accessToken: String,
        listener: WebSocketListener,
    ): WebSocket {
        return httpClient().newBuilder().pingInterval(20, java.util.concurrent.TimeUnit.SECONDS).build().newWebSocket(mobileTerminalSocketRequest(ticket, accessToken), listener)
    }

    fun mobileTerminalSocketRequest(ticket: MobileAttachTicketResponse, accessToken: String): Request {
        val builder = Request.Builder().url(ticket.wsUrl)
        val token = accessToken.trim()
        if (token.isNotBlank()) {
            builder.header("Authorization", "Bearer $token")
        }
        return builder.build()
    }

    fun isRetryableMobileTerminalSocketFailure(httpStatusCode: Int?, message: String?): Boolean {
        if (httpStatusCode != null) {
            return httpStatusCode in setOf(404, 426, 502, 503, 504)
        }
        return message.orEmpty().contains("Expected HTTP 101 response", ignoreCase = true)
    }

    suspend fun fetchFollows(baseUrl: String, token: String): li.rajeshgo.sm.data.model.FollowsResponse = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getFollows() }
    }

    suspend fun followSession(baseUrl: String, token: String, sessionId: String, message: String?): Result<li.rajeshgo.sm.data.model.OwnerFollow> = withContext(Dispatchers.IO) {
        runCatching {
            api(baseUrl, token).followSession(sessionId, li.rajeshgo.sm.data.model.FollowSessionRequest(message?.trim()?.takeIf { it.isNotEmpty() }))
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun unfollowSession(baseUrl: String, token: String, sessionId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).unfollowSession(sessionId) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun followJob(baseUrl: String, token: String, jobId: String): Result<li.rajeshgo.sm.data.model.OwnerFollow> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).followJob(jobId) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun unfollowJob(baseUrl: String, token: String, jobId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).unfollowJob(jobId) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun ackFollow(baseUrl: String, token: String, followId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).ackFollow(followId) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchInbox(baseUrl: String, token: String, filter: String): li.rajeshgo.sm.data.model.InboxResponse = withContext(Dispatchers.IO) {
        ScreenCache.remember({ executeReadRequest(baseUrl, token) { it.getInbox(filter) } }) { ScreenCache.inbox[filter] = it }
    }

    suspend fun fetchBoard(baseUrl: String, token: String): li.rajeshgo.sm.data.model.BoardResponse = withContext(Dispatchers.IO) {
        ScreenCache.remember({ executeReadRequest(baseUrl, token) { it.getBoard() } }) { ScreenCache.board = it }
    }

    suspend fun searchOwnerNotes(url: String, token: String, query: String) = withContext(Dispatchers.IO) {
        executeReadRequest(url, token) { it.searchOwnerNotes(query) }
    }

    suspend fun getOwnerNote(url: String, token: String, id: String) = withContext(Dispatchers.IO) {
        executeReadRequest(url, token) { it.getOwnerNote(id) }
    }

    suspend fun createOwnerNote(url: String, token: String, body: String) = withContext(Dispatchers.IO) {
        runCatching { api(url, token).createOwnerNote(li.rajeshgo.sm.data.model.OwnerNoteWrite(body)) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun saveOwnerNote(url: String, token: String, note: li.rajeshgo.sm.data.model.OwnerNote, body: String) = withContext(Dispatchers.IO) {
        runCatching { api(url, token).saveOwnerNote(note.id, li.rajeshgo.sm.data.model.OwnerNoteWrite(body, ifVersion = note.version)) }
            .mapFailure(::classifyWriteFailure)
    }

    suspend fun deleteOwnerNote(url: String, token: String, id: String) = withContext(Dispatchers.IO) {
        runCatching { api(url, token).deleteOwnerNote(id) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun ownerNoteRevisions(url: String, token: String, id: String) = withContext(Dispatchers.IO) {
        executeReadRequest(url, token) { it.ownerNoteRevisions(id) }
    }

    suspend fun ownerNoteRevision(url: String, token: String, id: String, version: Long) = withContext(Dispatchers.IO) {
        executeReadRequest(url, token) { it.ownerNoteRevision(id, version) }
    }

    suspend fun restoreOwnerNote(url: String, token: String, id: String, version: Long) = withContext(Dispatchers.IO) {
        runCatching { api(url, token).restoreOwnerNote(id, li.rajeshgo.sm.data.model.OwnerNoteRestore(version)) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fileOwnerIssue(url: String, token: String, repo: String, title: String, body: String) = withContext(Dispatchers.IO) {
        runCatching { api(url, token, readTimeoutSeconds = 90).fileOwnerIssue(li.rajeshgo.sm.data.model.OwnerIssueRequest(repo, title, body)) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchBoardBadge(baseUrl: String, token: String): li.rajeshgo.sm.data.model.BoardBadge = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getBoardBadge() }
    }

    suspend fun markBoardSeen(baseUrl: String, token: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).markBoardSeen() }.mapFailure(::classifyWriteFailure)
    }

    suspend fun refreshBoard(baseUrl: String, token: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).refreshBoard() }.mapFailure(::classifyWriteFailure)
    }

    suspend fun reorderBoard(baseUrl: String, token: String, laneIds: List<Long>): Result<li.rajeshgo.sm.data.model.BoardResponse> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).putBoardOrder(li.rajeshgo.sm.data.model.BoardOrderRequest(laneIds)) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun addBoardLane(baseUrl: String, token: String, repo: String, number: Long): Result<li.rajeshgo.sm.data.model.BoardLaneAdded> = withContext(Dispatchers.IO) {
        // Adding a lane reads GitHub before it answers.
        runCatching { api(baseUrl, token, readTimeoutSeconds = 90).addBoardLane(li.rajeshgo.sm.data.model.BoardLaneRequest(repo, number)) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun endBoardLane(baseUrl: String, token: String, laneId: Long): Result<li.rajeshgo.sm.data.model.BoardResponse> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).endBoardLane(laneId) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchBoardStartOptions(
        baseUrl: String,
        token: String,
        repo: String,
        number: Long,
        startBlocked: Boolean = false,
    ): li.rajeshgo.sm.data.model.BoardStartOptions = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getBoardStartOptions(repo, number, startBlocked.takeIf { it }) }
    }

    suspend fun closeBoardTicket(baseUrl: String, token: String, repo: String, number: Long): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token, readTimeoutSeconds = 60).closeBoardTicket(li.rajeshgo.sm.data.model.BoardCloseRequest(repo, number)) }
            .mapFailure(::classifyWriteFailure)
    }

    suspend fun startBoardTicket(baseUrl: String, token: String, request: li.rajeshgo.sm.data.model.BoardStartRequest): Result<li.rajeshgo.sm.data.model.BoardStarted> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token, readTimeoutSeconds = 120).startBoardTicket(request) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun setBoardAutoStart(baseUrl: String, token: String, choice: li.rajeshgo.sm.data.model.BoardAutoStartChoice): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).setBoardAutoStart(choice); Unit }.mapFailure(::classifyWriteFailure)
    }

    suspend fun cancelBoardAutoStart(baseUrl: String, token: String, repo: String, number: Long): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).cancelBoardAutoStart(repo, number); Unit }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchBugReportOptions(baseUrl: String, token: String): li.rajeshgo.sm.data.model.BugReportOptions = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getBugReportOptions() }
    }

    suspend fun fileBugReport(baseUrl: String, token: String, request: li.rajeshgo.sm.data.model.BugReportRequest): Result<li.rajeshgo.sm.data.model.BugReportFiled> = withContext(Dispatchers.IO) {
        // Filing waits on GitHub and, with an agent, on the board and the start.
        runCatching { api(baseUrl, token, readTimeoutSeconds = 120).fileBugReport(request) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchReviewPolicies(baseUrl: String, token: String): li.rajeshgo.sm.data.model.ReviewPoliciesResponse = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getReviewPolicies() }
    }

    suspend fun putReviewPolicy(baseUrl: String, token: String, request: li.rajeshgo.sm.data.model.PutReviewPolicyRequest): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).putReviewPolicy(request); Unit }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchReviewStatus(baseUrl: String, token: String): li.rajeshgo.sm.data.model.ReviewStatus = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getReviewStatus() }
    }

    suspend fun checkGithubCodex(baseUrl: String, token: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).checkGithubCodex(); Unit }.mapFailure(::classifyWriteFailure)
    }

    suspend fun retryReviewRequest(baseUrl: String, token: String, requestId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).retryReviewRequest(requestId); Unit }.mapFailure(::classifyWriteFailure)
    }

    suspend fun ownReviewRequest(baseUrl: String, token: String, requestId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).ownReviewRequest(requestId); Unit }.mapFailure(::classifyWriteFailure)
    }

    suspend fun dismissReviewRequest(baseUrl: String, token: String, requestId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).dismissReviewRequest(requestId); Unit }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchGuestbook(baseUrl: String, token: String, repo: String?, before: Long?): li.rajeshgo.sm.data.model.GuestbookResponse = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getGuestbook(repo, before) }
    }

    suspend fun fetchAgentHistory(baseUrl: String, token: String, query: String?, before: String?): li.rajeshgo.sm.data.model.AgentHistoryResponse = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getAgentHistory(query, before) }
    }

    /** Restores an agent; the failure carries the server's reason (not stopped, no resume id, …). */
    suspend fun restoreSession(baseUrl: String, token: String, sessionId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token, readTimeoutSeconds = 120).restoreSession(sessionId); Unit }.mapFailure(::classifyWriteFailure)
    }

    /**
     * A work thread by its key, or the thread an agent's newest item is in when [key] is null
     * or names no thread (a Board row's ticket the agent never wrote under).
     */
    suspend fun fetchInboxThread(baseUrl: String, token: String, key: String?, sessionId: String?): li.rajeshgo.sm.data.model.InboxThread =
        withContext(Dispatchers.IO) {
            executeReadRequest(baseUrl, token) { service ->
                if (key != null) {
                    try {
                        return@executeReadRequest service.getInboxThread(key)
                    } catch (error: HttpException) {
                        if (error.code() != 404 || sessionId == null) throw error
                    }
                }
                val response = service.getAgentThread(requireNotNull(sessionId))
                response.body()?.takeIf { response.isSuccessful }
                    ?: threadKeyFromLocation(response.headers()["Location"])?.let { service.getInboxThread(it) }
                    ?: throw retrofit2.HttpException(response)
            }
        }

    suspend fun sendInboxThread(baseUrl: String, token: String, key: String, request: li.rajeshgo.sm.data.model.InboxSendRequest): Result<Unit> =
        withContext(Dispatchers.IO) {
            runCatching { api(baseUrl, token).sendInboxThread(key, request); Unit }.mapFailure(::classifyWriteFailure)
        }

    suspend fun markInboxDone(baseUrl: String, token: String, threadKey: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).markInboxDone(li.rajeshgo.sm.data.model.InboxDoneRequest(threadKey)) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun archiveInbox(baseUrl: String, token: String, threadKey: String, unarchive: Boolean = false): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching {
            val request = li.rajeshgo.sm.data.model.InboxDoneRequest(threadKey)
            if (unarchive) api(baseUrl, token).unarchiveInbox(request) else api(baseUrl, token).archiveInbox(request)
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchDocAskTarget(baseUrl: String, token: String, docId: String): li.rajeshgo.sm.data.model.DocAskTarget =
        withContext(Dispatchers.IO) { executeReadRequest(baseUrl, token) { it.getDocAskTarget(docId) } }

    suspend fun askDoc(baseUrl: String, token: String, docId: String, request: li.rajeshgo.sm.data.model.DocAskRequest): Result<Unit> =
        withContext(Dispatchers.IO) {
            runCatching { api(baseUrl, token, readTimeoutSeconds = 120).askDoc(docId, request); Unit }.mapFailure(::classifyWriteFailure)
        }

    suspend fun ackNotice(baseUrl: String, token: String, noticeId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).ackNotice(noticeId) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun registerPushToken(baseUrl: String, token: String, request: li.rajeshgo.sm.data.model.PushTokenRequest): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).registerPushToken(request) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun deletePushToken(baseUrl: String, token: String, pushToken: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).deletePushToken(li.rajeshgo.sm.data.model.DeletePushTokenRequest(pushToken)) }.mapFailure(::classifyWriteFailure)
    }

    suspend fun sendTestPush(baseUrl: String, token: String): Result<li.rajeshgo.sm.data.model.TestPushResponse> = withContext(Dispatchers.IO) {
        runCatching { api(baseUrl, token).sendTestPush() }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchStudioSshStatus(baseUrl: String, token: String): StudioSshStatusResponse = withContext(Dispatchers.IO) {
        executeReadRequest(baseUrl, token) { it.getStudioSshStatus() }
    }

    suspend fun setStudioSsh(baseUrl: String, token: String, enabled: Boolean): Result<StudioSshStatusResponse> = withContext(Dispatchers.IO) {
        runCatching {
            api(baseUrl, token).setStudioSsh(li.rajeshgo.sm.data.model.StudioSshToggleRequest(enabled))
        }.mapFailure(::classifyWriteFailure)
    }

    suspend fun fetchSessionDetail(baseUrl: String, token: String, session: ClientSession): SessionDetail = withContext(Dispatchers.IO) {
        val service = api(baseUrl, token)
        coroutineScope {
            val outputDeferred = async {
                runCatching {
                    val rendered = runCatching {
                        service.getSessionOutput(session.id, lines = 10, rendered = true).output
                    }.getOrNull()
                    rendered ?: service
                        .getSessionOutput(session.id, lines = 10, rendered = false)
                        .output
                        .orEmpty()
                }
            }
            val actionsDeferred = async {
                if (session.provider == "codex-app") {
                    runCatching { summarizeActions(service.getActivityActions(session.id, limit = 10).actions) }
                } else {
                    runCatching { summarizeToolCalls(session.provider ?: "claude", service.getToolCalls(session.id, limit = 10).toolCalls) }
                }
            }
            val contextDeferred = async {
                runCatching { service.getSessionContext(session.id) }
            }

            val outputResult = outputDeferred.await()
            val actionsResult = actionsDeferred.await()
            val contextResult = contextDeferred.await()
            val lastError = listOf(
                outputResult.exceptionOrNull(),
                actionsResult.exceptionOrNull(),
                contextResult.exceptionOrNull(),
            )
                .firstOrNull()?.message
            val context = contextResult.getOrNull()

            SessionDetail(
                actionLines = actionsResult.getOrElse { listOf("n/a (unavailable)") },
                tailLines = stripTerminalControls(outputResult.getOrElse { "" })
                    .lines()
                    .takeLast(10)
                    .filter { it.isNotBlank() }
                    .ifEmpty { listOf("-") },
                contextPercentage = context?.usedPercentage,
                contextState = context?.state,
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

// A forbidden action or gateway refusal does not prove the device login expired.
/** The agent is already answering another side question; nothing was sent. */
class WhatRequestBusyException(detail: String?, cause: Throwable? = null) :
    Exception(detail ?: "The agent is answering another question.", cause)

internal fun forbiddenRequestFailure(error: Throwable): SessionManagerRequestException =
    SessionManagerRequestException("Access was refused. Your sign-in is saved. Retry or check device enrollment in Settings.", error)
