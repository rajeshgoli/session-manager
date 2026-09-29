package li.rajeshgo.sm.ui.queue

import java.time.OffsetDateTime
import java.time.ZoneOffset
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.HostStatus
import li.rajeshgo.sm.data.model.JobHolding
import li.rajeshgo.sm.data.model.QueueSlots
import li.rajeshgo.sm.data.model.QueueStats
import li.rajeshgo.sm.data.model.QueueTypeStats
import li.rajeshgo.sm.data.model.QueueWaitingGroup
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.data.model.SlotCount
import li.rajeshgo.sm.data.model.UtilizationBucket
import li.rajeshgo.sm.data.model.UtilizationSummary
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class QueueModelsTest {
    private val now = OffsetDateTime.of(2026, 9, 28, 12, 0, 0, 0, ZoneOffset.UTC)

    private fun job(state: String) = SessionJob(
        id = "job_1",
        label = "bench-ledger",
        state = state,
        type = "perf",
        notifySessionId = "abcdef123456",
        timeoutSeconds = 900,
        queuedAt = "2026-09-28T11:57:00Z",
        startedAt = "2026-09-28T11:56:00Z",
        waitDeadlineAt = "2026-09-28T12:02:00Z",
    )

    @Test
    fun durationsReadAsWholeUnits() {
        assertEquals("40s", shortDuration(40))
        assertEquals("5m", shortDuration(300))
        assertEquals("1h", shortDuration(3600))
        assertEquals("1h 20m", shortDuration(4800))
        assertEquals("2d 3h", shortDuration(2 * 86_400 + 3 * 3600))
        assertEquals("0s", shortDuration(-5))
        assertEquals("12 min", hoursLabel(720))
        assertEquals("11.2 h", hoursLabel(40_320))
    }

    @Test
    fun naiveServerTimesAreUtc() {
        assertEquals(now, parseQueueTime("2026-09-28T12:00:00"))
        assertEquals(now, parseQueueTime("2026-09-28T12:00:00Z"))
        assertEquals(null, parseQueueTime("soon"))
    }

    @Test
    fun rowLinesMatchTheMockup() {
        assertEquals("perf · 4m of 15m", runningLine(job("running"), now))
        assertEquals(4f / 15f, runningProgress(job("running"), now), 0.001f)
        assertEquals("perf · waiting 3m · gives up in 2m", queuedLine(job("pending"), now))
        assertEquals("perf · waiting 6m · giving up", queuedLine(job("pending"), now.plusMinutes(3)))
        assertEquals("abcdef12", jobAgentLabel(job("pending")))
        assertEquals("sm-1196", jobAgentLabel(job("pending").copy(notifyName = "sm-1196")))
        assertEquals("no agent", jobAgentLabel(job("pending").copy(notifySessionId = null)))
        assertEquals("Waiting for a free slot", queuedReason(job("pending").copy(holdingReason = "concurrency_cap")))
        assertEquals("Waiting for 2 tests", queuedReason(job("pending").copy(holding = JobHolding(summary = "Waiting for 2 tests"))))
        assertEquals(
            "background · 40m ago",
            endedLine(job("displaced").copy(type = "background", finishedAt = "2026-09-28T11:20:00Z"), now),
        )
        assertEquals("⌛", endedIcon("gave_up"))
        assertEquals("✕", endedIcon("displaced"))
    }

    @Test
    fun slotsAndMetersFollowTheServer() {
        val slots = QueueSlots(
            running = 3,
            max = 8,
            byType = mapOf("tests" to SlotCount(2, 6), "perf" to SlotCount(0, 1), "background" to SlotCount(1, 2), "service" to SlotCount(0, 2)),
        )
        assertEquals("3 of 8 slots in use · tests 2/6 · perf 0/1 · background 1/2 · service 0/2", slotsLine(slots))
        val gib = 1024L * 1024 * 1024
        val rows = meterRows(HostStatus(available = true, memoryTotalBytes = 256 * gib, memoryUsedBytes = 87 * gib, memoryPressure = "Elevated", cpuPercent = 72.4, gpuPercent = 11.0))
        assertEquals(listOf("MEM", "CPU", "GPU"), rows.map { it.label })
        assertEquals("87/256G", rows[0].value)
        assertEquals("Elevated", rows[0].warning)
        assertEquals("72%", rows[1].value)
        assertEquals(Meter.LOW, meterBand(0.59))
        assertEquals(Meter.MID, meterBand(0.60))
        assertEquals(Meter.HIGH, meterBand(0.86))
        assertTrue(meterRows(HostStatus(available = false)).isEmpty())
    }

    @Test
    fun heldBackCardMatchesTheWorkedExample() {
        val stats = QueueStats(
            available = true,
            waiting = listOf(
                QueueWaitingGroup("limits", 120, 100, 0),
                QueueWaitingGroup("perf_rules", 0, 0, 0),
                QueueWaitingGroup("memory", 0, 0, 0),
            ),
            byType = listOf(QueueTypeStats(type = "background", jobs = 4, peakRssP95Bytes = 18L * 1024 * 1024 * 1024)),
        )
        assertEquals(
            listOf(
                "Limits held jobs 2 min. The machine had headroom for 83% of it.",
                "Background jobs peak at 18G each (95th percentile).",
            ),
            heldBackLines(stats),
        )
        assertEquals(emptyList<String>(), heldBackLines(QueueStats(available = false)))
        assertEquals(listOf("Nothing waited in this window."), heldBackLines(QueueStats(available = true)))
    }

    @Test
    fun askAgentPromptNamesTheJobAndHowLong() {
        assertEquals(
            "Rajesh is asking from the sm app about your queue job bench-ledger (ID job_1, type perf, running for 4m): How long?",
            askAgentPrompt("Rajesh", job("running"), "  How long? ", now),
        )
        assertEquals(
            "Rajesh is asking from the sm app about your queue job bench-ledger (ID job_1, type perf, waiting 3m): Why?",
            askAgentPrompt("Rajesh", job("pending"), "Why?", now),
        )
    }

    @Test
    fun badgeCountsDistinctWaitingJobs() {
        val waiting = job("pending")
        fun session(id: String, jobs: List<SessionJob>) = ClientSession(
            id = id,
            name = id,
            workingDir = "/tmp",
            status = "running",
            createdAt = "2026-09-28T11:00:00Z",
            lastActivity = "2026-09-28T11:00:00Z",
            tmuxSession = id,
            jobs = jobs,
        )
        val sessions = listOf(
            session("a", listOf(waiting, job("running").copy(id = "r"))),
            session("b", listOf(waiting, waiting.copy(id = "job_2"))),
        )
        assertEquals(2, waitingJobCount(sessions))
    }

    @Test
    fun usageAxisLabelsLandOnBoundariesAndEndAtNow() {
        val starts = (0 until 288).map { now.minusHours(24).plusMinutes(5L * it).toString() }
        val labels = usageAxisLabels(starts, 24, ZoneOffset.UTC)
        assertEquals("now", labels.last().second)
        assertEquals(287, labels.last().first)
        assertEquals(listOf("12:00", "18:00", "00:00", "06:00"), labels.dropLast(1).map { it.second })
        assertEquals(15L, usageRefreshSeconds(1))
        assertEquals(60L, usageRefreshSeconds(24))
        assertEquals(null, usageRefreshSeconds(168))
    }

    @Test
    fun gapsBreakLinesAndPressureShades() {
        assertEquals(
            listOf(listOf(0 to 1.0, 1 to 2.0), listOf(3 to 4.0)),
            plotRuns(listOf(1.0, 2.0, null, 4.0)),
        )
        assertEquals(0, pressureShade(null))
        assertEquals(0, pressureShade(1))
        assertEquals(1, pressureShade(2))
        assertEquals(2, pressureShade(4))
    }

    @Test
    fun usageSummaryOmitsZeroClauses() {
        val summary = UtilizationSummary(
            coveredSeconds = 86_400,
            cpuAvg = 41.2,
            cpuBusySeconds = 8280,
            gpuAvg = 9.0,
            memUsedMax = 241L * 1024 * 1024 * 1024,
            pressureElevatedSeconds = 0,
            headroomSeconds = 61_344,
            unknownSeconds = 0,
        )
        assertEquals(
            listOf(
                "CPU averaged 41%, over 85% busy for 2.3 h.",
                "Memory peaked at 241G.",
                "GPU averaged 9%. The machine had headroom 71% of the time.",
            ),
            usageSummaryLines(summary),
        )
        val tooltip = bucketTooltip(UtilizationBucket(start = "2026-09-28T11:00:00Z", samples = 0), 300, ZoneOffset.UTC)
        assertEquals(listOf("Sep 28 11:00 – 11:05", "No samples (server down)"), tooltip)
    }
}
