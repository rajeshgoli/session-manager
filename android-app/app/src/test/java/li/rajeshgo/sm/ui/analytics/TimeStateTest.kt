package li.rajeshgo.sm.ui.analytics

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.*
import org.junit.Assert.*
import org.junit.Test

class TimeStateTest {
    private val agent = TimeNode("a:one", "agent", "Engineer", activeSeconds = 7860, parkedSeconds = 480, turns = 53,
        parts = mapOf("model" to 2640L, "review" to 2340L, "queue" to 1620L, "idle" to 840L, "tools" to 420L),
        tools = mapOf("git" to 120L, "approval" to 300L))
    private val sibling = TimeNode("a:two", "agent", "Reviewer", activeSeconds = 600)
    private val thread = TimeNode("t:repo#1620", "thread", "#1620", children = listOf(agent, sibling))
    private val otherThread = thread.copy(id = "t:repo#1621", label = "#1621")
    private val repo = TimeNode("r:repo", "repo", "repo", children = listOf(thread, otherThread))
    private val root = TimeNode("root", "root", "All", children = listOf(repo))
    private val legend = listOf(AnalyticsLegend("model", "Model working"), AnalyticsLegend("tools", "Tools"),
        AnalyticsLegend("queue", "Waiting on queue"), AnalyticsLegend("review", "Waiting on review"), AnalyticsLegend("idle", "Idle"))
    private val report = TimeReport("now", "7d", "a", "b", TimeTotal(1_918_800, 776_880, 257), legend,
        listOf(AnalyticsLegend("approval", "Approval checks"), AnalyticsLegend("git", "Git & GitHub")), root)

    @Test fun drillBackAndSiblingsPreserveThreadContext() {
        val state = TimeState(report = report).open(repo).open(otherThread).open(agent)
        assertEquals(listOf(repo.id, otherThread.id, agent.id), state.path)
        assertEquals(listOf(repo.id, otherThread.id, sibling.id), state.open(sibling).path)
        assertEquals(otherThread.id, state.back().current?.id)
        assertEquals(root.id, state.back().back().back().current?.id)
        assertEquals(TimeState(report = report), TimeState(report = report).open(agent))
    }

    @Test fun refreshPopsRemovedDescendants() {
        val state = TimeState(report = report).open(repo).open(thread).open(agent)
        assertEquals(state.path, state.received(report).path)
        val changed = report.copy(root = root.copy(children = listOf(repo.copy(children = listOf(otherThread)))))
        assertEquals(listOf(repo.id), state.received(changed).path)
    }

    @Test fun durationsMatchTheMemo() {
        assertEquals("533 h", timeDuration(1_918_800))
        assertEquals("216 h", timeDuration(776_880))
        assertEquals("2 h 11 m", timeDuration(7_860))
        assertEquals("44 m", timeDuration(2_640))
        assertEquals("1 h", timeDuration(3_590))
        assertEquals("45 s", timeDuration(45))
        assertEquals("34%", timeShare(2_640, 7_860))
    }

    @Test fun bucketsLargestFirstWithToolKindsUnderTools() {
        val rows = timeBucketRows(agent, report.partsLegend, report.toolLegend)
        assertEquals(listOf("model", "review", "queue", "idle", "tools", "approval", "git"), rows.map { it.key })
        assertEquals(listOf(false, false, false, false, false, true, true), rows.map { it.indented })
        assertEquals("Approval checks", rows[5].label)
    }

    @Test fun subtitlesCarryCountsStateAndParkedTime() {
        assertEquals("53 turns · 8 m parked", timeSubtitle(agent))
        assertEquals("2 agents", timeSubtitle(thread))
        assertEquals("2 threads", timeSubtitle(repo))
        assertEquals(TimeRange.WEEK, TimeRange.fromKey("obsolete"))
    }

    @Test fun wirePayloadDecodesWithNullsAndOmittedTurns() {
        val decoded = Json { ignoreUnknownKeys = true }.decodeFromString<TimeReport>("""
            {"end":"b","generated_at":"now","parts_legend":[{"key":"model","label":"Model working"}],"range":"24h","start":"a",
             "tool_legend":[{"key":"read","label":"Reading code"}],"total":{"active_seconds":291162,"agents":40,"parked_seconds":1200},
             "root":{"active_seconds":291162,"history_path":null,"id":"root","kind":"root","label":"All","parked_seconds":1200,
              "parts":{"model":291162},"session_id":null,"session_status":null,"state":null,"tools":{},
              "children":[{"active_seconds":30912,"children":[],"history_path":null,"id":"a:a8cf5f95","kind":"agent","label":"1775-engineer",
               "parked_seconds":73,"parts":{"model":30912},"session_id":"a8cf5f95","session_status":"stopped","state":null,"tools":{"read":148},"turns":35}]}}
        """)
        assertNull(decoded.root.turns)
        assertEquals(35L, decoded.root.children.single().turns)
        assertEquals(148L, decoded.root.children.single().tools["read"])
    }
}
