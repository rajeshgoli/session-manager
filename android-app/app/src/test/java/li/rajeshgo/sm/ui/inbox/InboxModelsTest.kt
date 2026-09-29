package li.rajeshgo.sm.ui.inbox

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.InboxResponse
import li.rajeshgo.sm.data.model.InboxRow
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class InboxModelsTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test fun parsesTheServerRow() {
        val response = json.decodeFromString(
            InboxResponse.serializer(),
            """{"filter":"open","needs_you_count":2,"has_new":true,"rows":[
               {"thread_key":"doc:abc12345","kind":"doc","title":"Queue memo","repo":"widgets",
                "status":"review_requested","verdict":null,"pr_number":12,"author":"eng-agent",
                "group":"needs_you","preview":"Review requested · revision 1",
                "newest_at":"2026-09-28T10:02:00Z","message_count":0,"revision_count":1,
                "open_asks":1,"url":"/docs/widgets/docs/memo.md?version=aaaaaaaaaaaa",
                "done":false,"session_id":null,"doc_id":"abc12345"}]}""",
        )
        assertEquals(2, response.needsYouCount)
        assertTrue(response.hasNew)
        val row = response.rows.single()
        assertEquals(12L, row.prNumber)
        assertEquals("widgets · PR #12 · eng-agent", inboxRowDetail(row))
        assertEquals("widgets · PR #12", inboxReaderPage(row).subtitle)
        assertNull(row.sessionId)
    }

    @Test fun openGroupsInOrderAndOtherFiltersAreOneList() {
        val rows = listOf(
            InboxRow(threadKey = "a", group = "earlier"),
            InboxRow(threadKey = "b", group = "needs_you"),
            InboxRow(threadKey = "c", group = "new"),
            InboxRow(threadKey = "d", group = "needs_you"),
        )
        val sections = inboxSections(InboxFilter.Open, rows)
        assertEquals(listOf("NEEDS YOU · 2", "NEW · 1", "EARLIER · 1"), sections.map { it.first })
        assertEquals(listOf("b", "d"), sections[0].second.map { it.threadKey })
        assertEquals(listOf<String?>(null), inboxSections(InboxFilter.Docs, rows).map { it.first })
        assertTrue(inboxSections(InboxFilter.Done, emptyList()).isEmpty())
    }

    @Test fun agentRowsSayWhatTheyAre() {
        val base = InboxRow(kind = "agent", repo = "trading-core", messageCount = 3, status = "live")
        assertEquals("trading-core · 3 messages · asks you", inboxRowDetail(base.copy(group = "needs_you")))
        assertEquals("trading-core · 1 message · agent ended", inboxRowDetail(base.copy(messageCount = 1, status = "ended", group = "new")))
        assertEquals("trading-core · 3 messages · you replied", inboxRowDetail(base.copy(group = "earlier", preview = "You: Ship it")))
        assertEquals("trading-core · for your information", inboxRowDetail(base.copy(messageCount = 0, group = "new")))
    }
}
