package li.rajeshgo.sm.push

import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

/**
 * Receives follow, message and review-request pushes. They are data-only, so
 * this builds the notification whether the app is open or closed.
 */
class FollowMessagingService : FirebaseMessagingService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    override fun onNewToken(token: String) {
        scope.launch { FollowPush.registerToken(applicationContext, token) }
    }

    override fun onMessageReceived(message: RemoteMessage) {
        // Removing a notification needs no sign-in or setting: it only ever hides.
        if (message.data["kind"] == NoticePush.KIND_WITHDRAW) {
            message.data["notice_id"]?.takeIf { it.isNotBlank() }?.let { NoticePush.withdraw(applicationContext, it) }
            return
        }
        // Firebase calls this on a background thread; the settings read is local.
        val settings = SettingsRepository(applicationContext)
        val (serverUrl, accessToken, enabled) = runBlocking {
            Triple(settings.serverUrl.first().trim(), settings.accessToken.first().trim(), settings.followPushEnabled.first())
        }
        // A signed-out phone shows nothing, even if its token outlived sign-out.
        if (serverUrl.isBlank() || accessToken.isBlank()) return
        // Turned off in Settings: nothing is shown or acknowledged, so sm emails
        // instead, even when unregistering the token failed (offline, say).
        if (!enabled) return
        if (message.data["kind"] == NoticePush.KIND_GITHUB_CODEX_PAUSED) {
            NoticePush.showGithubCodexPaused(applicationContext, message.data)
            return
        }
        if (message.data["kind"] in NoticePush.KINDS) {
            val notice = NoticeMessage.fromData(message.data) ?: return
            // Acknowledged only when actually shown, as follows are (sm#1580).
            if (!NoticePush.show(applicationContext, notice)) return
            scope.launch {
                SessionManagerRepository(settings).ackNotice(serverUrl, accessToken, notice.noticeId)
            }
            return
        }
        val followMessage = FollowMessage.fromData(message.data) ?: return
        val shown = FollowPush.show(applicationContext, followMessage)
        val followId = followMessage.followId
        // The ack tells sm the phone showed it, so no fallback email is sent;
        // a notification Android suppressed is never acknowledged.
        if (!shown || followMessage.isTest || followId == null) return
        scope.launch {
            SessionManagerRepository(settings).ackFollow(serverUrl, accessToken, followId)
        }
    }
}
