package li.rajeshgo.sm.ui.history

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.AgentHistoryResponse
import li.rajeshgo.sm.data.model.AgentHistoryRow
import li.rajeshgo.sm.data.model.AgentWork
import li.rajeshgo.sm.data.model.AgentWorkDoc
import li.rajeshgo.sm.data.model.AgentWorkItem
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test

class HistoryModelsTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test fun parsesTheServerPage() {
        val page = json.decodeFromString(
            AgentHistoryResponse.serializer(),
            """{"schema_version":1,"next_before":"MTc|YQ","total":2293,"agents":[
               {"id":"9255b4f7","name":"sm-1661-engineer","provider":"codex","model":null,"role":null,
                "working_dir":"/Users/r/worktrees/sm-1661","node":"studio","parent_session_id":"p1",
                "state":"retired","ended_at":"2026-09-29T20:00:00Z","last_status":"PR open",
                "restorable":false,"unrestorable_reason":"Runs on node studio",
                "work":{"tickets":[{"repo":"rajeshgoli/session-manager","number":1661,"title":"History",
                  "state":"open","url":"https://github.com/x","history_path":"/t/session-manager/1661"}],
                  "prs":[],"docs":[{"id":"d","name":"session-manager/docs/m.html","title":"Memo",
                  "state":"reviewed","reader_path":"/docs/session-manager/docs/m.html?version=abc",
                  "owner_reviews":1}]}}]}""",
        )
        assertEquals("MTc|YQ", page.nextBefore)
        assertEquals(2293, page.total)
        val row = page.agents.single()
        assertEquals("retired", row.state)
        assertFalse(row.restorable)
        assertEquals("Runs on node studio", row.unrestorableReason)
        assertEquals("/t/session-manager/1661", row.work.tickets.single().historyPath)
        assertEquals("/docs/session-manager/docs/m.html?version=abc", row.work.docs.single().readerPath)
        assertEquals("codex", providerLabel(row))
    }

    @Test fun nextPageAppendsWithoutRepeatingAnAgent() {
        val a = AgentHistoryRow(id = "a")
        val b = AgentHistoryRow(id = "b")
        val c = AgentHistoryRow(id = "c")
        assertEquals(listOf("a", "b", "c"), appendHistoryPage(listOf(a, b), listOf(b, c)).map { it.id })
    }

    @Test fun workSummaryCountsEachKind() {
        val item = AgentWorkItem(number = 1)
        assertNull(workSummary(AgentWork()))
        assertEquals("1 ticket", workSummary(AgentWork(tickets = listOf(item))))
        assertEquals(
            "2 tickets · 1 PR · 2 docs",
            workSummary(AgentWork(tickets = listOf(item, item), prs = listOf(item), docs = listOf(AgentWorkDoc(), AgentWorkDoc()))),
        )
    }

    @Test fun workingDirLabelIsTheLastSegment() {
        assertEquals("sm-1661", workingDirLabel("/Users/r/worktrees/sm-1661/"))
        assertEquals("session-manager", workingDirLabel("rajeshgoli/session-manager"))
        assertEquals("/", workingDirLabel("/"))
    }
}
