package li.rajeshgo.sm.ui.artifacts

import android.app.Application
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.serialization.json.*
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

object ArtifactsNavigation {
    var open by mutableStateOf(false)
    var name by mutableStateOf<String?>(null)
}

fun JsonObject.text(key: String): String = (get(key) as? JsonPrimitive)?.contentOrNull.orEmpty()
fun JsonObject.flag(key: String): Boolean = (get(key) as? JsonPrimitive)?.booleanOrNull == true
fun JsonObject.number(key: String): Long = (get(key) as? JsonPrimitive)?.doubleOrNull?.toLong() ?: 0L
fun JsonObject.rows(key: String): List<JsonObject> = (get(key) as? JsonArray)?.mapNotNull { it as? JsonObject }.orEmpty()
fun artifactSize(a: JsonObject): Long = a.number(if (a.text("state") == "frozen") "frozen_bytes" else "disk_bytes")

/** An investigation the owner is about to start from the New agent sheet (sm#2266). */
data class InvestigationStart(val request: li.rajeshgo.sm.data.model.CreateSessionRequest)

data class ArtifactsState(
    val data: JsonObject? = null,
    val error: String? = null,
    val busy: Boolean = false,
    val investigation: InvestigationStart? = null,
    val createError: String? = null,
)
class ArtifactsViewModel(application: Application) : AndroidViewModel(application) {
    private val settings = SettingsRepository(application)
    private val repository = SessionManagerRepository(settings)
    private val mutable = MutableStateFlow(ArtifactsState())
    val state: StateFlow<ArtifactsState> = mutable
    init { viewModelScope.launch { while (true) { load(); delay(60000) } } }
    private suspend fun load() {
        try {
            val url = settings.serverUrl.first()
            val token = settings.accessToken.first()
            if (url.isBlank() || token.isBlank()) { return }
            val data = repository.fetchArtifacts(url, token)
            mutable.value = mutable.value.copy(data = data, error = null)
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            mutable.value = mutable.value.copy(error = e.message)
        }
    }
    fun refresh() { viewModelScope.launch { load() } }

    /** Fetches the brief, then the screen opens the New agent sheet with it. */
    fun investigate(a: JsonObject) {
        if (mutable.value.busy) return
        mutable.value = mutable.value.copy(busy = true, error = null)
        viewModelScope.launch {
            try {
                val brief = repository.artifactBrief(settings.serverUrl.first(), settings.accessToken.first(), a.text("name"), a.text("root")).getOrThrow()
                mutable.value = mutable.value.copy(investigation = InvestigationStart(li.rajeshgo.sm.data.model.CreateSessionRequest(
                    provider = brief.text("provider").ifBlank { "claude" },
                    workingDir = brief.text("working_dir"),
                    model = brief.text("model").ifBlank { null },
                    reasoningEffort = brief.text("effort").ifBlank { null },
                    name = brief.text("name").ifBlank { null },
                    initialMessage = brief.text("brief"),
                )), createError = null)
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                mutable.value = mutable.value.copy(error = e.message)
            } finally { mutable.value = mutable.value.copy(busy = false) }
        }
    }

    fun closeInvestigation() { mutable.value = mutable.value.copy(investigation = null, createError = null) }

    fun createSession(request: li.rajeshgo.sm.data.model.CreateSessionRequest) {
        if (mutable.value.busy) return
        mutable.value = mutable.value.copy(busy = true, createError = null)
        viewModelScope.launch {
            repository.createSession(settings.serverUrl.first(), settings.accessToken.first(), request)
                .onSuccess { mutable.value = mutable.value.copy(investigation = null); load() }
                .onFailure { mutable.value = mutable.value.copy(createError = it.message ?: "Could not create session") }
            mutable.value = mutable.value.copy(busy = false)
        }
    }

    suspend fun sessionAgentTypes(): List<li.rajeshgo.sm.ui.watch.AgentTypeChoice> =
        li.rajeshgo.sm.ui.watch.agentTypeChoices(repository.fetchOwnerSettings(settings.serverUrl.first(), settings.accessToken.first()))

    suspend fun sessionModels(provider: String, workingDir: String): li.rajeshgo.sm.data.model.SessionModelsResponse =
        repository.fetchSessionModelCatalog(settings.serverUrl.first(), settings.accessToken.first(), provider, workingDir)
    fun act(a: JsonObject, action: String) {
        if (mutable.value.busy) return
        mutable.value = mutable.value.copy(busy = true, error = null)
        viewModelScope.launch {
            try {
                repository.artifactOwnerAction(settings.serverUrl.first(), settings.accessToken.first(), a.text("name"), buildJsonObject {
                    put("root",a.text("root")); put("action",action)
                }).getOrThrow()
                load()
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                mutable.value = mutable.value.copy(error = e.message)
            } finally { mutable.value = mutable.value.copy(busy = false) }
        }
    }
}
