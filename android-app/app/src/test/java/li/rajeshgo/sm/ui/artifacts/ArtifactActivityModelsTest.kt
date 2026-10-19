package li.rajeshgo.sm.ui.artifacts

import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test

/** The app words freezes, thaws and moves like the web page: both read this fixture (sm#2325). */
class ArtifactActivityModelsTest {
    private val fixture = Json.parseToJsonElement(javaClass.classLoader!!.getResource("artifact-activity.json")!!.readText()).jsonObject
    private fun JsonObject.opt(key: String): String? = (get(key) as? JsonPrimitive)?.contentOrNull

    @Test fun operationsReadLikeTheWebPage() {
        fixture.rows("operations").forEach { c ->
            val op = c["op"]!!.jsonObject
            assertEquals(c.text("status"), opStatus(op))
            assertEquals(c.number("step").toInt(), opStep(op))
            assertEquals(c.text("name"), opName(op))
            assertEquals(c.text("sizes"), opSizes(op))
            assertEquals(c.text("actor"), actorLabel(op))
            assertEquals(c.opt("actor_id"), actorId(op))
            assertEquals((c["saved"] as? JsonPrimitive)?.longOrNull, opSaved(op))
        }
    }
    @Test fun savingsReadLikeTheWebPage() {
        fixture.rows("savings").forEach { c ->
            val s = c["savings"]!!.jsonObject
            assertEquals(c.text("line"), savingsLine(s))
            assertEquals(c.opt("ratio"), ratio(s.number("data_bytes"), s.number("frozen_bytes")))
        }
    }
    @Test fun foldersReadLikeTheWebPage() {
        fixture.rows("artifacts").forEach { c ->
            val a = c["artifact"]!!.jsonObject
            assertEquals(c.text("summary"), artifactSummary(a))
            assertEquals(c.opt("placement"), placementLabel(a))
            assertEquals(c.opt("thaw"), thawTime(a))
        }
    }
    @Test fun estimatedProgressStopsShortOfDone() {
        fun op(state: String, elapsed: Double?) = buildJsonObject { put("state", state); put("expected_seconds", 100.0); put("elapsed_seconds", elapsed) }
        assertEquals(0.5f, opProgress(op("running", 50.0)))
        assertEquals(0.97f, opProgress(op("running", 500.0)))
        assertNull(opProgress(op("waiting", null)))
        assertEquals("45 s", span(45.0))
        assertEquals("1 h 20 min", span(4800.0))
    }
}
