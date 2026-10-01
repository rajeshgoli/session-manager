package li.rajeshgo.sm.ui.reviews

import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.BoardStartOptions
import li.rajeshgo.sm.data.model.BoardTicketReview
import li.rajeshgo.sm.data.model.JobReview
import li.rajeshgo.sm.data.model.PutReviewPolicyRequest
import li.rajeshgo.sm.data.model.ReviewStatus
import li.rajeshgo.sm.data.model.Reviewer
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.data.model.StoredReviewPolicy
import li.rajeshgo.sm.ui.board.boardReviewText
import li.rajeshgo.sm.ui.queue.jobTitle
import li.rajeshgo.sm.ui.queue.reviewJobLine
import li.rajeshgo.sm.ui.settings.narrowerPolicyLabel
import li.rajeshgo.sm.ui.settings.reviewDayLine
import li.rajeshgo.sm.ui.settings.reviewLimitsPatch
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import java.time.OffsetDateTime

class ReviewUiTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test
    fun fallbackMatchesTheServerTierTable() {
        // The server's chain (review/mod.rs) for each row of appendix C2.
        assertEquals(
            listOf(Reviewer("codex", model = "gpt-6-sol", effort = "medium"), Reviewer("claude", model = "opus", effort = "high")),
            reviewFallback(Reviewer()),
        )
        assertEquals(listOf(Reviewer("claude", model = "fable", effort = "xhigh")), reviewFallback(Reviewer("codex", model = "gpt-6-astra", effort = "medium")))
        assertEquals("sonnet", reviewFallback(Reviewer("codex", model = "gpt-5.6-luna", effort = "high")).single().model)
        assertEquals("opus", reviewFallback(Reviewer("codex", model = "gpt-5.5", effort = "high")).single().model)
        assertEquals(listOf(Reviewer("codex", model = "gpt-6-astra", effort = "high")), reviewFallback(Reviewer("claude", model = "fable", effort = "max")))
        assertEquals("gpt-6-luna", reviewFallback(Reviewer("claude", model = "haiku", effort = "low")).single().model)
        assertEquals("gpt-6-sol", reviewFallback(Reviewer("claude", model = "opus[1m]", effort = "high")).single().model)
        val paired = reviewFallback(Reviewer("paired", provider = "claude", model = "sonnet", effort = "high"))
        assertEquals(listOf(Reviewer("claude", model = "sonnet", effort = "high"), Reviewer("codex", model = "gpt-6-luna", effort = "high")), paired)
    }

    @Test
    fun labelsReadAsTheMemoWritesThem() {
        assertEquals("GitHub Codex", reviewerLabel(Reviewer()))
        assertEquals("Codex run · gpt-6-astra · high", reviewerLabel(Reviewer("codex", model = "gpt-6-astra", effort = "high")))
        assertEquals("Paired Codex · gpt-6-luna · medium", reviewerLabel(Reviewer("paired", provider = "codex", model = "gpt-6-luna", effort = "medium")))
        assertEquals(
            "If it can't review: Codex run · gpt-6-sol · medium, then Claude run · opus · high",
            fallbackText(reviewFallback(Reviewer())),
        )
    }

    @Test
    fun reviewersSendOnlyTheirOwnFields() {
        // The server refuses extra fields, so GitHub Codex is exactly {"kind":"github_codex"}.
        assertEquals("""{"kind":"github_codex"}""", json.encodeToString(Reviewer()))
        assertEquals(
            """{"kind":"codex","model":"gpt-6-sol","effort":"medium"}""",
            json.encodeToString(defaultRun("codex")),
        )
        assertEquals(
            """{"scope":"lane","repo":"o/r","number":7,"reviewer":null}""",
            json.encodeToString(PutReviewPolicyRequest("lane", "o/r", 7, null)),
        )
    }

    @Test
    fun liveShapesParse() {
        val status = json.decodeFromString<ReviewStatus>(
            """{"github_codex":{"state":"paused","paused_at":"2026-09-30T20:54:52Z"},"last_24h":{"claude_runs":1,"codex_runs":14,"github_codex":28,"no_reviewer":3},
               "meters":{"claude":54.0,"codex":null},"running":[],"needs_you":[{"id":"r1","repo":"o/r","pr_number":5,"author_name":"sm-1","steps":[{"label":"GitHub Codex","reason":"out of code-review quota"}]}]}""",
        )
        assertEquals("paused", status.githubCodex.state)
        assertEquals(null, status.meters["codex"])
        assertEquals("out of code-review quota", status.needsYou.single().steps.single().reason)
        assertEquals("46 · GitHub Codex 28 · Codex runs 14 · Claude runs 1 · 3 left unreviewed", reviewDayLine(status))
        val options = json.decodeFromString<BoardStartOptions>(
            """{"working_dir":"/w","review_policy":{"resolved":{"kind":"claude","model":"fable","effort":"max"},"fallback":[],"source":"ticket #1848"}}""",
        )
        assertEquals("fable", options.reviewPolicy!!.resolved.model)
    }

    @Test
    fun narrowerRowsNameTheirScope() {
        assertEquals("Repo · session-manager", narrowerPolicyLabel(StoredReviewPolicy(scope = "repo", repo = "o/session-manager")))
        assertEquals("Lane · far#1843", narrowerPolicyLabel(StoredReviewPolicy(scope = "lane", repo = "o/far", number = 1843)))
        assertEquals("Ticket · far#1848", narrowerPolicyLabel(StoredReviewPolicy(scope = "ticket", repo = "o/far", number = 1848)))
    }

    @Test
    fun limitsPatchChecksRanges() {
        val patch = reviewLimitsPatch("4", "95")
        assertEquals("""{"queue_limits":{"review":4},"reviews":{"skip_meter_percent":95}}""", patch.toString())
        assertEquals("""{"reviews":{"skip_meter_percent":100}}""", reviewLimitsPatch(null, "100").toString())
        assertThrows(IllegalArgumentException::class.java) { reviewLimitsPatch("17", null) }
        assertThrows(IllegalArgumentException::class.java) { reviewLimitsPatch(null, "49") }
    }

    @Test
    fun boardAndQueueShowTheReview() {
        val now = OffsetDateTime.parse("2026-09-30T12:06:00Z")
        assertEquals(
            "Review: Codex run (gpt-6-sol, medium), 6m",
            boardReviewText(BoardTicketReview("active", "Codex run (gpt-6-sol, medium)", "2026-09-30T12:00:00Z"), now),
        )
        val job = SessionJob(
            id = "job_1",
            label = "review-x",
            review = JobReview("r1", "o/far", 1851, 1, "Codex run (gpt-6-sol, medium)", "default", "GitHub Codex: paused, so its fallback", "far-1848"),
        )
        assertEquals("review · far #1851", jobTitle(job))
        assertEquals("Codex run (gpt-6-sol, medium) · round 1 · for far-1848\nGitHub Codex: paused, so its fallback", reviewJobLine(job))
        assertEquals(null, reviewJobLine(SessionJob(id = "job_2")))
    }
}
