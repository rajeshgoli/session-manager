package li.rajeshgo.sm.ui.overnight

import android.app.NotificationManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import li.rajeshgo.sm.data.model.OvernightStatus
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

/**
 * Overnight mode (sm#2494), one copy for the whole app: every screen's moon
 * button and banner read [status]. It also reports the phone's Do Not
 * Disturb, which sm uses to tell agents the owner is away.
 */
object OvernightController {
    private val _status = MutableStateFlow<OvernightStatus?>(null)
    val status: StateFlow<OvernightStatus?> = _status

    /** A toggle in flight; the button shows it at once and settles on the reply. */
    private val _pending = MutableStateFlow<Boolean?>(null)
    val pending: StateFlow<Boolean?> = _pending

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val dndLock = Mutex()
    private var lastReportedDnd: Boolean? = null
    private var receiverRegistered = false

    private suspend fun session(context: Context): Triple<SessionManagerRepository, String, String>? {
        val settings = SettingsRepository(context.applicationContext)
        val url = settings.serverUrl.first().trim()
        val token = settings.accessToken.first().trim()
        if (url.isBlank() || token.isBlank()) return null
        return Triple(SessionManagerRepository(settings), url, token)
    }

    suspend fun refresh(context: Context) {
        val (repository, url, token) = session(context) ?: return
        runCatching { repository.fetchOvernight(url, token) }.onSuccess { _status.value = it }
    }

    /** The moon button. Returns an error message, or null. */
    suspend fun set(context: Context, on: Boolean): String? {
        val (repository, url, token) = session(context) ?: return "Sign in first"
        _pending.value = on
        val result = repository.setOvernight(url, token, on)
        _pending.value = null
        return result.fold(
            onSuccess = { _status.value = it; null },
            onFailure = { it.message ?: "Couldn't reach sm" },
        )
    }

    /** Whether the phone is silencing alerts: Do Not Disturb, Bedtime or a Focus mode. */
    fun phoneDndOn(context: Context): Boolean {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return false
        val filter = manager.currentInterruptionFilter
        return filter != NotificationManager.INTERRUPTION_FILTER_ALL &&
            filter != NotificationManager.INTERRUPTION_FILTER_UNKNOWN
    }

    /** Tells sm the phone's Do Not Disturb when it changed, or always with [force]. */
    suspend fun reportDnd(context: Context, force: Boolean = false) {
        dndLock.withLock {
            val on = phoneDndOn(context)
            if (!force && on == lastReportedDnd) return
            val (repository, url, token) = session(context) ?: return
            repository.reportDnd(url, token, on).onSuccess {
                lastReportedDnd = on
                _status.value = it
            }
        }
    }

    fun reportDndAsync(context: Context, force: Boolean = false) {
        val app = context.applicationContext
        scope.launch { reportDnd(app, force) }
    }

    /** Hears Do Not Disturb change while the app's process is alive. */
    fun watchDnd(context: Context) {
        if (receiverRegistered) return
        receiverRegistered = true
        val app = context.applicationContext
        ContextCompat.registerReceiver(
            app,
            object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) {
                    reportDndAsync(context)
                }
            },
            IntentFilter(NotificationManager.ACTION_INTERRUPTION_FILTER_CHANGED),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
    }
}
