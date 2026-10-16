package li.rajeshgo.sm.ui.artifacts

import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test

class ArtifactsModelsTest {
    private fun rows(value: String) = Json.parseToJsonElement(value).jsonArray.map { it.jsonObject }
    @Test fun overviewShowsOnlyLargestMeaningfulFoldersAndPreservesUnknowns() {
        val items = rows("""[
          {"name":"unknown","state":"unrecorded","needs_owner":true},
          {"name":"tiny","state":"live","disk_bytes":100000},
          {"name":"large","state":"live","disk_bytes":2000000000,"note":"Market research"},
          {"name":"compressed","state":"frozen","disk_bytes":9000000000,"frozen_bytes":1000000000},
          {"name":"deleted","state":"deleted","disk_bytes":99000000000}
        ]""")
        assertEquals(listOf("large","compressed"), largestArtifacts(items).map { it.text("name") })
        assertEquals("unknown", artifactInventory(items).last().text("name"))
        assertNull(measuredArtifactBytes(items[0]))
        assertEquals(listOf("large"), artifactInventory(items,"market").map { it.text("name") })
        assertEquals(5, largestArtifacts((1..10).map { buildJsonObject { put("name", "$it"); put("state","live"); put("disk_bytes",it * 1_000_000_000L) } }).size)
    }
    @Test fun bookkeepingDoesNotBecomeAnOwnerDecision() {
        val items = rows("""[
          {"needs_owner":true,"needs_you":{"reasons":["unrecorded","project_added"]}},
          {"needs_you":{"reasons":["would_delete"]}}
        ]""")
        assertFalse(artifactNeedsDecision(items[0]))
        assertTrue(artifactNeedsDecision(items[1]))
    }
    @Test fun quantitiesAreRoundedAndMissingIsNotZero() {
        assertEquals("Not measured",storageSize(null))
        assertEquals("1.7 TB",storageSize(1_734_000_000_000))
        assertEquals("<1 MB",storageSize(100_000))
        assertEquals("26 GB",storageSize(25_600_427_493))
    }
    @Test fun operationalProblemsRemainVisibleForAgentFollowup() {
        val items = listOf("operation_failed", "consumer_unverifiable", "vanished").map { reason ->
            buildJsonObject {
                put("name",reason); put("state",if (reason == "vanished") "deleted" else "live"); put("disk_bytes",2_000_000_000L)
                put("needs_you",buildJsonObject { put("reasons",buildJsonArray { add(reason) }) })
            }
        }
        assertTrue(items.all(::artifactNeedsAgent))
        assertFalse(items.any(::artifactNeedsDecision))
        assertEquals(3,artifactInventory(items).size)
        assertTrue(largestArtifacts(items).isEmpty())
        assertEquals(1,artifactInventory(items,"vanished").size)
    }
}
