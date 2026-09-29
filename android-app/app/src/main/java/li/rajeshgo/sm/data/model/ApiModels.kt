package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class AuthSessionResponse(
    val enabled: Boolean = false,
    val authenticated: Boolean = false,
    val bypass: Boolean = false,
    val email: String? = null,
    val name: String? = null,
    @SerialName("auth_type")
    val authType: String? = null,
    val error: String? = null,
)

@Serializable
data class ClientBootstrapResponse(
    val auth: BootstrapAuth = BootstrapAuth(),
    @SerialName("external_access")
    val externalAccess: ExternalAccess = ExternalAccess(),
    @SerialName("session_open_defaults")
    val sessionOpenDefaults: SessionOpenDefaults = SessionOpenDefaults(),
)

@Serializable
data class BootstrapAuth(
    val mode: String = "",
    @SerialName("session_endpoint")
    val sessionEndpoint: String = "",
    @SerialName("login_endpoint")
    val loginEndpoint: String = "",
    @SerialName("logout_endpoint")
    val logoutEndpoint: String = "",
    @SerialName("device_auth_endpoint")
    val deviceAuthEndpoint: String = "",
    @SerialName("device_auth_token_type")
    val deviceAuthTokenType: String = "Bearer",
    @SerialName("google_server_client_id")
    val googleServerClientId: String? = null,
)

@Serializable
data class ExternalAccess(
    @SerialName("public_http_host")
    val publicHttpHost: String? = null,
    @SerialName("public_ssh_host")
    val publicSshHost: String? = null,
    @SerialName("ssh_username")
    val sshUsername: String? = null,
    @SerialName("termux_attach_supported")
    val termuxAttachSupported: Boolean = false,
    @SerialName("mobile_terminal_supported")
    val mobileTerminalSupported: Boolean = false,
    @SerialName("mobile_terminal_ws_url")
    val mobileTerminalWsUrl: String? = null,
)

@Serializable
data class SessionOpenDefaults(
    @SerialName("preferred_action")
    val preferredAction: String = "details",
    @SerialName("termux_package")
    val termuxPackage: String = "com.termux",
)

@Serializable
data class DeviceGoogleAuthRequest(
    @SerialName("id_token")
    val idToken: String,
)

@Serializable
data class DeviceGoogleAuthResponse(
    @SerialName("access_token")
    val accessToken: String,
    @SerialName("token_type")
    val tokenType: String = "Bearer",
    @SerialName("expires_at")
    val expiresAt: String,
    val email: String,
    val name: String? = null,
)

@Serializable
data class DeviceEnrollmentRequest(
    @SerialName("device_id")
    val deviceId: String,
    @SerialName("device_name")
    val deviceName: String,
    @SerialName("csr_pem")
    val csrPem: String,
    @SerialName("public_key_pem")
    val publicKeyPem: String,
)

@Serializable
data class DeviceEnrollmentResponse(
    @SerialName("device_id")
    val deviceId: String,
    @SerialName("device_name")
    val deviceName: String? = null,
    @SerialName("certificate_chain_pem")
    val certificateChainPem: String,
    @SerialName("expires_at")
    val expiresAt: String? = null,
)

@Serializable
data class AppArtifactMetadata(
    @SerialName("artifact_hash")
    val artifactHash: String? = null,
    @SerialName("size_bytes")
    val sizeBytes: Long? = null,
    @SerialName("uploaded_at")
    val uploadedAt: String? = null,
    @SerialName("uploaded_by")
    val uploadedBy: String? = null,
    @SerialName("version_code")
    val versionCode: Int? = null,
    @SerialName("version_name")
    val versionName: String? = null,
)

@Serializable
data class StudioSshToggleRequest(
    val enabled: Boolean,
)

@Serializable
data class StudioSshStatusResponse(
    val enabled: Boolean = false,
    val status: String = "off",
    val host: String = "",
    @SerialName("sshd_listening")
    val sshdListening: Boolean = false,
    @SerialName("tunnel_running")
    val tunnelRunning: Boolean = false,
    val error: String? = null,
)

@Serializable
data class SessionListResponse(
    val sessions: List<ClientSession> = emptyList(),
)

@Serializable
data class WhatRequestBody(
    @SerialName("delivery_mode")
    val deliveryMode: String,
    val prompt: String? = null,
)

@Serializable
data class WhatRequestRecord(
    @SerialName("request_id")
    val requestId: String,
    @SerialName("target_session_id")
    val targetSessionId: String,
    @SerialName("target_provider")
    val targetProvider: String,
    val status: String,
    @SerialName("created_at")
    val createdAt: String,
    @SerialName("started_at")
    val startedAt: String? = null,
    @SerialName("finished_at")
    val finishedAt: String? = null,
    val result: String? = null,
    val error: String? = null,
)

@Serializable
data class ClientSession(
    val id: String,
    val name: String,
    @SerialName("working_dir")
    val workingDir: String,
    val status: String,
    @SerialName("created_at")
    val createdAt: String,
    @SerialName("last_activity")
    val lastActivity: String,
    @SerialName("tmux_session")
    val tmuxSession: String,
    val provider: String? = null,
    val model: String? = null,
    @SerialName("reasoning_effort") val reasoningEffort: String? = null,
    val obligations: SessionObligations? = null,
    val jobs: List<SessionJob> = emptyList(),
    @SerialName("friendly_name")
    val friendlyName: String? = null,
    @SerialName("telegram_chat_id")
    val telegramChatId: Long? = null,
    @SerialName("telegram_thread_id")
    val telegramThreadId: Long? = null,
    @SerialName("current_task")
    val currentTask: String? = null,
    @SerialName("git_remote_url")
    val gitRemoteUrl: String? = null,
    @SerialName("parent_session_id")
    val parentSessionId: String? = null,
    @SerialName("last_handoff_path")
    val lastHandoffPath: String? = null,
    @SerialName("context_percent")
    val contextPercent: Double? = null,
    val handoff: HandoffPolicy? = null,
    @SerialName("agent_status_text")
    val agentStatusText: String? = null,
    @SerialName("agent_status_at")
    val agentStatusAt: String? = null,
    @SerialName("agent_task_completed_at")
    val agentTaskCompletedAt: String? = null,
    @SerialName("is_em")
    val isEm: Boolean = false,
    val role: String? = null,
    @SerialName("activity_state")
    val activityState: String? = null,
    @SerialName("last_tool_call")
    val lastToolCall: String? = null,
    @SerialName("last_tool_name")
    val lastToolName: String? = null,
    @SerialName("last_action_summary")
    val lastActionSummary: String? = null,
    @SerialName("last_action_at")
    val lastActionAt: String? = null,
    @SerialName("tokens_used")
    val tokensUsed: Int = 0,
    @SerialName("context_monitor_enabled")
    val contextMonitorEnabled: Boolean = false,
    @SerialName("pending_adoption_proposals")
    val pendingAdoptionProposals: List<AdoptionProposal> = emptyList(),
    val aliases: List<String> = emptyList(),
    @SerialName("is_maintainer")
    val isMaintainer: Boolean = false,
    @SerialName("attach_descriptor")
    val attachDescriptor: AttachDescriptor? = null,
    @SerialName("termux_attach")
    val termuxAttach: TermuxAttachMetadata? = null,
    @SerialName("mobile_terminal")
    val mobileTerminal: MobileTerminalMetadata? = null,
    @SerialName("primary_action")
    val primaryAction: PrimaryAction? = null,
    @SerialName("remote_control")
    val remoteControl: RemoteControlLink? = null,
)

@Serializable
data class AdoptionProposal(
    @SerialName("proposer_session_id")
    val proposerSessionId: String? = null,
    @SerialName("proposer_name")
    val proposerName: String? = null,
    @SerialName("created_at")
    val createdAt: String? = null,
    val status: String? = null,
)

@Serializable
data class AttachDescriptor(
    @SerialName("attach_supported")
    val attachSupported: Boolean = true,
    val message: String? = null,
    @SerialName("tmux_session")
    val tmuxSession: String? = null,
    @SerialName("runtime_mode")
    val runtimeMode: String? = null,
)

@Serializable
data class TermuxAttachMetadata(
    val supported: Boolean = false,
    val reason: String? = null,
    val transport: String? = null,
    @SerialName("ssh_host")
    val sshHost: String? = null,
    @SerialName("ssh_username")
    val sshUsername: String? = null,
    @SerialName("ssh_proxy_command")
    val sshProxyCommand: String? = null,
    @SerialName("ssh_command")
    val sshCommand: String? = null,
    @SerialName("tmux_session")
    val tmuxSession: String? = null,
    @SerialName("runtime_mode")
    val runtimeMode: String? = null,
    @SerialName("termux_package")
    val termuxPackage: String? = null,
)

@Serializable
data class MobileTerminalMetadata(
    val supported: Boolean = false,
    val reason: String? = null,
    val transport: String? = null,
    @SerialName("ticket_endpoint")
    val ticketEndpoint: String? = null,
    @SerialName("ws_url")
    val wsUrl: String? = null,
    @SerialName("tmux_session")
    val tmuxSession: String? = null,
    @SerialName("tmux_socket_name")
    val tmuxSocketName: String? = null,
    @SerialName("runtime_mode")
    val runtimeMode: String? = null,
    @SerialName("requires_device_key")
    val requiresDeviceKey: Boolean = true,
)

@Serializable
data class MobileAttachTicketRequest(
    val mode: String = "terminal",
)

@Serializable
data class MobileAttachTicketResponse(
    @SerialName("ticket_id")
    val ticketId: String,
    @SerialName("ticket_secret")
    val ticketSecret: String,
    @SerialName("device_key_id")
    val deviceKeyId: String,
    @SerialName("ws_url")
    val wsUrl: String,
    @SerialName("expires_at")
    val expiresAt: String,
)

/** Link that opens the session in its provider's own app (Claude Remote Control). */
@Serializable
data class RemoteControlLink(
    val provider: String? = null,
    val url: String? = null,
)

@Serializable
data class PrimaryAction(
    val type: String? = null,
    val label: String? = null,
    val reason: String? = null,
)

@Serializable
data class OutputResponse(
    val output: String? = null,
)

@Serializable
data class ToolCallsResponse(
    @SerialName("tool_calls")
    val toolCalls: List<ToolCallRow> = emptyList(),
)

@Serializable
data class ToolCallRow(
    val timestamp: String? = null,
    @SerialName("tool_name")
    val toolName: String? = null,
)

@Serializable
data class ActivityActionsResponse(
    val actions: List<ActivityActionRow> = emptyList(),
)

@Serializable
data class ActivityActionRow(
    @SerialName("summary_text")
    val summaryText: String? = null,
    @SerialName("action_kind")
    val actionKind: String? = null,
    val status: String? = null,
    @SerialName("started_at")
    val startedAt: String? = null,
    @SerialName("ended_at")
    val endedAt: String? = null,
)

@Serializable
data class RetireSessionResponse(
    val status: String? = null,
    val error: String? = null,
)

@Serializable
data class RetireSessionRequest(
    @SerialName("requester_session_id")
    val requesterSessionId: String? = null,
)

@Serializable
data class SessionDetail(
    val actionLines: List<String> = emptyList(),
    val tailLines: List<String> = emptyList(),
    val contextPercentage: Double? = null,
    val contextState: String? = null,
    val lastError: String? = null,
    val fetchedAt: Long = System.currentTimeMillis(),
)

@Serializable
data class ContextSnapshotResponse(
    @SerialName("session_id")
    val sessionId: String,
    @SerialName("used_percentage")
    val usedPercentage: Double? = null,
    val state: String = "unknown",
)

@Serializable
data class PersistedWhatSummaryEntry(
    val requestId: String,
    val markdown: String,
    val createdAt: String,
    val isUpdate: Boolean,
)

@Serializable
data class PersistedWhatSummary(
    val targetSessionId: String,
    val targetName: String,
    val entries: List<PersistedWhatSummaryEntry>,
    val updatedAt: String,
)

@Serializable
data class AnalyticsSummary(
    val workload: WorkloadMetrics? = null,
    @SerialName("generated_at")
    val generatedAt: String,
    @SerialName("window_hours")
    val windowHours: Int = 24,
    val kpis: AnalyticsKpiSet = AnalyticsKpiSet(),
    val throughput: List<AnalyticsThroughputBucket> = emptyList(),
    @SerialName("state_distribution")
    val stateDistribution: List<AnalyticsDistributionItem> = emptyList(),
    @SerialName("provider_distribution")
    val providerDistribution: List<AnalyticsDistributionItem> = emptyList(),
    @SerialName("repo_distribution")
    val repoDistribution: List<AnalyticsRepoItem> = emptyList(),
    @SerialName("longest_running")
    val longestRunning: List<AnalyticsLongestRunningItem> = emptyList(),
    @SerialName("health_checks")
    val healthChecks: List<AnalyticsHealthCheck> = emptyList(),
    val reliability: AnalyticsReliability = AnalyticsReliability(),
    val totals: AnalyticsTotals = AnalyticsTotals(),
    @SerialName("attach_available")
    val attachAvailable: Boolean = false,
)

@Serializable
data class AnalyticsKpiSet(
    @SerialName("active_sessions")
    val activeSessions: AnalyticsKpi = AnalyticsKpi(),
    @SerialName("sends_24h")
    val sends24h: AnalyticsKpi = AnalyticsKpi(),
    @SerialName("spawns_24h")
    val spawns24h: AnalyticsKpi = AnalyticsKpi(),
    @SerialName("active_tracks")
    val activeTracks: AnalyticsKpi = AnalyticsKpi(),
    @SerialName("overdue_tracks")
    val overdueTracks: AnalyticsKpi = AnalyticsKpi(),
    @SerialName("incidents_24h")
    val incidents24h: AnalyticsKpi = AnalyticsKpi(),
)

@Serializable
data class AnalyticsKpi(
    val label: String = "",
    val value: Int = 0,
    @SerialName("delta_pct")
    val deltaPct: Float? = null,
)

@Serializable
data class AnalyticsThroughputBucket(
    @SerialName("bucket_start")
    val bucketStart: String,
    @SerialName("bucket_label")
    val bucketLabel: String,
    val sends: Int = 0,
    val spawns: Int = 0,
    @SerialName("track_reminders")
    val trackReminders: Int = 0,
)

@Serializable
data class AnalyticsDistributionItem(
    val key: String,
    val label: String,
    val count: Int = 0,
    @SerialName("share_pct")
    val sharePct: Float? = null,
)

@Serializable
data class AnalyticsRepoItem(
    val key: String,
    val label: String,
    @SerialName("session_count")
    val sessionCount: Int = 0,
    @SerialName("tokens_used")
    val tokensUsed: Int = 0,
    @SerialName("share_pct")
    val sharePct: Float = 0f,
)

@Serializable
data class AnalyticsLongestRunningItem(
    val id: String,
    val name: String,
    val repo: String,
    val provider: String,
    @SerialName("age_hours")
    val ageHours: Float = 0f,
)

@Serializable
data class AnalyticsHealthCheck(
    val key: String,
    val label: String,
    val status: String? = null,
    val message: String? = null,
)

@Serializable
data class AnalyticsReliability(
    @SerialName("restart_count_24h")
    val restartCount24h: Int = 0,
    @SerialName("self_heal_count_24h")
    val selfHealCount24h: Int = 0,
)

@Serializable
data class AnalyticsTotals(
    @SerialName("tokens_live")
    val tokensLive: Int = 0,
    @SerialName("track_reminders_24h")
    val trackReminders24h: Int = 0,
)

@Serializable
data class CreateSessionRequest(
    val provider: String,
    @SerialName("working_dir") val workingDir: String,
    val model: String? = null,
    @SerialName("reasoning_effort") val reasoningEffort: String?,
    val name: String? = null,
    @SerialName("initial_message") val initialMessage: String? = null,
)

@Serializable
data class CreatedSession(val id: String)

@Serializable
data class SessionObligationsResponse(val sessions: List<SessionObligations> = emptyList())

@Serializable
data class SessionObligations(
    @SerialName("session_id") val sessionId: String,
    @SerialName("waiting_on") val waitingOn: List<WaitingObligation> = emptyList(),
    @SerialName("review_history") val reviewHistory: List<ReviewHistory> = emptyList(),
    val docs: List<SessionDoc> = emptyList(),
    /** Active work claims (`sm ticket`, `sm pr`); schema 3 and later. */
    val claims: List<SessionClaim> = emptyList(),
    /** Messages this agent sent the owner in the last 30 days, newest first; schema 4 and later (sm#1580). */
    val messages: List<SessionMessage> = emptyList(),
)

/** A message an agent sent the owner with `sm send <person>` (sm#1580). */
@Serializable
data class SessionMessage(
    val id: String = "",
    val title: String = "",
    /** `new`, `read`, `needs_you`, `replied` or `handled`. */
    val state: String = "",
    @SerialName("created_at") val createdAt: String? = null,
    @SerialName("reader_path") val readerPath: String = "",
)

/** A ticket or PR this session holds. `history_path` is its ticket page, `/t/<repo-name>/<n>`. */
@Serializable
data class MergeHold(
    @SerialName("placed_by") val placedBy: String = "",
    @SerialName("placed_at") val placedAt: String = "",
    val reason: String? = null,
)

@Serializable
data class SessionClaim(
    @SerialName("merge_hold") val mergeHold: MergeHold? = null,
    val kind: String = "",
    val repo: String = "",
    val number: Long = 0,
    val title: String = "",
    val state: String = "",
    @SerialName("claimed_at") val claimedAt: String? = null,
    val source: String = "",
    @SerialName("history_path") val historyPath: String? = null,
)

/** A doc this session published for the owner (`sm doc publish`). The internal doc id is deliberately not parsed: every link uses the readable form. */
@Serializable
data class SessionDoc(
    val title: String = "",
    val state: String = "",
    val repo: String = "",
    val path: String = "",
    @SerialName("pr_number") val prNumber: Long? = null,
    @SerialName("latest_commit_sha") val latestCommitSha: String = "",
    @SerialName("published_at") val publishedAt: String? = null,
    val name: String? = null,
    @SerialName("reader_path") val readerPath: String? = null,
    @SerialName("browser_url") val browserUrl: String? = null,
    /** The latest review reached no session: the author was retired with no parent. */
    @SerialName("review_undelivered") val reviewUndelivered: Boolean = false,
)

@Serializable
data class WaitingObligation(
    val kind: String = "",
    val label: String = "",
    val state: String = "",
    val since: String? = null,
    @SerialName("last_polled_at") val lastPolledAt: String? = null,
    @SerialName("last_error") val lastError: String? = null,
)

@Serializable
data class ReviewHistory(
    val repo: String = "",
    @SerialName("pr_number") val prNumber: Long = 0,
    @SerialName("landed_count") val landedCount: Int = 0,
    @SerialName("requested_by_agent") val requestedByAgent: Int = 0,
    @SerialName("landed_requested_by_agent") val landedRequestedByAgent: Int = 0,
)

@Serializable
data class SessionJobsResponse(val jobs: List<SessionJob> = emptyList())

@Serializable
data class JobHolding(val summary: String? = null, val detail: String? = null)

@Serializable
data class SessionJob(
    val id: String,
    val label: String = "",
    val state: String = "",
    val pid: Long? = null,
    @SerialName("requester_session_id") val requesterSessionId: String? = null,
    @SerialName("notify_session_id") val notifySessionId: String? = null,
    @SerialName("holding_reason") val holdingReason: String? = null,
    val holding: JobHolding? = null,
    /** Set while the owner's Start now run is going (sm#1627). */
    @SerialName("owner_forced_at") val ownerForcedAt: String? = null,
    @SerialName("queued_at") val queuedAt: String? = null,
    @SerialName("started_at") val startedAt: String? = null,
    @SerialName("finished_at") val finishedAt: String? = null,
    @SerialName("exit_code") val exitCode: Int? = null,
    // Queue page fields (sm#1609); absent from older servers.
    val type: String? = null,
    @SerialName("notify_name") val notifyName: String? = null,
    val cwd: String? = null,
    val argv: List<String>? = null,
    @SerialName("script_path") val scriptPath: String? = null,
    @SerialName("timeout_seconds") val timeoutSeconds: Long? = null,
    @SerialName("max_wait_seconds") val maxWaitSeconds: Long? = null,
    @SerialName("cpu_percent") val cpuPercent: Int? = null,
    @SerialName("gpu_percent") val gpuPercent: Int? = null,
    @SerialName("memory_bytes") val memoryBytes: Long? = null,
    val position: Int? = null,
    @SerialName("wait_deadline_at") val waitDeadlineAt: String? = null,
    @SerialName("ended_reason") val endedReason: String? = null,
    @SerialName("ended_summary") val endedSummary: String? = null,
) {
    fun isAwaitedBy(sessionId: String): Boolean =
        (notifySessionId?.takeIf(String::isNotBlank) ?: requesterSessionId) == sessionId
}

@Serializable
data class SessionModelsResponse(val models: List<String> = emptyList())

@Serializable
data class WorkloadMetrics(
    @SerialName("agents_live") val agentsLive: Int = 0,
    @SerialName("agents_working") val agentsWorking: Int = 0,
    @SerialName("agents_waiting_review") val agentsWaitingReview: Int = 0,
    @SerialName("agents_created_24h") val agentsCreated24H: Int = 0,
    @SerialName("jobs_running") val jobsRunning: Int = 0,
    @SerialName("jobs_queued") val jobsQueued: Int = 0,
    @SerialName("jobs_submitted_24h") val jobsSubmitted24H: Int = 0,
    @SerialName("jobs_completed_24h") val jobsCompleted24H: Int = 0,
    @SerialName("jobs_failed_24h") val jobsFailed24H: Int = 0,
    @SerialName("reviews_waiting") val reviewsWaiting: Int = 0,
    @SerialName("reviews_requested_24h") val reviewsRequested24H: Int = 0,
    @SerialName("reviews_received_24h") val reviewsReceived24H: Int = 0,
)

@Serializable
data class HostStatus(
    val available: Boolean = false,
    val host: String? = null,
    @SerialName("sampled_at") val sampledAt: String? = null,
    @SerialName("memory_total_bytes") val memoryTotalBytes: Long? = null,
    @SerialName("memory_used_bytes") val memoryUsedBytes: Long? = null,
    @SerialName("memory_pressure") val memoryPressure: String? = null,
    @SerialName("cpu_percent") val cpuPercent: Double? = null,
    @SerialName("gpu_percent") val gpuPercent: Double? = null,
    @SerialName("memory_available_bytes") val memoryAvailableBytes: Long? = null,
    val source: String? = null,
)

/** `GET /client/queue/jobs/{id}/start-check`: what Start now overrides (sm#1627). */
@Serializable
data class QueueStartCheck(
    @SerialName("job_id") val jobId: String,
    val state: String,
    val warnings: List<String> = emptyList(),
    @SerialName("memory_available_bytes") val memoryAvailableBytes: Long? = null,
    @SerialName("memory_reserve_bytes") val memoryReserveBytes: Long = 0,
    @SerialName("memory_estimate_bytes") val memoryEstimateBytes: Long? = null,
    @SerialName("memory_estimate_source") val memoryEstimateSource: String? = null,
    @SerialName("past_runs") val pastRuns: Int = 0,
)

/** `GET /client/queue` (sm#1609). */
@Serializable
data class QueueOverview(
    @SerialName("generated_at") val generatedAt: String? = null,
    @SerialName("owner_name") val ownerName: String? = null,
    val host: HostStatus? = null,
    val slots: QueueSlots = QueueSlots(),
    val running: List<SessionJob> = emptyList(),
    val queued: List<SessionJob> = emptyList(),
    val ended: List<SessionJob> = emptyList(),
)

@Serializable
data class SlotCount(val running: Int = 0, val max: Int = 0)

@Serializable
data class QueueSlots(
    val running: Int = 0,
    val max: Int = 0,
    @SerialName("by_type") val byType: Map<String, SlotCount> = emptyMap(),
)

/** `GET /client/queue/stats`: the "Held back?" card. */
@Serializable
data class QueueStats(
    val available: Boolean = false,
    val hours: Int = 0,
    val waiting: List<QueueWaitingGroup> = emptyList(),
    @SerialName("by_type") val byType: List<QueueTypeStats> = emptyList(),
)

@Serializable
data class QueueWaitingGroup(
    val group: String = "",
    @SerialName("job_seconds") val jobSeconds: Long = 0,
    @SerialName("headroom_job_seconds") val headroomJobSeconds: Long = 0,
    @SerialName("unknown_job_seconds") val unknownJobSeconds: Long = 0,
)

@Serializable
data class QueueTypeStats(
    val type: String = "",
    val jobs: Int = 0,
    @SerialName("peak_rss_p50_bytes") val peakRssP50Bytes: Long? = null,
    @SerialName("peak_rss_p95_bytes") val peakRssP95Bytes: Long? = null,
    @SerialName("peak_rss_max_bytes") val peakRssMaxBytes: Long? = null,
    @SerialName("cpu_cores_p95") val cpuCoresP95: Double? = null,
)

/** `GET /client/utilization/series`: the Mac usage charts. */
@Serializable
data class UtilizationSeries(
    val available: Boolean = false,
    val hours: Int = 0,
    @SerialName("bucket_seconds") val bucketSeconds: Long = 0,
    val start: String? = null,
    val end: String? = null,
    @SerialName("memory_total_bytes") val memoryTotalBytes: Long? = null,
    val buckets: List<UtilizationBucket> = emptyList(),
    val summary: UtilizationSummary? = null,
)

@Serializable
data class UtilizationBucket(
    val start: String = "",
    val samples: Int = 0,
    @SerialName("cpu_avg") val cpuAvg: Double? = null,
    @SerialName("cpu_max") val cpuMax: Double? = null,
    @SerialName("gpu_avg") val gpuAvg: Double? = null,
    @SerialName("gpu_max") val gpuMax: Double? = null,
    @SerialName("mem_used_avg") val memUsedAvg: Long? = null,
    @SerialName("mem_used_max") val memUsedMax: Long? = null,
    @SerialName("mem_available_min") val memAvailableMin: Long? = null,
    @SerialName("pressure_max") val pressureMax: Int? = null,
    val running: Map<String, Double> = emptyMap(),
    @SerialName("pending_max") val pendingMax: Int? = null,
)

@Serializable
data class UtilizationSummary(
    @SerialName("covered_seconds") val coveredSeconds: Long = 0,
    @SerialName("cpu_avg") val cpuAvg: Double? = null,
    @SerialName("cpu_busy_seconds") val cpuBusySeconds: Long = 0,
    @SerialName("gpu_avg") val gpuAvg: Double? = null,
    @SerialName("mem_used_max") val memUsedMax: Long? = null,
    @SerialName("pressure_elevated_seconds") val pressureElevatedSeconds: Long = 0,
    @SerialName("headroom_seconds") val headroomSeconds: Long = 0,
    @SerialName("unknown_seconds") val unknownSeconds: Long = 0,
)

/** Body of `POST /queue-jobs/{id}/cancel`; a null note cancels silently. */
@Serializable
data class CancelQueueJobBody(val note: String? = null)

@Serializable
data class QueueJobLog(
    @SerialName("job_id") val jobId: String = "",
    val text: String = "",
)

/** Body of `POST /sessions/{id}/follow`; a null or blank message follows silently. */
@Serializable
data class FollowSessionRequest(
    val message: String?,
)

/** An owner follow of an agent or a queue job (sm#1569). */
@Serializable
data class OwnerFollow(
    val id: String,
    @SerialName("target_kind") val targetKind: String = "session",
    @SerialName("session_id") val sessionId: String = "",
    @SerialName("session_name") val sessionName: String = "",
    @SerialName("job_id") val jobId: String? = null,
    @SerialName("job_label") val jobLabel: String? = null,
    val state: String = "",
    @SerialName("fire_reason") val fireReason: String? = null,
    @SerialName("report_reader_path") val reportReaderPath: String? = null,
) {
    val isActive: Boolean get() = state == "active"
}

@Serializable
data class FollowsResponse(
    @SerialName("push_configured") val pushConfigured: Boolean = false,
    /** How sm names the owner (sm#1580); builds the Follow dialog's default message. */
    @SerialName("owner_name") val ownerName: String? = null,
    val follows: List<OwnerFollow> = emptyList(),
)

@Serializable
data class PushTokenRequest(
    val token: String,
    @SerialName("device_id") val deviceId: String? = null,
    @SerialName("device_name") val deviceName: String,
    @SerialName("app_version") val appVersion: String,
)

@Serializable
data class DeletePushTokenRequest(
    val token: String,
)

@Serializable
data class TestPushFailure(
    @SerialName("device_name") val deviceName: String = "",
    val error: String = "",
)

@Serializable
data class TestPushResponse(
    val sent: Int = 0,
    val failed: List<TestPushFailure> = emptyList(),
)

/** `GET /inbox?format=json` (sm#1647): the owner's Inbox, one row per thread. */
@Serializable
data class InboxResponse(
    val filter: String = "open",
    /** Open rows that need the owner: the tab badge. */
    @SerialName("needs_you_count") val needsYouCount: Int = 0,
    /** Any Open row is new: the tab dot. */
    @SerialName("has_new") val hasNew: Boolean = false,
    val rows: List<InboxRow> = emptyList(),
)

/** One Inbox thread: an agent's exchange with the owner, or one doc across its revisions. */
@Serializable
data class InboxRow(
    @SerialName("thread_key") val threadKey: String = "",
    /** `agent` or `doc`. */
    val kind: String = "",
    val title: String = "",
    val repo: String = "",
    /** Agent: `live` or `ended`. Doc: its state (`new`, `updated`, `review_requested`, `reviewed`, `read`). */
    val status: String = "",
    /** A doc's latest posted verdict: `approve`, `changes_requested` or `comment`. */
    val verdict: String? = null,
    @SerialName("pr_number") val prNumber: Long? = null,
    /** A doc's latest revision's publisher. */
    val author: String? = null,
    /** `needs_you`, `new` or `earlier`. */
    val group: String = "",
    val preview: String = "",
    @SerialName("newest_at") val newestAt: String = "",
    @SerialName("message_count") val messageCount: Int = 0,
    @SerialName("revision_count") val revisionCount: Int = 0,
    @SerialName("open_asks") val openAsks: Int = 0,
    /** The page the row opens: the agent thread, or the doc. */
    val url: String = "",
    val done: Boolean = false,
    @SerialName("session_id") val sessionId: String? = null,
    @SerialName("doc_id") val docId: String? = null,
)

@Serializable
data class InboxDoneRequest(
    @SerialName("thread_key") val threadKey: String,
)

/** `GET /guestbook?format=json` (sm#1660): signed entries, newest first. */
@Serializable
data class GuestbookResponse(
    val entries: List<GuestbookEntry> = emptyList(),
    /** The `before` cursor for the next older page; null on the last page. */
    @SerialName("next_before") val nextBefore: Long? = null,
)

/** One agent's note, signed as it finished. */
@Serializable
data class GuestbookEntry(
    val id: Long = 0,
    @SerialName("session_id") val sessionId: String = "",
    @SerialName("session_name") val sessionName: String = "",
    val provider: String = "",
    val model: String? = null,
    /** `owner/name` slugs. */
    val repos: List<String> = emptyList(),
    val claims: List<GuestbookClaim> = emptyList(),
    @SerialName("signed_at") val signedAt: String = "",
    /** The note, markdown. */
    val text: String = "",
)

/** A ticket or PR the agent held when it signed. */
@Serializable
data class GuestbookClaim(
    val repo: String = "",
    val number: Long = 0,
    /** `ticket` or `pr`. */
    val kind: String = "",
    val title: String = "",
)

/** `GET /history/agents` (sm#1661): agents no longer live, newest first. */
@Serializable
data class AgentHistoryResponse(
    val agents: List<AgentHistoryRow> = emptyList(),
    /** The `before` cursor for the next older page; null on the last page. */
    @SerialName("next_before") val nextBefore: String? = null,
    /** Agents matching the search across every page. */
    val total: Int = 0,
)

@Serializable
data class AgentHistoryRow(
    val id: String = "",
    val name: String = "",
    val provider: String = "",
    val model: String? = null,
    val role: String? = null,
    @SerialName("working_dir") val workingDir: String = "",
    val node: String = "",
    @SerialName("parent_session_id") val parentSessionId: String? = null,
    /** `retired` or `stopped`. */
    val state: String = "",
    @SerialName("ended_at") val endedAt: String = "",
    @SerialName("last_status") val lastStatus: String? = null,
    val restorable: Boolean = true,
    @SerialName("unrestorable_reason") val unrestorableReason: String? = null,
    val work: AgentWork = AgentWork(),
)

/** What an agent worked on, newest first, at most 20 of each. */
@Serializable
data class AgentWork(
    val tickets: List<AgentWorkItem> = emptyList(),
    val prs: List<AgentWorkItem> = emptyList(),
    val docs: List<AgentWorkDoc> = emptyList(),
)

@Serializable
data class AgentWorkItem(
    val repo: String = "",
    val number: Long = 0,
    val title: String = "",
    val state: String = "",
    val url: String = "",
    /** `/t/<repo-name>/<n>`. */
    @SerialName("history_path") val historyPath: String = "",
)

@Serializable
data class AgentWorkDoc(
    val id: String = "",
    /** `<repo-name>/<path>`. */
    val name: String = "",
    val title: String = "",
    val state: String = "",
    @SerialName("reader_path") val readerPath: String = "",
)
