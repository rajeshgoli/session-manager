package li.rajeshgo.sm.ui.notes

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.OwnerNote
import li.rajeshgo.sm.data.model.OwnerNoteHit
import li.rajeshgo.sm.data.model.OwnerNoteRevision
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

data class NotesState(
    val query: String = "",
    val hits: List<OwnerNoteHit> = emptyList(),
    val note: OwnerNote? = null,
    val draft: String = "",
    val revisions: List<OwnerNoteRevision> = emptyList(),
    val conflict: OwnerNote? = null,
    val status: String = "",
    val busy: Boolean = false,
)

class NotesViewModel(application: Application) : AndroidViewModel(application) {
    private val settings = SettingsRepository(application)
    private val repository = SessionManagerRepository(settings)
    private val _state = MutableStateFlow(NotesState())
    val state: StateFlow<NotesState> = _state
    private var searchJob: Job? = null
    private var saveJob: Job? = null
    private val saveMutex = Mutex()

    private suspend fun credentials() = settings.serverUrl.first() to settings.accessToken.first()

    init { refresh() }

    fun search(query: String) {
        _state.update { it.copy(query = query) }
        searchJob?.cancel()
        searchJob = viewModelScope.launch { delay(150); loadSearch() }
    }

    fun refresh() { viewModelScope.launch { loadSearch() } }

    private suspend fun loadSearch() {
        val (url, token) = credentials()
        if (url.isBlank() || token.isBlank()) return
        runCatching { repository.searchOwnerNotes(url, token, _state.value.query) }
            .onSuccess { hits -> _state.update { it.copy(hits = hits, status = if (it.status.startsWith("Couldn't")) "" else it.status) } }
            .onFailure { error -> _state.update { it.copy(status = "Couldn't search notes: ${error.message}") } }
    }

    fun open(id: String) {
        saveJob?.cancel()
        viewModelScope.launch {
            if (!flush()) return@launch
            val (url, token) = credentials()
            runCatching { repository.getOwnerNote(url, token, id) }
                .onSuccess { note -> _state.update { it.copy(note = note, draft = note.body, conflict = null, revisions = emptyList(), status = "") } }
                .onFailure { error -> _state.update { it.copy(status = "Couldn't open note: ${error.message}") } }
        }
    }

    fun new() {
        saveJob?.cancel()
        viewModelScope.launch {
            if (!flush()) return@launch
            val (url, token) = credentials()
            repository.createOwnerNote(url, token, "")
                .onSuccess { note -> _state.update { it.copy(note = note, draft = "", conflict = null, status = "New note") }; loadSearch() }
                .onFailure { error -> _state.update { it.copy(status = "Couldn't create note: ${error.message}") } }
        }
    }

    fun edit(body: String) {
        if (body.toByteArray().size > 2 * 1024 * 1024) { _state.update { it.copy(status = "Note is limited to 2 MB") }; return }
        _state.update { it.copy(draft = body, status = "Saving…") }
        saveJob?.cancel()
        saveJob = viewModelScope.launch { delay(800); flush() }
    }

    fun save() { saveJob?.cancel(); saveJob = viewModelScope.launch { flush() } }

    fun finish(onDone: () -> Unit) {
        saveJob?.cancel()
        viewModelScope.launch { if (flush()) onDone() }
    }

    private suspend fun flush(): Boolean = saveMutex.withLock {
        val current = _state.value
        val note = current.note ?: return@withLock true
        if (current.conflict != null) return@withLock false
        if (current.draft == note.body) return@withLock true
        val (url, token) = credentials()
        var completed = false
        repository.saveOwnerNote(url, token, note, current.draft)
            .onSuccess { response ->
                when {
                    response.isSuccessful -> response.body()?.let { saved ->
                        completed = true
                        _state.update { state -> if (state.note?.id == saved.id) state.copy(note = saved,
                            status = if (state.draft == saved.body) "Saved" else "Saving…") else state }
                        loadSearch()
                        if (_state.value.draft != saved.body) save()
                    }
                    response.code() == 409 -> {
                        val theirs = response.errorBody()?.string()?.let {
                            runCatching { Json { ignoreUnknownKeys = true }.decodeFromString<OwnerNote>(it) }.getOrNull()
                        }
                        _state.update { it.copy(conflict = theirs, status = "Changed on another device") }
                    }
                    else -> _state.update { it.copy(status = "Couldn't save note: HTTP ${response.code()}") }
                }
            }.onFailure { error -> _state.update { it.copy(status = "Couldn't save note: ${error.message}") } }
        completed
    }

    fun loadTheirs() { _state.update { it.conflict?.let { note -> it.copy(note = note, draft = note.body, conflict = null, status = "Loaded latest note") } ?: it } }
    fun keepMine() { _state.update { it.conflict?.let { note -> it.copy(note = note, conflict = null) } ?: it }; save() }

    fun history() {
        val note = _state.value.note ?: return
        viewModelScope.launch {
            val (url, token) = credentials()
            runCatching { repository.ownerNoteRevisions(url, token, note.id) }
                .onSuccess { revisions -> _state.update { it.copy(revisions = revisions) } }
                .onFailure { error -> _state.update { it.copy(status = "Couldn't load history: ${error.message}") } }
        }
    }

    fun withNoteText(id: String, action: (String) -> Unit) {
        viewModelScope.launch {
            val current = _state.value
            if (current.note?.id == id) { action(current.draft); return@launch }
            val (url, token) = credentials()
            runCatching { repository.getOwnerNote(url, token, id) }
                .onSuccess { action(it.body) }
                .onFailure { error -> _state.update { it.copy(status = "Couldn't open note: ${error.message}") } }
        }
    }

    fun restore(version: Long) {
        val note = _state.value.note ?: return
        viewModelScope.launch {
            val (url, token) = credentials()
            repository.restoreOwnerNote(url, token, note.id, version)
                .onSuccess { restored -> _state.update { it.copy(note = restored, draft = restored.body, revisions = emptyList(), status = "Restored version $version") }; loadSearch() }
                .onFailure { error -> _state.update { it.copy(status = "Couldn't restore note: ${error.message}") } }
        }
    }

    fun delete() {
        val note = _state.value.note ?: return
        viewModelScope.launch {
            val (url, token) = credentials()
            repository.deleteOwnerNote(url, token, note.id)
                .onSuccess { response -> if (response.isSuccessful) { _state.update { it.copy(note = null, draft = "", status = "Deleted note") }; loadSearch() } }
                .onFailure { error -> _state.update { it.copy(status = "Couldn't delete note: ${error.message}") } }
        }
    }

    fun fileIssue(repo: String, text: String, onDone: (String) -> Unit) {
        viewModelScope.launch {
            val (url, token) = credentials()
            val title = text.lineSequence().firstOrNull().orEmpty().trim().trimStart('#').trim().take(80)
            val body = text.substringAfter('\n', "").trim()
            repository.fileOwnerIssue(url, token, repo, title, body)
                .onSuccess { onDone("Filed #${it.number}: ${it.url}") }
                .onFailure { onDone(it.message ?: "Couldn't file ticket") }
        }
    }

    fun startAgent(text: String, provider: String, workingDir: String, model: String?, effort: String?, onDone: (String) -> Unit) {
        viewModelScope.launch {
            val (url, token) = credentials()
            repository.createSession(url, token, li.rajeshgo.sm.data.model.CreateSessionRequest(
                provider = provider, workingDir = workingDir, model = model, reasoningEffort = effort, initialMessage = text,
            )).onSuccess { onDone("Started ${it.id}") }.onFailure { onDone(it.message ?: "Couldn't start agent") }
        }
    }
}
