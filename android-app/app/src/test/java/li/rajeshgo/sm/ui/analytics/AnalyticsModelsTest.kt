package li.rajeshgo.sm.ui.analytics

import org.junit.Assert.assertEquals
import org.junit.Test

class AnalyticsModelsTest {
    @Test
    fun routeArgumentWinsOverStoredSection() {
        assertEquals(AnalyticsSection.QUEUE, resolveAnalyticsSection("queue", "time"))
    }

    @Test
    fun storedSectionOpensWithoutArgument() {
        assertEquals(AnalyticsSection.TIME, resolveAnalyticsSection(null, "time"))
    }

    @Test
    fun firstVisitAndUnknownValuesOpenSpend() {
        assertEquals(AnalyticsSection.SPEND, resolveAnalyticsSection(null, ""))
        assertEquals(AnalyticsSection.SPEND, resolveAnalyticsSection("detail", "bogus"))
    }
}
