package li.rajeshgo.sm.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import com.google.firebase.FirebaseApp
import com.google.firebase.FirebaseOptions
import com.google.firebase.messaging.FirebaseMessaging
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.suspendCancellableCoroutine
import li.rajeshgo.sm.BuildConfig
import li.rajeshgo.sm.MainActivity
import li.rajeshgo.sm.R
import li.rajeshgo.sm.data.model.PushTokenRequest
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.security.DeviceKeyManager
import kotlin.coroutines.resume

/** Follow notifications (sm#1569): Firebase setup, token registration, and the notification itself. */
object FollowPush {
    const val CHANNEL_ID = "agent_follow"
    const val ACTION_OPEN_FOLLOW = "li.rajeshgo.sm.OPEN_FOLLOW"
    const val EXTRA_FOLLOW_ID = "follow_id"
    const val EXTRA_SESSION_ID = "session_id"
    const val EXTRA_READER_PATH = "reader_path"
    const val EXTRA_TITLE = "title"

    /** Firebase options come from BuildConfig; any blank value leaves push off and follows fall back to email. */
    val isConfigured: Boolean
        get() = firebaseOptionValues().all { it.isNotBlank() }

    private fun firebaseOptionValues() = listOf(
        BuildConfig.SM_FIREBASE_PROJECT_ID,
        BuildConfig.SM_FIREBASE_SENDER_ID,
        BuildConfig.SM_FIREBASE_APP_ID,
        BuildConfig.SM_FIREBASE_API_KEY,
    )

    fun initialize(context: Context) {
        createChannel(context)
        if (!isConfigured || FirebaseApp.getApps(context).isNotEmpty()) return
        FirebaseApp.initializeApp(
            context,
            FirebaseOptions.Builder()
                .setProjectId(BuildConfig.SM_FIREBASE_PROJECT_ID)
                .setGcmSenderId(BuildConfig.SM_FIREBASE_SENDER_ID)
                .setApplicationId(BuildConfig.SM_FIREBASE_APP_ID)
                .setApiKey(BuildConfig.SM_FIREBASE_API_KEY)
                .build(),
        )
    }

    private fun createChannel(context: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Agents you follow", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "One notification when a followed agent or queue job finishes"
            },
        )
    }

    private suspend fun currentToken(): String? {
        if (!isConfigured) return null
        return suspendCancellableCoroutine { continuation ->
            FirebaseMessaging.getInstance().token.addOnCompleteListener { task ->
                continuation.resume(if (task.isSuccessful) task.result else null)
            }
        }
    }

    /** Sends this phone's push token to sm; a no-op when signed out or push is off. */
    suspend fun registerToken(context: Context, token: String? = null): Result<Unit> {
        val settings = SettingsRepository(context)
        val serverUrl = settings.serverUrl.first().trim()
        val accessToken = settings.accessToken.first().trim()
        if (serverUrl.isBlank() || accessToken.isBlank()) return Result.success(Unit)
        val pushToken = token ?: currentToken() ?: return Result.success(Unit)
        val deviceId = runCatching { DeviceKeyManager().deviceKeyId() }.getOrNull()
        return SessionManagerRepository(settings).registerPushToken(
            serverUrl,
            accessToken,
            PushTokenRequest(
                token = pushToken,
                deviceId = deviceId,
                deviceName = Build.MODEL.orEmpty().ifBlank { "Android" },
                appVersion = BuildConfig.SM_APK_HASH.ifBlank { BuildConfig.VERSION_NAME },
            ),
        )
    }

    /**
     * Stops pushes to this phone before signing out: removes the token from sm,
     * then deletes it at Firebase. The second step covers a failed first one:
     * Google then reports the token unregistered and sm stops using it.
     */
    suspend fun unregisterToken(context: Context, serverUrl: String, accessToken: String) {
        if (!isConfigured) return
        val pushToken = currentToken()
        if (pushToken != null && serverUrl.isNotBlank() && accessToken.isNotBlank()) {
            SessionManagerRepository(SettingsRepository(context)).deletePushToken(serverUrl, accessToken, pushToken)
        }
        suspendCancellableCoroutine { continuation ->
            FirebaseMessaging.getInstance().deleteToken().addOnCompleteListener { continuation.resume(Unit) }
        }
    }

    /**
     * Whether a notification posted now would be seen: the app may notify
     * and the follow channel is not muted. When it is off, the phone must not
     * acknowledge, so sm falls back to email.
     */
    fun canNotify(context: Context): Boolean {
        val manager = NotificationManagerCompat.from(context)
        if (!manager.areNotificationsEnabled()) return false
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = context.getSystemService(NotificationManager::class.java)?.getNotificationChannel(CHANNEL_ID)
            if (channel != null && channel.importance == NotificationManager.IMPORTANCE_NONE) return false
        }
        return true
    }

    /** Posts the notification; returns whether it was shown. */
    fun show(context: Context, message: FollowMessage): Boolean {
        if (!canNotify(context)) return false
        val intent = Intent(context, MainActivity::class.java).apply {
            action = ACTION_OPEN_FOLLOW
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
            putExtra(EXTRA_FOLLOW_ID, message.followId)
            putExtra(EXTRA_SESSION_ID, message.sessionId)
            putExtra(EXTRA_READER_PATH, message.readerPath)
            putExtra(EXTRA_TITLE, message.title)
        }
        val pendingIntent = PendingIntent.getActivity(
            context,
            message.notificationId,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.mipmap.ic_launcher)
            .setContentTitle(message.title)
            .setContentText(message.body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(message.body))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .setContentIntent(pendingIntent)
            .build()
        return runCatching { NotificationManagerCompat.from(context).notify(message.notificationId, notification) }.isSuccess
    }
}

/** A push's data payload (spec appendix F), parsed. */
data class FollowMessage(
    val kind: String,
    val followId: String?,
    val sessionId: String?,
    val title: String,
    val body: String,
    val readerPath: String?,
) {
    /** A duplicate delivery of the same follow replaces its notification instead of stacking. */
    val notificationId: Int get() = (followId ?: kind).hashCode()

    val isTest: Boolean get() = kind == "test"

    companion object {
        fun fromData(data: Map<String, String>): FollowMessage? {
            val title = data["title"]?.takeIf { it.isNotBlank() } ?: return null
            return FollowMessage(
                kind = data["kind"].orEmpty(),
                followId = data["follow_id"]?.takeIf { it.isNotBlank() },
                sessionId = data["session_id"]?.takeIf { it.isNotBlank() },
                title = title,
                body = data["body"].orEmpty(),
                readerPath = data["reader_path"]?.takeIf { it.isNotBlank() },
            )
        }
    }
}

/**
 * A tapped follow notification waiting for the watch screen. Process-wide
 * Compose state, read directly by the screens: a value handed through the
 * NavHost builder is captured when the graph is built, so a tap while the
 * app is already running would never reach the watch screen.
 */
object FollowOpenRequests {
    var pending by mutableStateOf<FollowOpen?>(null)
}

/** What tapping a follow notification opens: the report, else the agent's card. */
data class FollowOpen(
    val sessionId: String?,
    val readerPath: String?,
    val title: String,
) {
    companion object {
        fun fromIntent(intent: Intent?): FollowOpen? {
            if (intent?.action != FollowPush.ACTION_OPEN_FOLLOW) return null
            val sessionId = intent.getStringExtra(FollowPush.EXTRA_SESSION_ID)?.takeIf { it.isNotBlank() }
            val readerPath = intent.getStringExtra(FollowPush.EXTRA_READER_PATH)?.takeIf { it.isNotBlank() }
            if (sessionId == null && readerPath == null) return null
            return FollowOpen(sessionId, readerPath, intent.getStringExtra(FollowPush.EXTRA_TITLE).orEmpty())
        }
    }
}
