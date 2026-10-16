package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/** `GET /client/autopm` (spec 2170 F.3, sm#2176): Settings › Auto PM and the What changed card. */
@Serializable
data class AutopmView(
    val enabled: Boolean = false,
    @SerialName("paused_at") val pausedAt: String? = null,
    @SerialName("skip_window_starting") val skipWindowStarting: String? = null,
    val skipped: Boolean = false,
    val budget: Map<String, AutopmBudget?>? = null,
    val changes: List<AutopmChange> = emptyList(),
    val ended: List<AutopmChange> = emptyList(),
    val waiting: List<AutopmWaiting> = emptyList(),
    val readout: AutopmReadout? = null,
    val switches: Map<String, Boolean> = emptyMap(),
)

@Serializable
data class AutopmBudget(
    val allowance: Double? = null,
    val spent: Double? = null,
    val remaining: Double? = null,
    @SerialName("resets_at") val resetsAt: String? = null,
    val skipped: Boolean = false,
)

@Serializable
data class AutopmChange(
    val id: String,
    val title: String,
    val line: String? = null,
    val state: String,
    @SerialName("switch_key") val switchKey: String? = null,
    @SerialName("live_at_ms") val liveAtMs: Long? = null,
    @SerialName("measure_at_ms") val measureAtMs: Long? = null,
    @SerialName("kept_at_ms") val keptAtMs: Long? = null,
    @SerialName("ended_at_ms") val endedAtMs: Long? = null,
    @SerialName("decision_note") val decisionNote: String? = null,
    val vote: String? = null,
)

@Serializable
data class AutopmWaiting(val id: String, val title: String, val path: String? = null)

@Serializable
data class AutopmReadout(val title: String, val week: String? = null, @SerialName("reader_path") val readerPath: String)

@Serializable
data class AutopmFeedback(@SerialName("experiment_id") val experimentId: String, val vote: String)

@Serializable
data class AutopmSwitchWrite(val on: Boolean, val reason: String? = null)
