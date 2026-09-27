package li.rajeshgo.sm.push

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class NoticeMessageTest {
    private fun data(
        sessionId: String = "eng00001",
        unread: String = "1",
        kind: String = "message",
    ) = mapOf(
        "kind" to kind,
        "notice_id" to "not_abcdefghijkl",
        "session_id" to sessionId,
        "title" to "sm-1679-engineer needs you",
        "body" to "Keep the old fills table or drop it?",
        "reader_path" to "/messages/msg_3f9a2c1d",
        "blocking" to "1",
        "unread_count" to unread,
    )

    @Test
    fun parsesTheSpecPayload() {
        val notice = NoticeMessage.fromData(data())!!
        assertEquals("message", notice.kind)
        assertEquals("not_abcdefghijkl", notice.noticeId)
        assertEquals("eng00001", notice.sessionId)
        assertEquals("sm-1679-engineer needs you", notice.title)
        assertEquals("/messages/msg_3f9a2c1d", notice.readerPath)
        assertTrue(notice.blocking)
        assertEquals(1, notice.unreadCount)
        val review = NoticeMessage.fromData(data(kind = "review_requested") + ("blocking" to "0") + ("unread_count" to "0"))!!
        assertFalse(review.blocking)
        assertEquals("Keep the old fills table or drop it?", review.displayBody)
    }

    @Test
    fun oneNotificationPerAgent() {
        val first = NoticeMessage.fromData(data())!!
        val second = NoticeMessage.fromData(data() + ("notice_id" to "not_zzzzzzzzzzzz") + ("body" to "Another"))!!
        assertEquals(first.notificationId, second.notificationId)
        assertEquals("agent:eng00001".hashCode(), first.notificationId)
        assertNotEquals(first.notificationId, NoticeMessage.fromData(data(sessionId = "eng00002"))!!.notificationId)
    }

    @Test
    fun bodyCountsTheOtherUnreadMessages() {
        assertEquals("Keep the old fills table or drop it?", NoticeMessage.fromData(data(unread = "1"))!!.displayBody)
        assertEquals("Keep the old fills table or drop it? (+2 more)", NoticeMessage.fromData(data(unread = "3"))!!.displayBody)
        assertEquals("Keep the old fills table or drop it?", NoticeMessage.fromData(data(unread = "junk"))!!.displayBody)
    }

    @Test
    fun followPushesAndIncompletePayloadsAreNotNotices() {
        assertNull(NoticeMessage.fromData(data(kind = "task_complete")))
        assertNull(NoticeMessage.fromData(data() - "notice_id"))
        assertNull(NoticeMessage.fromData(data() - "reader_path"))
        assertNull(NoticeMessage.fromData(data() + ("title" to " ")))
    }
}
