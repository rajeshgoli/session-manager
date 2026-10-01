package li.rajeshgo.sm.ui.inbox

import li.rajeshgo.sm.data.model.InboxResponse
import li.rajeshgo.sm.data.model.InboxRow
import li.rajeshgo.sm.data.repository.ScreenCache
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Returning to the Inbox shows its last rows at once and refreshes behind them (spec 1782 J5). */
class InboxCacheTest {
    @After fun clearCache() = ScreenCache.clear()

    @Test
    fun aCachedFilterStartsWithItsRowsAndNoSpinner() {
        val row = kotlinx.serialization.json.Json { ignoreUnknownKeys = true }.decodeFromString(
            InboxRow.serializer(),
            """{"thread_key":"agent:a","kind":"agent","title":"sm-1","preview":"Merge?","group":"needs_you","newest_at":"2026-09-30T19:00:00Z"}""",
        )
        ScreenCache.inbox[InboxFilter.Open.query] = InboxResponse(rows = listOf(row))
        val cached = cachedInboxState(InboxFilter.Open)
        assertEquals(listOf(row), cached.rows)
        assertFalse(cached.loading)
        assertTrue(cached.revalidating)

        val fresh = cachedInboxState(InboxFilter.Docs)
        assertTrue(fresh.loading)
        assertFalse(fresh.revalidating)
        assertTrue(fresh.rows.isEmpty())
    }
}
