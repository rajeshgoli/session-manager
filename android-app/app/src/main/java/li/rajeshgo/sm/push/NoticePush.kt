package li.rajeshgo.sm.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import li.rajeshgo.sm.MainActivity
import li.rajeshgo.sm.R

/**
 * Message and review-request notifications (sm#1580): one per agent, updated
 * in place without buzzing again while it is showing. Tapping opens the
 * message or doc in the reader, as a follow tap opens its report.
 */
object NoticePush {
    const val CHANNEL_ID = "agent_messages"
    val KINDS = setOf("message", "review_requested")
    /** sm no longer needs the owner to see a shown notice (sm#1643). */
    const val KIND_WITHDRAW = "withdraw"
    /** The notice a notification shows, so a withdrawal removes only that one. */
    private const val EXTRA_NOTICE_ID = "li.rajeshgo.sm.notice_id"

    fun createChannel(context: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Messages from agents", NotificationManager.IMPORTANCE_HIGH).apply {
                description = "Messages agents send you, and their review requests"
            },
        )
    }

    /** As [FollowPush.canNotify], for this channel: a muted channel must not acknowledge, so sm emails instead. */
    fun canNotify(context: Context): Boolean {
        val manager = NotificationManagerCompat.from(context)
        if (!manager.areNotificationsEnabled()) return false
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = context.getSystemService(NotificationManager::class.java)?.getNotificationChannel(CHANNEL_ID)
            if (channel != null && channel.importance == NotificationManager.IMPORTANCE_NONE) return false
        }
        return true
    }

    /** Posts or updates the agent's notification; returns whether it was shown. */
    fun show(context: Context, message: NoticeMessage): Boolean {
        if (!canNotify(context)) return false
        val intent = Intent(context, MainActivity::class.java).apply {
            action = FollowPush.ACTION_OPEN_FOLLOW
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
            putExtra(FollowPush.EXTRA_SESSION_ID, message.sessionId)
            putExtra(FollowPush.EXTRA_READER_PATH, message.readerPath)
            putExtra(FollowPush.EXTRA_TITLE, message.body)
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
            .setContentText(message.displayBody)
            .setStyle(NotificationCompat.BigTextStyle().bigText(message.displayBody))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setOnlyAlertOnce(true)
            .setAutoCancel(true)
            .setContentIntent(pendingIntent)
            .addExtras(Bundle().apply { putString(EXTRA_NOTICE_ID, message.noticeId) })
            .build()
        return runCatching { NotificationManagerCompat.from(context).notify(message.notificationId, notification) }.isSuccess
    }

    /**
     * Removes the notification while it still shows [noticeId]. A newer notice
     * from the same agent reuses the slot, and stays.
     */
    fun withdraw(context: Context, noticeId: String) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        runCatching {
            manager.activeNotifications
                .filter { it.notification.extras.getString(EXTRA_NOTICE_ID) == noticeId }
                .forEach { manager.cancel(it.tag, it.id) }
        }
    }
}

/** A message or review-request push's data payload (spec appendix F3), parsed. */
data class NoticeMessage(
    val kind: String,
    val noticeId: String,
    val sessionId: String,
    val title: String,
    val body: String,
    val readerPath: String,
    val blocking: Boolean,
    val unreadCount: Int,
) {
    /** One notification per agent: a newer message replaces the last. */
    val notificationId: Int get() = ("agent:$sessionId").hashCode()

    /** The body, with how many more unread messages the agent has sent. */
    val displayBody: String get() = if (unreadCount > 1) "$body (+${unreadCount - 1} more)" else body

    companion object {
        fun fromData(data: Map<String, String>): NoticeMessage? {
            val kind = data["kind"]?.takeIf { it in NoticePush.KINDS } ?: return null
            val noticeId = data["notice_id"]?.takeIf { it.isNotBlank() } ?: return null
            val sessionId = data["session_id"]?.takeIf { it.isNotBlank() } ?: return null
            val title = data["title"]?.takeIf { it.isNotBlank() } ?: return null
            val readerPath = data["reader_path"]?.takeIf { it.isNotBlank() } ?: return null
            return NoticeMessage(
                kind = kind,
                noticeId = noticeId,
                sessionId = sessionId,
                title = title,
                body = data["body"].orEmpty(),
                readerPath = readerPath,
                blocking = data["blocking"] == "1",
                unreadCount = data["unread_count"]?.toIntOrNull() ?: 0,
            )
        }
    }
}
