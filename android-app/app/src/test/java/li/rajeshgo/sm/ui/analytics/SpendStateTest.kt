package li.rajeshgo.sm.ui.analytics

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.*
import org.junit.Assert.*
import org.junit.Test
import java.time.OffsetDateTime

class SpendStateTest {
    private val agent = SpendNode("a:one", "agent", "Engineer", percent = 4.0)
    private val sibling = SpendNode("a:two", "agent", "Reviewer", percent = 2.0)
    private val thread = SpendNode("t:repo#1679", "thread", "#1679", children = listOf(agent, sibling))
    private val otherThread = thread.copy(id = "t:repo#1680", label = "#1680")
    private val repo = SpendNode("r:repo", "repo", "repo", children = listOf(thread, otherThread))
    private val gap = SpendNode("gap", "gap", "Not in the ledger", percent = 3.0)
    private val root = SpendNode("root", "root", "All", children = listOf(repo, gap))
    private val report = SpendReport("2026-09-29T19:00:00Z", "claude", "week", "start", "end", total = SpendTotal(35.0, 4_000_000_000), root = root)

    @Test fun drillBackAndSiblingsPreserveThreadContext() {
        val state = SpendState(report = report).open(repo).open(otherThread).open(agent)
        assertEquals(listOf(repo.id, otherThread.id, agent.id), state.path)
        assertEquals(listOf(repo.id, otherThread.id, sibling.id), state.open(sibling).path)
        assertEquals(otherThread.id, state.back().current?.id)
        assertEquals(repo.id, state.back().back().current?.id)
        assertEquals(root.id, state.back().back().back().current?.id)
    }

    @Test fun gapAndUnrelatedNodesCannotBeOpened() {
        val state = SpendState(report = report)
        assertEquals(state, state.open(gap))
        assertEquals(state, state.open(agent))
    }

    @Test fun refreshKeepsExistingPathAndPopsRemovedDescendants() {
        val state = SpendState(report = report).open(repo).open(thread).open(agent)
        assertEquals(state.path, state.received(report).path)
        val changed = report.copy(root = root.copy(children = listOf(repo.copy(children = listOf(otherThread)))))
        assertEquals(listOf(repo.id), state.received(changed).path)
        assertEquals(repo.id, state.received(changed).current?.id)
    }

    @Test fun wirePayloadAllowsNullMetadataAndMissingModelsAndLargeTokens() {
        val decoded = Json { ignoreUnknownKeys = true }.decodeFromString<SpendReport>("""
            {"generated_at":"now","provider":"codex","range":"4w","start":"a","end":"b",
             "rates_fitted_at":"2026-09-29","total":{"percent":140.0,"tokens":4000000000},
             "meters":[{"account_key":"account","label":null,"percent":21.0,"observed_at":"now","resets_at":"later","pace":null}],
             "root":{"id":"root","kind":"root","label":"All","state":null,"history_path":null,"session_id":null,"session_status":null,
             "children":[{"id":"a:owner","kind":"agent","label":"You","models":[{"model":"codex-cloud-review","effort":null,"turns":10,"percent":0.5,"tokens":{"input":0,"output":0,"cache_write":0,"cache_read":0}}]}]}}
        """)
        assertEquals(4_000_000_000, decoded.total.tokens)
        assertNull(decoded.root.models)
        assertNull(decoded.meters.single().label)
        assertEquals(10, decoded.root.children.single().models!!.single().turns)
    }

    @Test fun compositionUsesLegendOrderAndDoesNotDropUnknownModels() {
        assertEquals(listOf("opus" to 4.0, "fable" to 1.0, "new" to 2.0), orderedAnalyticsParts(
            linkedMapOf("new" to 2.0, "fable" to 1.0, "opus" to 4.0, "zero" to 0.0),
            listOf(AnalyticsLegend("opus", "Opus"), AnalyticsLegend("fable", "Fable")),
        ))
    }

    @Test fun modelLabelsKeepReadableVersions() {
        assertEquals("Fable 5.1", spendModelLabel("claude-fable-5-1"))
        assertEquals("Astra", spendModelLabel("gpt-6-astra"))
        assertEquals("Cloud reviews", spendModelLabel("codex-cloud-review"))
        assertEquals("future-model", spendModelLabel("future-model"))
    }

    @Test fun staleMeterIsVisibleAndUnknownRangeFallsBack() {
        val meter = SpendMeter("one", percent = 21.0, observedAt = "2026-09-29T18:00:00Z", resetsAt = "2026-10-03T16:00:00Z", pace = SpendPace("on_pace", percent = 65.0))
        val line = spendMeterLine(meter, OffsetDateTime.parse("2026-09-29T19:00:00Z"))
        assertTrue(line.contains("meter from 1h ago"))
        assertTrue(line.contains("on pace for 65.0%"))
        assertEquals(SpendRange.WEEK, SpendRange.fromKey("obsolete"))
        assertEquals("4.00B", spendTokens(4_000_000_000))
    }
}
