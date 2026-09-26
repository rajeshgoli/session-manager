package li.rajeshgo.sm.push

import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

/** Receives follow pushes. They are data-only, so this builds the notification whether the app is open or closed. */
class FollowMessagingService : FirebaseMessagingService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    override fun onNewToken(token: String) {
        scope.launch { FollowPush.registerToken(applicationContext, token) }
    }

    override fun onMessageReceived(message: RemoteMessage) {
        val followMessage = FollowMessage.fromData(message.data) ?: return
        val shown = FollowPush.show(applicationContext, followMessage)
        val followId = followMessage.followId
        // The ack tells sm the phone showed it, so no fallback email is sent;
        // a notification Android suppressed is never acknowledged.
        if (!shown || followMessage.isTest || followId == null) return
        scope.launch {
            val settings = SettingsRepository(applicationContext)
            val serverUrl = settings.serverUrl.first().trim()
            val accessToken = settings.accessToken.first().trim()
            if (serverUrl.isNotBlank() && accessToken.isNotBlank()) {
                SessionManagerRepository(settings).ackFollow(serverUrl, accessToken, followId)
            }
        }
    }
}
