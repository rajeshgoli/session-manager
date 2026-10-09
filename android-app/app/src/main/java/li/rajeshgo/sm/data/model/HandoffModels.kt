package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class HandoffPolicy(
    val enabled: Boolean,
    @SerialName("threshold_percent") val thresholdPercent: Double,
    val source: String,
    @SerialName("has_gauge") val hasGauge: Boolean,
    val display: String,
    val state: String? = null,
    val successor: HandoffAgent? = null,
    val predecessor: HandoffAgent? = null,
)

@Serializable
data class HandoffAgent(val id: String, val name: String)

@Serializable
data class HandoffDefaults(
    val providers: Map<String, Boolean>,
    @SerialName("threshold_percent") val thresholdPercent: Double,
    @SerialName("ask_on_codex_review") val askOnCodexReview: Boolean,
    @SerialName("ask_on_doc_review") val askOnDocReview: Boolean,
    @SerialName("review_floor_percent") val reviewFloorPercent: Double,
    @SerialName("reminder_percent") val reminderPercent: Double,
    @SerialName("provider_thresholds") val providerThresholds: Map<String, HandoffThresholds> = emptyMap(),
)

@Serializable
data class HandoffThresholds(
    @SerialName("threshold_percent") val thresholdPercent: Double,
    @SerialName("review_floor_percent") val reviewFloorPercent: Double,
    @SerialName("reminder_percent") val reminderPercent: Double,
)
