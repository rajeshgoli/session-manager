package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/**
 * A reviewer (sm#1768 appendix C1): `github_codex`, a `codex` or `claude` run,
 * or a ticket's `paired` reviewer. Unset fields are left out when sent, as the
 * server refuses unknown or extra fields.
 */
@OptIn(kotlinx.serialization.ExperimentalSerializationApi::class)
@Serializable
data class Reviewer(
    /** Always sent, even as the default: the server needs the kind. */
    @kotlinx.serialization.EncodeDefault val kind: String = "github_codex",
    val provider: String? = null,
    val model: String? = null,
    val effort: String? = null,
)

/** A stored policy as the board and `GET /review-policies` show it. */
@Serializable
data class ReviewPolicy(
    val reviewer: Reviewer = Reviewer(),
    val fallback: List<Reviewer> = emptyList(),
    @SerialName("set_by_name") val setByName: String? = null,
    @SerialName("set_at") val setAt: String? = null,
)

/** A ticket PR's active review request, shown in amber on the board. */
@Serializable
data class BoardTicketReview(
    val state: String = "",
    @SerialName("reviewer_label") val reviewerLabel: String? = null,
    val since: String? = null,
)

/** `GET /review-policies`: the default and every narrower policy. */
@Serializable
data class ReviewPoliciesResponse(
    val default: ResolvedReviewPolicy = ResolvedReviewPolicy(),
    val policies: List<StoredReviewPolicy> = emptyList(),
)

@Serializable
data class ResolvedReviewPolicy(
    val reviewer: Reviewer = Reviewer(),
    val fallback: List<Reviewer> = emptyList(),
    val source: String = "default",
)

@Serializable
data class StoredReviewPolicy(
    /** `repo`, `lane` or `ticket`. */
    val scope: String = "",
    val repo: String = "",
    /** 0 for a repo; the goal ticket for a lane; the ticket. */
    val number: Long = 0,
    val reviewer: Reviewer = Reviewer(),
    val fallback: List<Reviewer> = emptyList(),
    @SerialName("set_by_name") val setByName: String? = null,
    @SerialName("set_at") val setAt: String? = null,
)

/** `PUT /review-policies`; a null [reviewer] clears the stored policy. */
@Serializable
data class PutReviewPolicyRequest(
    /** `default`, `repo`, `lane` or `ticket`. */
    val scope: String,
    val repo: String?,
    val number: Long?,
    val reviewer: Reviewer?,
)

/** `GET /client/reviews/status`. */
@Serializable
data class ReviewStatus(
    @SerialName("github_codex") val githubCodex: GithubCodexChannel = GithubCodexChannel(),
    @SerialName("last_24h") val last24h: ReviewCounts = ReviewCounts(),
    val running: List<RunningReview> = emptyList(),
    @SerialName("needs_you") val needsYou: List<NoReviewerRequest> = emptyList(),
    /** Each provider's weekly meter, percent; null when unknown. */
    val meters: Map<String, Double?> = emptyMap(),
)

@Serializable
data class GithubCodexChannel(
    /** `available` or `paused`. */
    val state: String = "available",
    @SerialName("paused_at") val pausedAt: String? = null,
    @SerialName("next_check_at") val nextCheckAt: String? = null,
    @SerialName("quota_resets_at") val quotaResetsAt: String? = null,
    @SerialName("refusal_url") val refusalUrl: String? = null,
)

@Serializable
data class ReviewCounts(
    @SerialName("github_codex") val githubCodex: Int = 0,
    @SerialName("codex_runs") val codexRuns: Int = 0,
    @SerialName("claude_runs") val claudeRuns: Int = 0,
    @SerialName("no_reviewer") val noReviewer: Int = 0,
)

@Serializable
data class RunningReview(
    val id: String = "",
    val repo: String = "",
    @SerialName("pr_number") val prNumber: Long = 0,
    @SerialName("reviewer_label") val reviewerLabel: String? = null,
)

/** A request no reviewer could take, still waiting on the owner (appendix D7). */
@Serializable
data class NoReviewerRequest(
    val id: String = "",
    val repo: String = "",
    @SerialName("pr_number") val prNumber: Long = 0,
    @SerialName("author_name") val authorName: String = "",
    @SerialName("requested_at") val requestedAt: String? = null,
    val steps: List<NoReviewerStep> = emptyList(),
)

@Serializable
data class NoReviewerStep(val label: String = "", val reason: String = "")

/** Start options' `review_policy`: what a request for the ticket would use now. */
@Serializable
data class StartReviewPolicy(
    val resolved: Reviewer = Reviewer(),
    val fallback: List<Reviewer> = emptyList(),
    val source: String = "default",
)

/** A queue job's review fields (appendix I2). */
@Serializable
data class JobReview(
    @SerialName("request_id") val requestId: String = "",
    val repo: String = "",
    @SerialName("pr_number") val prNumber: Long = 0,
    val round: Int = 1,
    @SerialName("reviewer_label") val reviewerLabel: String? = null,
    @SerialName("policy_source") val policySource: String? = null,
    val why: String? = null,
    @SerialName("author_name") val authorName: String? = null,
)
