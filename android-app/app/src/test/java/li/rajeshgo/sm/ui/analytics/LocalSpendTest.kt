package li.rajeshgo.sm.ui.analytics

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.SpendReport
import org.junit.Assert.*
import org.junit.Test

class LocalSpendTest {
    @Test fun localResponseKeepsTokensAndHoursSeparateFromQuota() {
        val report = Json.decodeFromString<SpendReport>("""{
            "generated_at":"2026-10-08T12:00:00Z", "provider":"local", "range":"week",
            "start":"2026-10-05T00:00:00Z", "end":"2026-10-08T12:00:00Z",
            "total":{"tokens":150,"percent":0}, "root":{"id":"root","kind":"root","label":"Local"},
            "local":{"busy_hours":1.25,"days":[{"date":"2026-10-08","busy_hours":1.25}],
            "models":[{"model":"local/flash-next","turns":2,"percent":0,"tokens":{"input":100,"output":20,"cache_read":30}}]}
        }""")
        assertEquals("local", SpendState().received(report).provider)
        assertEquals(150L, report.total.tokens)
        assertEquals("1.25", localBusyHours(report.local!!.days.single().busyHours))
        assertEquals(30L, report.local.models.single().tokens.cacheRead)
        assertTrue(report.meters.isEmpty())
    }
}
