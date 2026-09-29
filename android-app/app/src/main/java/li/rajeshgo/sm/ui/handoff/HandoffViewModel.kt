package li.rajeshgo.sm.ui.handoff

import androidx.lifecycle.ViewModel
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import li.rajeshgo.sm.data.model.HandoffDefaults
import li.rajeshgo.sm.data.model.HandoffPolicy

data class HandoffUiState(
    val policy: HandoffPolicy? = null,
    val defaults: HandoffDefaults? = null,
    val loading: Boolean = false,
    val saving: Boolean = false,
    val error: String? = null,
)

/** The editor's scope belongs to its composition, so closing it cancels reads.
 * Read/write functions capture one connection; changing accounts creates a new editor.
 */
class HandoffViewModel(
    private val scope: CoroutineScope,
    private val read: suspend () -> HandoffUiState,
    private val write: suspend (JsonObject) -> HandoffUiState,
) : ViewModel() {
    private val mutableState = MutableStateFlow(HandoffUiState())
    val state = mutableState.asStateFlow()
    private var readJob: Job? = null
    private var generation = 0
    private var writeError: String? = null

    fun refresh() {
        if (state.value.saving || readJob?.isActive == true) return
        val started = generation
        mutableState.value = state.value.copy(loading = true)
        readJob = scope.launch {
            try {
                val loaded = read()
                if (started == generation) {
                    // Keep a failed write visible until the user retries it.
                    mutableState.value = loaded.copy(error = writeError)
                }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                if (started == generation) mutableState.value = state.value.copy(
                    loading = false, error = error.message ?: "Couldn't load handoff settings. Try again.",
                )
            }
        }
    }

    fun update(patch: JsonObject) {
        if (state.value.saving || (state.value.policy == null && state.value.defaults == null)) return
        generation++
        writeError = null
        readJob?.cancel()
        mutableState.value = state.value.copy(loading = false, saving = true, error = null)
        scope.launch {
            try {
                mutableState.value = write(patch)
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                writeError = error.message ?: "Couldn't save handoff settings. Try again."
                mutableState.value = state.value.copy(
                    saving = false, error = writeError,
                )
            }
        }
    }
}
