package li.rajeshgo.sm.ui.queue

import java.time.Duration
import java.time.LocalDateTime
import java.time.OffsetDateTime
import java.time.ZoneId
import java.time.ZoneOffset
import java.time.format.DateTimeFormatter
import java.time.format.DateTimeParseException
import java.util.Locale
import kotlin.math.roundToLong
import li.rajeshgo.sm.data.model.HostStatus
import li.rajeshgo.sm.data.model.QueueSlots
import li.rajeshgo.sm.data.model.QueueStats
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.data.model.UtilizationSummary

/** Pure formatting for the Queue and Mac usage screens (sm#1609). */

/** Waiting jobs across the agents list, for the Queue tab's badge. */
fun waitingJobCount(sessions: List<li.rajeshgo.sm.data.model.ClientSession>): Int =
    sessions.flatMap { it.jobs }.filter { it.state == "pending" }.distinctBy { it.id }.size

private const val GIB = 1024.0 * 1024.0 * 1024.0

/** Server timestamps; naive ones (Python-era rows) are UTC. */
fun parseQueueTime(value: String?): OffsetDateTime? {
    if (value.isNullOrBlank()) return null
    return try {
        OffsetDateTime.parse(value)
    } catch (_: DateTimeParseException) {
        try {
            LocalDateTime.parse(value).atOffset(ZoneOffset.UTC)
        } catch (_: DateTimeParseException) {
            null
        }
    }
}

/** "40s", "5m", "1h", "1h 20m", "3d 4h". */
fun shortDuration(seconds: Long): String {
    val s = seconds.coerceAtLeast(0)
    return when {
        s < 60 -> "${s}s"
        s < 3600 -> "${s / 60}m"
        s < 86_400 -> if (s % 3600 < 60) "${s / 3600}h" else "${s / 3600}h ${s % 3600 / 60}m"
        else -> if (s % 86_400 < 3600) "${s / 86_400}d" else "${s / 86_400}d ${s % 86_400 / 3600}h"
    }
}

/** Hours with one decimal, or minutes under an hour: "11.2 h", "12 min". */
fun hoursLabel(seconds: Long): String =
    if (seconds < 3600) "${(seconds / 60.0).roundToLong()} min"
    else String.format(Locale.US, "%.1f h", seconds / 3600.0)

fun secondsBetween(from: String?, to: OffsetDateTime): Long? =
    parseQueueTime(from)?.let { Duration.between(it, to).seconds }

/** Whole GiB, as the terminal status bar and the Settings sheet show memory. */
fun gib(bytes: Long?): String? = bytes?.let { (it / GIB).roundToLong().toString() }

/** "Free now 20 GiB · 8 GiB reserve · needs ~30 GiB (past runs)". */
fun startNowMemoryLine(check: li.rajeshgo.sm.data.model.QueueStartCheck): String = listOfNotNull(
    "Free now ${gib(check.memoryAvailableBytes)?.let { "$it GiB" } ?: "unknown"}",
    "${gib(check.memoryReserveBytes)} GiB reserve",
    check.memoryEstimateBytes?.let { bytes ->
        val source = if (check.memoryEstimateSource == "declared") "declared" else "most past runs used"
        "needs ~${gib(bytes)} GiB ($source)"
    },
).joinToString(" · ")

/** The agent a job reports to, by name when known. */
fun jobAgentLabel(job: SessionJob): String =
    job.notifyName?.takeIf { it.isNotBlank() }
        ?: job.notifySessionId?.takeIf { it.isNotBlank() }?.take(8)
        ?: "no agent"

fun jobTitle(job: SessionJob): String = job.label.ifBlank { job.id }

/** "tests · 4m of 15m · 12G · cpu 25% · gpu 3%"; use is the job's share of the whole Mac. */
fun runningLine(job: SessionJob, now: OffsetDateTime): String {
    val elapsed = secondsBetween(job.startedAt, now)?.let(::shortDuration) ?: "-"
    val limit = job.timeoutSeconds?.let(::shortDuration)
    val usage = job.usage
    return listOfNotNull(
        job.type,
        if (limit != null) "$elapsed of $limit" else elapsed,
        usage?.memoryBytes?.let { "${gib(it)}G" },
        usage?.cpuPercent?.let { "cpu ${it.roundToLong()}%" },
        usage?.gpuPercent?.takeIf { it >= 0.5 }?.let { "gpu ${it.roundToLong()}%" },
    ).joinToString(" · ")
}

/** Elapsed share of the job's time limit, 0..1. */
fun runningProgress(job: SessionJob, now: OffsetDateTime): Float {
    val elapsed = secondsBetween(job.startedAt, now) ?: return 0f
    val limit = job.timeoutSeconds?.takeIf { it > 0 } ?: return 0f
    return (elapsed.toFloat() / limit).coerceIn(0f, 1f)
}

/** "perf · waiting 3m · gives up in 2m", or "· giving up" past the deadline. */
fun queuedLine(job: SessionJob, now: OffsetDateTime): String {
    val waited = secondsBetween(job.queuedAt, now)?.let { "waiting ${shortDuration(it)}" }
    val deadline = parseQueueTime(job.waitDeadlineAt)?.let { Duration.between(now, it).seconds }
    val givesUp = deadline?.let { if (it > 0) "gives up in ${shortDuration(it)}" else "giving up" }
    return listOfNotNull(job.type, waited, givesUp).joinToString(" · ")
}

/** Why a waiting job is not running: the server's summary, else a fallback. */
fun queuedReason(job: SessionJob): String =
    job.holding?.summary ?: when (job.holdingReason) {
        "awaiting_tests" -> "Tests ahead"
        "perf_running" -> "Performance run in progress"
        "perf_cooldown" -> "Performance cooldown"
        "concurrency_cap" -> "Waiting for a free slot"
        else -> job.holdingReason?.replace('_', ' ') ?: "Waiting for scheduler"
    }

/** "background · 40m ago". */
fun endedLine(job: SessionJob, now: OffsetDateTime): String {
    val ago = secondsBetween(job.finishedAt, now)?.let { "${shortDuration(it)} ago" }
    return listOfNotNull(job.type, ago).joinToString(" · ")
}

/**
 * The Queue tab's amber line to Analytics › Queue: null when nothing stopped
 * in the last 24 h. The server returns at most 50 stopped jobs, so 50 reads "50+".
 */
fun stoppedLinkText(endedCount: Int): String? = when {
    endedCount <= 0 -> null
    endedCount >= 50 -> "50+ stopped in the last 24h ›"
    else -> "$endedCount stopped in the last 24h ›"
}

/** ⌛ for jobs that never started, ✕ for jobs the queue stopped. */
fun endedIcon(reason: String?): String = if (reason == "gave_up") "⌛" else "✕"

/** "3 of 8 slots in use · tests 2/6 · perf 0/1 · background 1/2 · service 0/2". */
fun slotsLine(slots: QueueSlots): String {
    val types = listOf("tests", "perf", "background", "service").mapNotNull { type ->
        slots.byType[type]?.let { "$type ${it.running}/${it.max}" }
    }
    return (listOf("${slots.running} of ${slots.max} slots in use") + types).joinToString(" · ")
}

enum class Meter { LOW, MID, HIGH }

/** Bar colour bands: under 60% low, 60–85% mid, over 85% high. */
fun meterBand(fraction: Double): Meter = when {
    fraction > 0.85 -> Meter.HIGH
    fraction >= 0.60 -> Meter.MID
    else -> Meter.LOW
}

/**
 * One live bar. [fraction] is the whole Mac's use; [queueFraction] is the part
 * running queue jobs account for, drawn darker inside it, and [queueValue]
 * names it as a share of what is used.
 */
data class MeterRow(
    val label: String,
    val fraction: Double?,
    val value: String,
    val warning: String? = null,
    val queueFraction: Double? = null,
    val queueValue: String? = null,
)

/** The queue's part of a reading, both as a fraction of the bar and as "queue 43%" of what is used. */
private fun queuePart(queue: Double?, used: Double?, whole: Double): Pair<Double?, String?> {
    if (queue == null || used == null || whole <= 0) return null to null
    val capped = queue.coerceIn(0.0, used)
    val share = if (used > 0) (capped / used * 100).roundToLong() else 0L
    return capped / whole to "queue $share%"
}

/** MEM / CPU / GPU rows for the live bar; empty when the host is unavailable. */
fun meterRows(host: HostStatus?): List<MeterRow> {
    if (host == null || !host.available) return emptyList()
    val total = host.memoryTotalBytes
    val used = host.memoryUsedBytes
    val memFraction = if (total != null && used != null && total > 0) used.toDouble() / total else null
    val memValue = if (total != null && used != null) "${gib(used)}/${gib(total)}G" else "-"
    val pressure = host.memoryPressure?.takeIf { it != "Normal" }
    val (memQueue, memQueueValue) = queuePart(host.queueMemoryBytes?.toDouble(), used?.toDouble(), total?.toDouble() ?: 0.0)
    val (cpuQueue, cpuQueueValue) = queuePart(host.queueCpuPercent, host.cpuPercent, 100.0)
    val (gpuQueue, gpuQueueValue) = queuePart(host.queueGpuPercent, host.gpuPercent, 100.0)
    return listOf(
        MeterRow("MEM", memFraction, memValue, pressure, memQueue, memQueueValue),
        MeterRow("CPU", host.cpuPercent?.div(100.0), host.cpuPercent?.let { "${it.roundToLong()}%" } ?: "-", null, cpuQueue, cpuQueueValue),
        MeterRow("GPU", host.gpuPercent?.div(100.0), host.gpuPercent?.let { "${it.roundToLong()}%" } ?: "-", null, gpuQueue, gpuQueueValue),
    )
}

/** The "Held back?" card's lines; empty when nothing has been recorded. */
fun heldBackLines(stats: QueueStats?): List<String> {
    if (stats == null || !stats.available) return emptyList()
    val lines = mutableListOf<String>()
    stats.waiting.firstOrNull { it.group == "limits" }?.takeIf { it.jobSeconds > 0 }?.let { limits ->
        val known = limits.jobSeconds - limits.unknownJobSeconds
        val share = if (known > 0) " The machine had headroom for ${(limits.headroomJobSeconds * 100 / known)}% of it." else ""
        lines += "Limits held jobs ${hoursLabel(limits.jobSeconds)}.$share"
    }
    stats.waiting.firstOrNull { it.group == "perf_rules" }?.takeIf { it.jobSeconds > 0 }?.let {
        lines += "Perf rules held jobs ${hoursLabel(it.jobSeconds)} (by design)."
    }
    stats.waiting.firstOrNull { it.group == "memory" }?.takeIf { it.jobSeconds > 0 }?.let {
        lines += "Perf jobs waited ${hoursLabel(it.jobSeconds)} for memory."
    }
    stats.byType.firstOrNull { it.type == "background" }?.peakRssP95Bytes?.let {
        lines += "Background jobs peak at ${gib(it)}G each (95th percentile)."
    }
    if (lines.isEmpty()) lines += "Nothing waited in this window."
    return lines
}

/** The question sent to a job's agent from Ask agent. */
fun askAgentPrompt(ownerName: String, job: SessionJob, question: String, now: OffsetDateTime): String {
    val timing = when (job.state) {
        "running" -> secondsBetween(job.startedAt, now)?.let { "running for ${shortDuration(it)}" } ?: "running"
        "pending" -> secondsBetween(job.queuedAt, now)?.let { "waiting ${shortDuration(it)}" } ?: "waiting"
        else -> job.state
    }
    val type = job.type ?: "job"
    return "$ownerName is asking from the sm app about your queue job ${jobTitle(job)} " +
        "(ID ${job.id}, type $type, $timing): ${question.trim()}"
}

val ASK_AGENT_SUGGESTIONS = listOf(
    "How long do you expect this to run?",
    "What is this job for?",
    "Is it safe to cancel this?",
)

fun jobCommand(job: SessionJob): String? =
    job.argv?.takeIf { it.isNotEmpty() }?.joinToString(" ") ?: job.scriptPath?.let { "script: $it" }

/** "time limit 15m · cpu 100% · gpu 0% · memory 200G". */
fun jobLimits(job: SessionJob): String = listOfNotNull(
    job.timeoutSeconds?.let { "time limit ${shortDuration(it)}" },
    job.cpuPercent?.let { "cpu $it%" },
    job.gpuPercent?.let { "gpu $it%" },
    job.memoryBytes?.let { "memory ${gib(it)}G" },
).joinToString(" · ")

// ---------------------------------------------------------------------------
// Mac usage
// ---------------------------------------------------------------------------

val USAGE_RANGES = listOf(1 to "1h", 24 to "24h", 168 to "7d", 720 to "30d")

/** Seconds between auto-refreshes for a range; null for ranges that don't refresh. */
fun usageRefreshSeconds(hours: Int): Long? = when (hours) {
    1 -> 15
    24 -> 60
    else -> null
}

/** Indices of buckets that get an x label, and the label; the last reads "now". */
fun usageAxisLabels(starts: List<String>, hours: Int, zone: ZoneId): List<Pair<Int, String>> {
    if (starts.isEmpty()) return emptyList()
    val stepSeconds: Long = when (hours) {
        1 -> 15 * 60
        24 -> 6 * 3600
        168 -> 86_400
        else -> 7 * 86_400
    }
    val format = when (hours) {
        1, 24 -> DateTimeFormatter.ofPattern("HH:mm", Locale.US)
        168 -> DateTimeFormatter.ofPattern("EEE", Locale.US)
        else -> DateTimeFormatter.ofPattern("MMM d", Locale.US)
    }
    val labels = mutableListOf<Pair<Int, String>>()
    starts.forEachIndexed { index, start ->
        val at = parseQueueTime(start) ?: return@forEachIndexed
        val local = at.atZoneSameInstant(zone)
        val epochLocal = local.toLocalDateTime().toEpochSecond(ZoneOffset.UTC)
        if (index < starts.size - 1 && epochLocal % stepSeconds == 0L) {
            labels += index to local.format(format)
        }
    }
    // Keep "now" clear of a label that would overlap it.
    val minGap = (starts.size / 8).coerceAtLeast(1)
    return labels.filter { it.first <= starts.size - 1 - minGap } + ((starts.size - 1) to "now")
}

/** Runs of consecutive plottable values; a gap (null) ends a run. */
fun <T> plotRuns(values: List<T?>): List<List<Pair<Int, T>>> {
    val runs = mutableListOf<List<Pair<Int, T>>>()
    var current = mutableListOf<Pair<Int, T>>()
    values.forEachIndexed { index, value ->
        if (value == null) {
            if (current.isNotEmpty()) runs += current
            current = mutableListOf()
        } else {
            current += index to value
        }
    }
    if (current.isNotEmpty()) runs += current
    return runs
}

/** Background shading for memory pressure: 0 none, 1 elevated, 2 critical. */
fun pressureShade(pressureMax: Int?): Int = when {
    pressureMax == null -> 0
    pressureMax >= 4 -> 2
    pressureMax >= 2 -> 1
    else -> 0
}

fun usageSummaryLines(summary: UtilizationSummary?): List<String> {
    if (summary == null) return emptyList()
    val lines = mutableListOf<String>()
    summary.cpuAvg?.let { avg ->
        val busy = if (summary.cpuBusySeconds > 0) ", over 85% busy for ${hoursLabel(summary.cpuBusySeconds)}" else ""
        lines += "CPU averaged ${avg.roundToLong()}%$busy."
    }
    summary.memUsedMax?.let { peak ->
        val pressure = if (summary.pressureElevatedSeconds > 0) "; pressure was elevated for ${hoursLabel(summary.pressureElevatedSeconds)}" else ""
        lines += "Memory peaked at ${gib(peak)}G$pressure."
    }
    val known = summary.coveredSeconds - summary.unknownSeconds
    val headroom = if (known > 0) " The machine had headroom ${summary.headroomSeconds * 100 / known}% of the time." else ""
    summary.gpuAvg?.let { lines += "GPU averaged ${it.roundToLong()}%.$headroom" }
        ?: headroom.takeIf { it.isNotEmpty() }?.let { lines += it.trim() }
    return lines
}

/** Tooltip lines for one bucket. */
fun bucketTooltip(
    bucket: li.rajeshgo.sm.data.model.UtilizationBucket,
    bucketSeconds: Long,
    zone: ZoneId,
): List<String> {
    val start = parseQueueTime(bucket.start)?.atZoneSameInstant(zone)
    val time = DateTimeFormatter.ofPattern("MMM d HH:mm", Locale.US)
    val range = start?.let { "${it.format(time)} – ${it.plusSeconds(bucketSeconds).format(DateTimeFormatter.ofPattern("HH:mm", Locale.US))}" } ?: bucket.start
    if (bucket.samples == 0) return listOf(range, "No samples (server down)")
    val pressure = when (pressureShade(bucket.pressureMax)) {
        2 -> "critical"
        1 -> "elevated"
        else -> "normal"
    }
    val jobs = listOf("tests", "perf", "background", "service").mapNotNull { type ->
        bucket.running[type]?.takeIf { it > 0 }?.let { String.format(Locale.US, "%s %.1f", type, it) }
    }.ifEmpty { listOf("none") }.joinToString(", ")
    return listOfNotNull(
        range,
        "CPU ${bucket.cpuAvg?.roundToLong() ?: "-"}% avg, ${bucket.cpuMax?.roundToLong() ?: "-"}% max",
        "GPU ${bucket.gpuAvg?.roundToLong() ?: "-"}% avg",
        "Memory ${gib(bucket.memUsedAvg) ?: "-"}G avg, ${gib(bucket.memUsedMax) ?: "-"}G max · pressure $pressure",
        "Jobs running: $jobs",
        bucket.pendingMax?.takeIf { it > 0 }?.let { "Up to $it waiting" },
    )
}
