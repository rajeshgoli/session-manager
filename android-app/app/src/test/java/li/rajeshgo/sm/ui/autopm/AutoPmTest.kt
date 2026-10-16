package li.rajeshgo.sm.ui.autopm

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.AutopmBudget
import li.rajeshgo.sm.data.model.AutopmChange
import li.rajeshgo.sm.data.model.AutopmView
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.time.Instant
import java.time.ZoneId

/** Spec 2170 F.3 (sm#2176): the What changed card's text and Settings › Auto PM's writes. */
class AutoPmTest {
    private val utc = ZoneId.of("UTC")
    private val oct21 = Instant.parse("2026-10-21T12:00:00Z").toEpochMilli()
    private val nov4 = Instant.parse("2026-11-04T12:00:00Z").toEpochMilli()
    private fun change(state: String, note: String? = null) =
        AutopmChange(id = "x", title = "Decision card", state = state, liveAtMs = oct21, measureAtMs = nov4, endedAtMs = oct21, decisionNote = note)

    @Test
    fun liveChangesAreCountedByState() {
        assertEquals("None live", liveSummary(emptyList()))
        assertEquals("2 live · 1 measuring", liveSummary(listOf(change("live"), change("measuring"), change("live"))))
    }

    @Test
    fun eachChangeSaysWhenItWentLiveAndWhenSmMeasuresIt() {
        assertEquals("Live since Oct 21 · measured Nov 4", changeMeta(change("live"), oct21, utc))
        assertEquals("Live since Oct 21 · measuring now", changeMeta(change("measuring"), nov4 + 1, utc))
        assertEquals("Reverted Oct 21 · missed its bar", changeMeta(change("reverted", "missed its bar"), oct21, utc))
        assertEquals("Rolling back", changeMeta(change("rolling_back"), oct21, utc))
    }

    @Test
    fun skipWritesNowAndUndoWritesNull() {
        val now = Instant.parse("2026-10-15T10:00:00Z")
        assertEquals("2026-10-15T10:00:00Z", skipValue(false, now))
        assertNull(skipValue(true, now))
        assertEquals("7 of 20", budgetText(AutopmBudget(allowance = 20.0, spent = 7.0)))
        assertEquals("2.5 of 20", budgetText(AutopmBudget(allowance = 20.0, spent = 2.46)))
        assertEquals("Skipped this week", budgetText(AutopmBudget(skipped = true)))
        assertEquals("No meter data", budgetText(null))
    }

    @Test
    fun theServerViewParsesWithNullBudgetsAndVotes() {
        val view = Json { ignoreUnknownKeys = true }.decodeFromString(
            AutopmView.serializer(),
            """{"enabled":true,"paused_at":null,"skipped":false,"budget":{"claude":{"allowance":5,"spent":1.5,"resets_at":"2026-10-18T00:00:00Z","skipped":false},"codex":null},
               "changes":[{"id":"x-1","title":"T","line":"h","state":"live","switch_key":"autopm.t","live_at_ms":1,"measure_at_ms":2,"vote":"up","extra":1}],
               "ended":[],"waiting":[{"id":"x-2","title":"W","path":"shadow"}],"readout":null,"switches":{"autopm.t":true},"switches_version":3}""",
        )
        assertEquals("up", view.changes.single().vote)
        assertNull(view.budget!!["codex"])
        assertEquals("1.5 of 5", budgetText(view.budget!!["claude"]))
        assertEquals(true, view.switches["autopm.t"])
    }
}
