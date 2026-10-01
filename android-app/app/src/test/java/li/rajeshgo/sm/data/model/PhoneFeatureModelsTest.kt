package li.rajeshgo.sm.data.model

import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PhoneFeatureModelsTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test fun serverNotesAndConflictsUseTheSameNoteShape() {
        val current = """{"id":"n1","title":"Plan","body":"new body","version":7,"updated_at":"2026-10-01T00:00:00Z"}"""
        val note = json.decodeFromString(OwnerNote.serializer(), current)
        assertEquals(7L, note.version)
        assertEquals("new body", note.body)
        val write = json.encodeToString(OwnerNoteWrite.serializer(), OwnerNoteWrite("mine", ifVersion = note.version))
        assertTrue(write.contains("\"if_version\":7"))
    }

    @Test fun boardAutoStartReadsItsChipAndSendsTheServerFieldNames() {
        val ticket = json.decodeFromString(BoardTicket.serializer(), """{
            "repo":"rajeshgoli/session-manager","number":1841,"tier":"Mid",
            "auto_start":{"agent_type":"Mid","provider":"claude","model":"opus[1m]",
              "effort":"high","brief":"begin","state":"waiting","attempts":0,"last_error":null}
        }""")
        assertEquals("Mid", ticket.tier)
        assertEquals("opus[1m]", ticket.autoStart?.model)
        val choice = BoardAutoStartChoice(ticket.repo, ticket.number, "Mid", "claude", "opus[1m]", "high", "begin")
        val request = json.encodeToString(BoardAutoStartChoice.serializer(), choice)
        assertTrue(request.contains("\"agent_type\":\"Mid\""))
        assertTrue(request.contains("\"reasoning_effort\":\"high\""))
    }
}
