package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/** `GET /client/analytics/time` (sm#1662 appendix F.2). Durations are seconds. */
@Serializable
data class TimeReport(
    @SerialName("generated_at") val generatedAt: String,
    val range: String,
    val start: String,
    val end: String,
    val total: TimeTotal,
    @SerialName("parts_legend") val partsLegend: List<AnalyticsLegend> = emptyList(),
    @SerialName("tool_legend") val toolLegend: List<AnalyticsLegend> = emptyList(),
    val root: TimeNode,
)

@Serializable
data class TimeTotal(
    @SerialName("active_seconds") val activeSeconds: Long,
    @SerialName("parked_seconds") val parkedSeconds: Long,
    val agents: Int,
)

@Serializable
data class TimeNode(
    val id: String,
    val kind: String,
    val label: String,
    val state: String? = null,
    @SerialName("history_path") val historyPath: String? = null,
    @SerialName("session_id") val sessionId: String? = null,
    @SerialName("session_status") val sessionStatus: String? = null,
    @SerialName("active_seconds") val activeSeconds: Long = 0,
    @SerialName("parked_seconds") val parkedSeconds: Long = 0,
    val parts: Map<String, Long> = emptyMap(),
    val tools: Map<String, Long> = emptyMap(),
    val turns: Long? = null,
    val children: List<TimeNode> = emptyList(),
)
