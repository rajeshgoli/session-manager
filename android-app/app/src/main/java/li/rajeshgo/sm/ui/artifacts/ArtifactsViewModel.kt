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

data class ArtifactsState(val data: JsonObject? = null, val error: String? = null, val busy: Boolean = false)
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
