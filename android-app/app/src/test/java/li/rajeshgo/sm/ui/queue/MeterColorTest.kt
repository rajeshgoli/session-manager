package li.rajeshgo.sm.ui.queue

import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import org.junit.Assert.assertEquals
import org.junit.Test

/** One colour rule for the Queue bars and the Host status sheet (spec 1782 A2, J7). */
class MeterColorTest {
    @Test
    fun bandsAreGreenAmberRedWithACyanLowGpu() {
        assertEquals(Emerald, meterBandColor(0.59))
        assertEquals(Amber, meterBandColor(0.60))
        assertEquals(Amber, meterBandColor(0.85))
        assertEquals(Rose, meterBandColor(0.86))
        assertEquals(Cyan, meterBandColor(0.10, gpu = true))
        assertEquals(Rose, meterBandColor(0.90, gpu = true))
        assertEquals(TextMuted, meterBandColor(null))
    }
}
