package li.rajeshgo.sm.push

import li.rajeshgo.sm.ui.watch.DEFAULT_FOLLOW_MESSAGE
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class FollowMessageTest {
    @Test
    fun parsesAFollowPushAndKeysTheNotificationByFollow() {
        val data = mapOf(
            "kind" to "task_complete",
            "follow_id" to "fol_abc",
            "session_id" to "a8d3f4e9",
            "title" to "sm-1569-engineer finished",
            "body" to "Report: Follow — completion",
            "reader_path" to "/docs/session-manager/docs/x.html?version=abc",
        )
        val message = FollowMessage.fromData(data)!!
        assertEquals("fol_abc", message.followId)
        assertEquals("a8d3f4e9", message.sessionId)
        assertEquals("/docs/session-manager/docs/x.html?version=abc", message.readerPath)
        assertFalse(message.isTest)
        // A duplicate delivery replaces rather than stacks.
        assertEquals(message.notificationId, FollowMessage.fromData(data)!!.notificationId)
    }

    @Test
    fun jobAndTestPushesCarryNoReport() {
        val job = FollowMessage.fromData(
            mapOf("kind" to "job_finished", "follow_id" to "fol_j", "session_id" to "s1", "job_id" to "job_1",
                "title" to "1679-stage1-prepare-2 succeeded", "body" to "1679-engineer · ran 2h 14m"),
        )!!
        assertNull(job.readerPath)
        val test = FollowMessage.fromData(mapOf("kind" to "test", "title" to "sm notifications work", "body" to "Sent from mac"))!!
        assertTrue(test.isTest)
        assertNull(test.followId)
    }

    @Test
    fun ignoresPushesWithoutATitle() {
        assertNull(FollowMessage.fromData(mapOf("kind" to "task_complete", "body" to "x")))
    }

    @Test
    fun defaultMessageLeavesTheMarkerToTheServer() {
        assertFalse(DEFAULT_FOLLOW_MESSAGE.startsWith("[sm follow]"))
        assertTrue(DEFAULT_FOLLOW_MESSAGE.contains("sm doc publish"))
        assertTrue(DEFAULT_FOLLOW_MESSAGE.contains("sm task-complete"))
    }
}
