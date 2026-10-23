package li.rajeshgo.sm.ui.overnight

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.InboxRow
import li.rajeshgo.sm.data.model.OvernightStatus
import li.rajeshgo.sm.ui.inbox.InboxFilter
import li.rajeshgo.sm.ui.inbox.inboxSections
import li.rajeshgo.sm.ui.settings.clockLabel
import li.rajeshgo.sm.ui.watch.ATTENTION_SECTIONS
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Overnight mode (sm#2494): the banner's words and the Blocked on you tier. */
class OvernightUiTest {
    private fun status(json: String) = Json { ignoreUnknownKeys = true }.decodeFromString(OvernightStatus.serializer(), json)

    @Test
    fun theBannerSaysWhyWhatSmDoesAndWhatWaits() {
        val bedtime = status(
            """{"on":true,"away":{"source":"bedtime","since":"x","since_label":"11:20 PM","until":"y","until_label":"7:00 AM","holds_alerts":true},
               "waiting":{"blocked":["sm-2494"],"unread":3},"manual":false,"dnd":false,"settings":{}}""",
        )
        assertEquals("Past bedtime · since 11:20 PM", overnightHeadline(bedtime))
        assertEquals("Agents work on their own; alerts wait until 7:00 AM.", overnightDetail(bedtime))
        assertEquals("sm-2494 is blocked on you · 3 new messages", overnightWaitingLine(bedtime))

        val dnd = status("""{"on":true,"away":{"source":"dnd","since_label":"2:05 PM","holds_alerts":false},"waiting":{"blocked":[],"unread":0}}""")
        assertEquals("Do Not Disturb · since 2:05 PM", overnightHeadline(dnd))
        assertEquals("Agents work on their own; your phone silences alerts.", overnightDetail(dnd))
        assertNull(overnightWaitingLine(dnd))

        val two = status("""{"on":true,"away":{"source":"manual"},"waiting":{"blocked":["a","b"],"unread":1}}""")
        assertEquals("Overnight", overnightHeadline(two))
        assertEquals("2 agents blocked on you · 1 new message", overnightWaitingLine(two))
    }

    @Test
    fun clockLabelsAreTwelveHour() {
        assertEquals("11:00 PM", clockLabel("23:00"))
        assertEquals("12:30 AM", clockLabel("00:30"))
        assertEquals("7:05 AM", clockLabel("07:05"))
    }

    @Test
    fun blockedLeadsTheInboxAndWatch() {
        val rows = listOf(
            InboxRow(threadKey = "agent:a", kind = "agent", title = "Ask", url = "/a", group = "needs_you"),
            InboxRow(threadKey = "agent:b", kind = "agent", title = "Blocked: log in", url = "/b", group = "blocked"),
        )
        assertEquals(listOf("BLOCKED ON YOU · 1", "NEEDS YOU · 1"), inboxSections(InboxFilter.Open, rows).map { it.first })
        assertEquals("blocked", ATTENTION_SECTIONS.first().first)
    }
}
