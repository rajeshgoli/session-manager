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
data class SessionListResponse(
    val sessions: List<ClientSession> = emptyList(),
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
    @SerialName("primary_action")
    val primaryAction: PrimaryAction? = null,
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
data class PrimaryAction(
    val type: String? = null,
    val label: String? = null,
    val reason: String? = null,
)

@Serializable
data class OutputResponse(
    val output: String = "",
)

@Serializable
data class RequestStatusResponse(
    val status: String = "requested",
    val prompt: String,
    @SerialName("targeted_count")
    val targetedCount: Int = 0,
    @SerialName("delivered_count")
    val deliveredCount: Int = 0,
    @SerialName("queued_count")
    val queuedCount: Int = 0,
    @SerialName("failed_count")
    val failedCount: Int = 0,
    @SerialName("targeted_session_ids")
    val targetedSessionIds: List<String> = emptyList(),
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
data class KillSessionResponse(
    val status: String? = null,
    val error: String? = null,
)

@Serializable
data class KillSessionRequest(
    @SerialName("requester_session_id")
    val requesterSessionId: String? = null,
)

@Serializable
data class SessionDetail(
    val actionLines: List<String> = emptyList(),
    val tailLines: List<String> = emptyList(),
    val lastError: String? = null,
    val fetchedAt: Long = System.currentTimeMillis(),
)

@Serializable
data class AnalyticsSummary(
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
