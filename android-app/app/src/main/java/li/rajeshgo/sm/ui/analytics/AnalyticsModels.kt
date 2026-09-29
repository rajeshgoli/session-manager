package li.rajeshgo.sm.ui.analytics

/** The three Analytics sections (sm#1662); [key] is the route argument and the stored choice. */
enum class AnalyticsSection(val key: String, val label: String) {
    SPEND("spend", "Spend"),
    TIME("time", "Time"),
    QUEUE("queue", "Queue");

    companion object {
        fun fromKey(key: String?): AnalyticsSection? = entries.firstOrNull { it.key == key }
    }
}

/** The route's `section` argument wins, then the section last shown, then Spend. */
fun resolveAnalyticsSection(argument: String?, stored: String?): AnalyticsSection =
    AnalyticsSection.fromKey(argument) ?: AnalyticsSection.fromKey(stored) ?: AnalyticsSection.SPEND
