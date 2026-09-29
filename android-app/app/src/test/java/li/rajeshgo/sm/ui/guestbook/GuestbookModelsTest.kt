package li.rajeshgo.sm.ui.guestbook

import java.time.OffsetDateTime
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.GuestbookEntry
import li.rajeshgo.sm.data.model.GuestbookResponse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class GuestbookModelsTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test fun parsesTheServerPage() {
        val page = json.decodeFromString(
            GuestbookResponse.serializer(),
            """{"schema_version":1,"next_before":7,"page_url":"https://sm.example/guestbook","entries":[
               {"id":8,"session_id":"9255b4f7","session_name":"1774-engineer","provider":"claude",
                "model":"claude-opus-5-5","working_dir":"/w","repos":["rajeshgoli/widgets"],
                "claims":[{"repo":"rajeshgoli/widgets","number":12,"kind":"ticket","title":"Queue"},
                          {"repo":"rajeshgoli/widgets","number":13,"kind":"pr","title":"Fix"}],
                "signed_at":"2026-09-29T13:53:51.940413Z","text":"**Done.**"}]}""",
        )
        assertEquals(7L, page.nextBefore)
        val entry = page.entries.single()
        assertEquals("claude · claude-opus-5-5", guestbookModelLabel(entry))
        assertEquals("/t/widgets/12", guestbookClaimPath(entry.claims[0]))
        assertEquals("#12", guestbookClaimLabel(entry.claims[0]))
        assertEquals("PR #13", guestbookClaimLabel(entry.claims[1]))
    }

    @Test fun lastPageHasNoCursorAndModelIsOptional() {
        val page = json.decodeFromString(
            GuestbookResponse.serializer(),
            """{"entries":[{"id":1,"provider":"codex","model":null}],"next_before":null}""",
        )
        assertNull(page.nextBefore)
        assertEquals("codex", guestbookModelLabel(page.entries.single()))
    }

    @Test fun appendingAPageSkipsEntriesAlreadyListed() {
        val merged = appendGuestbookPage(
            listOf(GuestbookEntry(id = 9), GuestbookEntry(id = 8)),
            listOf(GuestbookEntry(id = 8), GuestbookEntry(id = 7)),
        )
        assertEquals(listOf(9L, 8L, 7L), merged.map { it.id })
    }

    @Test fun signedLabelShowsAgeAndFallsBackToRaw() {
        val now = OffsetDateTime.parse("2026-09-29T15:53:51Z")
        assertTrue(signedLabel("2026-09-29T13:53:51Z", now).endsWith("· 2h ago"))
        assertEquals("yesterday", signedLabel("yesterday", now))
    }
}
