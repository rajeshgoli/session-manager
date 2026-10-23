package li.rajeshgo.sm.data.model

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

/** `GET /client/overnight` (sm#2494): whether the owner is away, and why. */
@Serializable
data class OvernightStatus(
    val on: Boolean = false,
    val away: OvernightAway? = null,
    val waiting: OvernightWaiting? = null,
    /** The moon button is on (as opposed to bedtime or Do Not Disturb). */
    val manual: Boolean = false,
    /** The phone last reported Do Not Disturb on. */
    val dnd: Boolean = false,
    val settings: OvernightSettings = OvernightSettings(),
)

@Serializable
data class OvernightAway(
    /** `manual`, `bedtime` or `dnd`. */
    val source: String = "manual",
    val since: String = "",
    @SerialName("since_label") val sinceLabel: String = "",
    val until: String? = null,
    @SerialName("until_label") val untilLabel: String? = null,
    @SerialName("holds_alerts") val holdsAlerts: Boolean = true,
)

/** What waits for the owner since overnight began. */
@Serializable
data class OvernightWaiting(
    /** Agents blocked on the owner, by name. */
    val blocked: List<String> = emptyList(),
    val unread: Int = 0,
)

@Serializable
data class OvernightSettings(
    @SerialName("follow_dnd") val followDnd: Boolean = true,
    @SerialName("bedtime_enabled") val bedtimeEnabled: Boolean = true,
    @SerialName("bedtime_start") val bedtimeStart: String = "23:00",
    @SerialName("bedtime_end") val bedtimeEnd: String = "07:00",
    @SerialName("idle_minutes") val idleMinutes: Int = 20,
)

@Serializable
data class OvernightSwitch(val on: Boolean)
