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
    @Test fun backupCardReportsTheRemoteDisksSelfUpdate() {
        fun backup(update: String) = Json.parseToJsonElement("""{"enabled":true,"update":$update}""").jsonObject
        assertNull(backupUpdate(Json.parseToJsonElement("""{"enabled":true}""").jsonObject))
        assertNull(backupUpdate(backup("null")))
        assertEquals("Remote disk: updated to sm 0.2.0 (abc) on 2026-10-18",
            backupUpdate(backup("""{"to":"sm 0.2.0 (abc)","ok":true,"at":"2026-10-18T09:31:00Z"}""")))
        assertEquals("Remote disk: update to sm 0.2.0 (abc) failed on 2026-10-18: downloaded sm failed SHA-256 check",
            backupUpdate(backup("""{"to":"sm 0.2.0 (abc)","ok":false,"error":"downloaded sm failed SHA-256 check","at":"2026-10-18T09:31:00Z"}""")))
    }

    @Test fun backupCardShowsOnlyWhatItHasToReportAndRaisesOneLineWhenNeeded() {
        fun card(json: String) = backupCard(Json.parseToJsonElement(json).jsonObject)
        assertNull(card("""{"enabled":false}"""))
        val rates = """"fetch_rate":18750000.0,"apply_rate":100000000.0,"probe_rate":25000000.0,"effective_rate":18750000.0"""
        val fresh = card("""{"enabled":true,$rates,"card":{"nights":[{"date":"2026-10-17","ok":null,"error":"not yet"}],"last_night":null,"weekly_check":null,"disk":null,
            "measured":{"fetch":false,"apply":false,"probe":false}}}""")!!
        assertTrue(fresh.empty)
        assertTrue(fresh.nights.isEmpty())
        assertNull(fresh.rates)
        assertNull(fresh.attention)
        val old = card("""{"enabled":true,$rates,"card":{"nights":[{"date":"2026-10-16","ok":true},{"date":"2026-10-17","ok":false,"error":"backup disk full: needs 5 bytes"}],
            "last_night":{"bytes":2000000000,"seconds":300,"confirmed_at":"2026-10-16T09:35:00Z"},"weekly_check":{"date":"2026-10-12","ok":true,"files_checked":120,"bad":0},"disk":null,
            "measured":{"fetch":true,"apply":false,"probe":true}}}""")!!
        assertNull(old.disk)
        assertEquals(listOf("✓", "✗"), old.nights.map { it.mark })
        assertEquals("Last night's backup failed: backup disk full: needs 5 bytes", old.attention)
        assertTrue(old.lastNight!!.startsWith("Last backup: 2.0 GB in 5 min, confirmed "))
        assertEquals("Weekly check 2026-10-12: 120 files, all ok", old.check)
        assertEquals("Download 150 Mbit/s · Restore 200 Mbit/s · in force 150 Mbit/s", old.rates)
        val failing = card("""{"enabled":true,$rates,"card":{"nights":[{"date":"2026-10-17","ok":true}],"weekly_check":{"date":"2026-10-12","ok":false,"files_checked":120,"bad":2},
            "disk":{"total_bytes":4000000000000,"free_bytes":1000000000000,"holding_bytes":30000000000,"holding_oldest":"2026-09-17","health":"failing","pruned_on":"2026-10-01"}}}""")!!
        assertEquals("Backup drive is failing. Replace it, then reseed with sm backup seed.", failing.attention)
        val disk = failing.disk!!
        assertEquals("3.0 TB used · 1.0 TB free of 4.0 TB", disk.line)
        assertEquals("Holding 30 GB · oldest 2026-09-17", disk.holding)
        assertEquals("Holding shortened for space on 2026-10-01", disk.pruned)
        assertEquals("Failing", disk.health)
        assertEquals("Weekly check 2026-10-12: 120 files, 2 bad", failing.check)
        assertNull(failing.lastNight)
    }
}
