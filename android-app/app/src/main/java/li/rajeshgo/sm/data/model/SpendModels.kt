package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class SpendReport(
    @SerialName("generated_at") val generatedAt: String,
    val provider: String,
    val range: String,
    val start: String,
    val end: String,
    val meters: List<SpendMeter> = emptyList(),
    val total: SpendTotal,
    val basis: Map<String, Double> = emptyMap(),
    @SerialName("parts_legend") val partsLegend: List<AnalyticsLegend> = emptyList(),
    val notes: List<String> = emptyList(),
    val root: SpendNode,
    val local: LocalSpend? = null,
)

@Serializable
data class AnalyticsLegend(val key: String, val label: String)

@Serializable
data class SpendTotal(val percent: Double, val tokens: Long)

@Serializable
data class SpendMeter(
    @SerialName("account_key") val accountKey: String,
    val label: String? = null,
    val percent: Double,
    @SerialName("observed_at") val observedAt: String,
    @SerialName("resets_at") val resetsAt: String,
    val pace: SpendPace? = null,
)

@Serializable
data class SpendPace(val kind: String, val at: String? = null, val percent: Double? = null)

@Serializable
data class SpendNode(
    val id: String,
    val kind: String,
    val label: String,
    val state: String? = null,
    @SerialName("history_path") val historyPath: String? = null,
    @SerialName("session_id") val sessionId: String? = null,
    @SerialName("session_status") val sessionStatus: String? = null,
    val percent: Double = 0.0,
    val tokens: Long = 0,
    val parts: Map<String, Double> = emptyMap(),
    val children: List<SpendNode> = emptyList(),
    val models: List<SpendModel>? = null,
)

@Serializable
data class SpendModel(
    val model: String,
    val effort: String? = null,
    val turns: Long,
    val percent: Double,
    val tokens: SpendTokens,
)

@Serializable
data class SpendTokens(
    val input: Long = 0,
    val output: Long = 0,
    @SerialName("cache_write") val cacheWrite: Long = 0,
    @SerialName("cache_read") val cacheRead: Long = 0,
)

@Serializable
data class LocalSpend(
    val models: List<SpendModel> = emptyList(),
    val days: List<LocalSpendDay> = emptyList(),
    @SerialName("busy_hours") val busyHours: Double = 0.0,
)

@Serializable
data class LocalSpendDay(val date: String, @SerialName("busy_hours") val busyHours: Double)
