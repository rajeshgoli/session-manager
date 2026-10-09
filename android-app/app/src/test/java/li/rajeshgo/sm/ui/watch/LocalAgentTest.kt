package li.rajeshgo.sm.ui.watch

import li.rajeshgo.sm.data.model.SessionModelsResponse
import li.rajeshgo.sm.data.model.HandoffDefaults
import kotlinx.serialization.json.Json
import org.junit.Assert.*
import org.junit.Test

class LocalAgentTest {
    @Test fun catalogControlsLocalAdmission() {
        assertFalse(SessionModelsResponse().canStartLocal)
        val occupied = Json.decodeFromString<SessionModelsResponse>("""{"models":["loaded"],"available":false,"reason":"no local seat free (1/1 used by worker)"}""")
        assertFalse(occupied.canStartLocal)
        assertEquals("no local seat free (1/1 used by worker)", occupied.reason)
        assertTrue(occupied.copy(available = true, reason = null).canStartLocal)
    }
    @Test fun localBadgeAndThresholdsStayDistinctFromClaude() {
        assertEquals("LOCAL", providerTag("opencode"))
        assertEquals("CLAUDE", providerTag("claude"))
        val defaults = Json.decodeFromString<HandoffDefaults>("""{"providers":{"claude":true,"opencode":true},"threshold_percent":35,"ask_on_codex_review":true,"ask_on_doc_review":true,"review_floor_percent":20,"reminder_percent":50,"provider_thresholds":{"opencode":{"threshold_percent":75,"review_floor_percent":50,"reminder_percent":85}}}""")
        assertEquals(75.0, defaults.providerThresholds.getValue("opencode").thresholdPercent, 0.0)
        assertEquals(35.0, defaults.thresholdPercent, 0.0)
    }
}
