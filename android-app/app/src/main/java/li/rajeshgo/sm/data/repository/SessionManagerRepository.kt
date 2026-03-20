package li.rajeshgo.sm.data.repository

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.ActivityActionRow
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.DeviceGoogleAuthResponse
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.data.model.ToolCallRow
import li.rajeshgo.sm.data.remote.ApiService
import li.rajeshgo.sm.data.remote.AuthInterceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.logging.HttpLoggingInterceptor
import retrofit2.Retrofit
import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory

class SessionManagerRepository {
    private val json = Json { ignoreUnknownKeys = true }

    private fun api(baseUrl: String, token: String = ""): ApiService {
        require(baseUrl.isNotBlank()) { "Server URL is required" }
        val normalizedBaseUrl = baseUrl.trim().trimEnd('/') + "/"
        val logger = HttpLoggingInterceptor().apply { level = HttpLoggingInterceptor.Level.BASIC }
        val client = OkHttpClient.Builder()
            .addInterceptor(AuthInterceptor { token })
            .addInterceptor(logger)
            .build()
        return Retrofit.Builder()
            .baseUrl(normalizedBaseUrl)
            .client(client)
            .addConverterFactory(json.asConverterFactory("application/json".toMediaType()))
            .build()
            .create(ApiService::class.java)
    }

    suspend fun fetchBootstrap(baseUrl: String): ClientBootstrapResponse = withContext(Dispatchers.IO) {
        api(baseUrl).getBootstrap()
    }

    suspend fun exchangeGoogleIdToken(baseUrl: String, idToken: String): DeviceGoogleAuthResponse = withContext(Dispatchers.IO) {
        api(baseUrl).exchangeGoogleToken(li.rajeshgo.sm.data.model.DeviceGoogleAuthRequest(idToken))
    }

    suspend fun fetchAuthSession(baseUrl: String, token: String) = withContext(Dispatchers.IO) {
        api(baseUrl, token).getAuthSession()
    }

    suspend fun fetchSessions(baseUrl: String, token: String): List<ClientSession> = withContext(Dispatchers.IO) {
        api(baseUrl, token).getClientSessions().sessions
    }

    suspend fun killSession(baseUrl: String, token: String, sessionId: String): Result<Unit> = withContext(Dispatchers.IO) {
        runCatching {
            val response = api(baseUrl, token).killSession(sessionId)
            check(response.status == "killed") { response.error ?: "Kill request failed" }
        }
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
