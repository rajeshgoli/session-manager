package li.rajeshgo.sm.ui.settings

import org.junit.Assert.*
import org.junit.Test
import java.time.Instant

class ReviewWarningTest {
    @Test fun dismissalSurvivesRetriesButExpiresAtReset() {
        val now = Instant.parse("2026-10-01T00:00:00Z").toEpochMilli()
        val reset = "2026-10-07T00:00:00Z"
        val until = githubWarningDismissUntil(reset, now)
        assertFalse(showGithubWarning("paused", until, now + 7_200_000))
        assertTrue(showGithubWarning("paused", until, until))
        assertFalse(showGithubWarning("available", until, until))
        assertTrue(showGithubWarning("paused", 0, now))
    }

    @Test fun missingOrExpiredResetWaitsForRecoveryInsteadOfInventingAReset() {
        val now = Instant.parse("2026-10-01T00:00:00Z").toEpochMilli()
        for (reset in listOf(null, "invalid", "2026-09-01T00:00:00Z")) {
            assertEquals(Long.MAX_VALUE, githubWarningDismissUntil(reset, now))
            assertFalse(showGithubWarning("paused", githubWarningDismissUntil(reset, now), now))
        }
    }
}
