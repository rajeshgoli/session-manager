package li.rajeshgo.sm.ui.bug

import android.app.Activity
import android.app.Application
import android.content.Context
import android.content.ContextWrapper
import android.graphics.Bitmap
import android.os.Handler
import android.os.Looper
import android.view.PixelCopy
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.io.ByteArrayOutputStream
import java.util.Base64
import kotlin.coroutines.resume
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive
import li.rajeshgo.sm.BuildConfig
import li.rajeshgo.sm.data.model.BoardStartRequest
import li.rajeshgo.sm.data.model.BoardStarted
import li.rajeshgo.sm.data.model.BugReportFiled
import li.rajeshgo.sm.data.model.BugReportIssue
import li.rajeshgo.sm.data.model.BugReportOptions
import li.rajeshgo.sm.data.model.BugReportRequest
import li.rajeshgo.sm.data.model.BugReportStart
import li.rajeshgo.sm.data.model.Reviewer
import li.rajeshgo.sm.data.remote.PageDataRecorder
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.navigation.Routes

/** The screenshot cap the server takes (spec 1859 A1). */
const val SCREENSHOT_MAX_BYTES = 8 * 1024 * 1024

/** The screen a report is about: its nav label, its route and when it became visible. */
object BugScreen {
    @Volatile var route: String = Routes.WATCH
        private set
    @Volatile var since: Long = System.nanoTime()
        private set

    fun shown(route: String) {
        if (route == this.route) return
        this.route = route
        since = System.nanoTime()
    }
}

/** The nav label a route shows as, the `page` of a report. */
fun bugPageLabel(route: String): String = when (route.substringBefore('?')) {
    Routes.WATCH -> "Watch"
    Routes.INBOX -> "Inbox"
    Routes.BOARD -> "Board"
    Routes.QUEUE -> "Queue"
    Routes.USAGE -> "Usage"
    Routes.HISTORY -> "History"
    Routes.GUESTBOOK -> "Guestbook"
    Routes.ANALYTICS -> "Analytics"
    Routes.SETTINGS -> "Settings"
    else -> route.substringBefore('?').replaceFirstChar { it.uppercase() }.take(40)
}

/** "versionName (versionCode)". */
fun bugClientVersion(): String = "${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})"

/** The provider, model and effort Start opens on, from `/client/settings` as the web's `providerDefaults` reads it. */
data class BugAgentDefaults(val provider: String, val model: String?, val effort: String)

fun bugAgentDefaults(settings: JsonObject?): BugAgentDefaults {
    val newAgent = settings?.get("new_agent") as? JsonObject
    val provider = newAgent?.string("provider")?.takeIf { it == "codex-fork" } ?: "claude"
    val saved = newAgent?.get(if (provider == "claude") "claude" else "codex") as? JsonObject
    return BugAgentDefaults(provider, saved?.string("model")?.ifBlank { null }, saved?.string("effort")?.ifBlank { null } ?: "high")
}

private fun JsonObject.string(key: String): String? = runCatching { get(key)?.jsonPrimitive?.contentOrNull }.getOrNull()

/** What the sheet hands back on File, File and start, or Start. */
data class BugFiling(
    val text: String,
    val screenshot: Boolean,
    /** Present when "Start an agent" is on. */
    val start: BugAgentChoice?,
)

data class BugAgentChoice(val provider: String, val model: String?, val effort: String?, val reviewer: Reviewer?)

/** The request body for a filing (spec 1859 A1); the screenshot only while its switch is on. */
fun bugReportRequest(
    filing: BugFiling,
    png: ByteArray?,
    page: String,
    route: String,
    pageData: JsonObject,
    clientVersion: String,
): BugReportRequest = BugReportRequest(
    text = filing.text.trim(),
    clientVersion = clientVersion,
    page = page.take(40),
    route = route.take(500),
    pageData = pageData,
    screenshotPng = png?.takeIf { filing.screenshot }?.let { Base64.getEncoder().encodeToString(it) },
    start = filing.start?.let { BugReportStart(it.provider, it.model, it.effort, it.reviewer) },
)

/** The toast after filing: `Filed #1870 · started sm-1870-engineer`. */
fun bugFiledText(filed: BugReportFiled, started: BoardStarted? = filed.started): String =
    "Filed #${filed.issue.number}" +
        started?.let { " · started ${it.name}" }.orEmpty() +
        (if (!filed.onBoard || filed.boardNote != null) " · not on the board" else "")

data class BugToast(val text: String, val issueUrl: String, val started: BoardStarted?)

/** One press of the bug button: the capture, the sheet's server reads, and the filing. */
data class BugSheet(
    val page: String,
    val route: String,
    val pageData: JsonObject,
    val png: ByteArray?,
    val thumbnail: ImageBitmap?,
    val options: BugReportOptions? = null,
    val defaults: BugAgentDefaults? = null,
    /** Why the agent choices could not load. */
    val agentError: String? = null,
    val busy: Boolean = false,
    val error: String? = null,
    /** Filed, but the agent did not start: the sheet offers Start only. */
    val filed: BugReportFiled? = null,
) {
    val filedIssue: BugReportIssue? get() = filed?.issue
}

class BugReportViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)

    var sheet by mutableStateOf<BugSheet?>(null)
        private set
    private val drafts = application.getSharedPreferences("bug_report_draft", Context.MODE_PRIVATE)
    var draftText by mutableStateOf(drafts.getString("text", "").orEmpty())
        private set
    private var retainedSheet: BugSheet? = null

    fun saveDraft(text: String) {
        draftText = text
        drafts.edit().putString("text", text).apply()
    }

    fun clearDraft() {
        if (sheet?.busy == true) return
        saveDraft("")
        retainedSheet = null
        sheet = null
    }

    private var capturing = false
    private val toasts = Channel<BugToast>(Channel.BUFFERED)
    val toastFlow = toasts.receiveAsFlow()

    /** Captures the window, then opens the sheet; the reads for its agent section follow. */
    fun open(activity: Activity) {
        if (capturing || sheet != null) return
        retainedSheet?.let {
            sheet = it
            retainedSheet = null
            return
        }
        capturing = true
        val route = BugScreen.route
        val pageData = PageDataRecorder.pages.snapshot(BugScreen.since)
        viewModelScope.launch {
            val shot = runCatching { captureWindow(activity) }.getOrNull()
            capturing = false
            sheet = BugSheet(bugPageLabel(route), route, pageData, shot?.first, shot?.second?.asImageBitmap())
            val (url, token) = credentials() ?: return@launch
            val options = async { runCatching { repository.fetchBugReportOptions(url, token) } }
            val settings = async { runCatching { repository.fetchOwnerSettings(url, token) } }
            val optionsResult = options.await()
            val settingsResult = settings.await()
            val error = optionsResult.exceptionOrNull() ?: settingsResult.exceptionOrNull()
            if (error != null && handleAuth(error)) return@launch
            update {
                it.copy(
                    options = optionsResult.getOrNull(),
                    defaults = bugAgentDefaults(settingsResult.getOrNull()),
                    agentError = error?.let { e -> e.message ?: "Couldn't load agent choices" },
                )
            }
        }
    }

    fun close() {
        if (sheet?.busy != true) {
            retainedSheet = sheet
            sheet = null
        }
    }

    suspend fun sessionModels(provider: String, workingDir: String): List<String> {
        val (url, token) = credentials() ?: return emptyList()
        return repository.fetchSessionModels(url, token, provider, workingDir)
    }

    /** File, File and start, or — after a filed bug's start failed — Start alone (decision 9). */
    fun submit(filing: BugFiling) {
        val current = sheet ?: return
        if (current.busy) return
        update { it.copy(busy = true, error = null) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            val filed = current.filed
            if (filed != null) {
                val start = filing.start ?: return@launch update { it.copy(busy = false) }
                repository.startBoardTicket(
                    url, token,
                    BoardStartRequest(filed.issue.repo, filed.issue.number, start.provider, start.model, start.effort, reviewer = start.reviewer),
                ).onSuccess { started -> finish(filed, started) }
                    .onFailure { error -> fail(error, "Start failed") }
                return@launch
            }
            val request = bugReportRequest(filing, current.png, current.page, current.route, current.pageData, bugClientVersion())
            repository.fileBugReport(url, token, request)
                .onSuccess { result ->
                    // Already filed, even if starting its agent failed: do not offer
                    // this text as a new report after an app restart.
                    drafts.edit().remove("text").apply()
                    if (filing.start != null && result.started == null) {
                        // The bug is filed; the sheet stays open to retry the start only.
                        update { it.copy(busy = false, filed = result, error = result.startError ?: "The agent did not start") }
                    } else finish(result, result.started)
                }
                .onFailure { error -> fail(error, "Filing failed") }
        }
    }

    private suspend fun finish(filed: BugReportFiled, started: BoardStarted?) {
        sheet = null
        retainedSheet = null
        saveDraft("")
        toasts.send(BugToast(bugFiledText(filed, started), filed.issue.url, started))
    }

    private suspend fun fail(error: Throwable, fallback: String) {
        if (handleAuth(error)) return
        update { it.copy(busy = false, error = error.message ?: fallback) }
    }

    private fun update(change: (BugSheet) -> BugSheet) {
        if (sheet != null) sheet = sheet?.let(change)
        else retainedSheet = retainedSheet?.let(change)
    }

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            update { it.copy(busy = false, error = "Sign in to report a bug.") }
            return null
        }
        return serverUrl to token
    }

    private suspend fun handleAuth(error: Throwable): Boolean {
        if (error !is SessionManagerAuthException) return false
        settingsRepository.clearAuth()
        retainedSheet = null
        sheet = null
        return true
    }
}

/** The activity a composable's context belongs to. */
tailrec fun Context.findActivity(): Activity? = when (this) {
    is Activity -> this
    is ContextWrapper -> baseContext.findActivity()
    else -> null
}

/**
 * The activity window as a PNG and the bitmap it came from (spec 1859 C2).
 * Over [SCREENSHOT_MAX_BYTES], half size is tried once; null when the copy
 * fails or the half-size PNG is still too large.
 */
suspend fun captureWindow(activity: Activity): Pair<ByteArray, Bitmap>? {
    val window = activity.window
    val view = window.decorView
    if (view.width <= 0 || view.height <= 0) return null
    val bitmap = Bitmap.createBitmap(view.width, view.height, Bitmap.Config.ARGB_8888)
    val copied = suspendCancellableCoroutine { continuation ->
        PixelCopy.request(window, bitmap, { result -> continuation.resume(result == PixelCopy.SUCCESS) }, Handler(Looper.getMainLooper()))
    }
    if (!copied) return null
    return withContext(Dispatchers.Default) {
        val full = png(bitmap)
        if (full.size <= SCREENSHOT_MAX_BYTES) return@withContext full to bitmap
        val half = Bitmap.createScaledBitmap(bitmap, bitmap.width / 2, bitmap.height / 2, true)
        png(half).takeIf { it.size <= SCREENSHOT_MAX_BYTES }?.let { it to half }
    }
}

private fun png(bitmap: Bitmap): ByteArray =
    ByteArrayOutputStream().use { out -> bitmap.compress(Bitmap.CompressFormat.PNG, 100, out); out.toByteArray() }
