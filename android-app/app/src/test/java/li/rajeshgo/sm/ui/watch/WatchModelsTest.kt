package li.rajeshgo.sm.ui.watch

import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.data.model.WhatRequestRecord
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class WatchModelsTest {
    @Test
    fun retireSessionCopyUsesRetireLanguage() {
        assertEquals("Retire", RETIRE_SESSION_ACTION_LABEL)
        assertEquals("Sign in to retire sessions", SIGN_IN_TO_RETIRE_SESSIONS_MESSAGE)
        assertFalse(RETIRE_SESSION_ACTION_LABEL.contains("kill", ignoreCase = true))
        assertFalse(SIGN_IN_TO_RETIRE_SESSIONS_MESSAGE.contains("kill", ignoreCase = true))
    }

    @Test
    fun projectedStatusTreatsWorkingIdleSessionAsActive() {
        val session = session(status = "idle", activityState = "working")

        assertTrue(isOperationallyActive(session))
        assertTrue(isActiveSession(session))
        assertEquals("working", projectedStatusLabel(session))
        assertEquals(SessionVisualState.Active, sessionVisualState(session))
    }

    @Test
    fun backgroundWaitIsLabelledDistinctlyFromWaitingOnAHuman() {
        assertEquals("bg-wait", activityLabel("waiting"))
        assertEquals("waiting", activityLabel("waiting_input"))
        assertEquals("waiting", activityLabel("waiting_permission"))
    }

    @Test
    fun backgroundWaitCountsAsOperationallyActive() {
        val session = session(status = "idle", activityState = "waiting")

        assertTrue(isOperationallyActive(session))
        assertEquals("bg-wait", projectedStatusLabel(session))
        assertEquals(SessionVisualState.Active, sessionVisualState(session))
    }

    @Test
    fun allOperationalActivityStatesUseActiveVisualTreatment() {
        listOf("working", "thinking", "waiting", "waiting_input", "waiting_permission")
            .forEach { activity ->
                val session = session(status = "running", activityState = activity)

                assertTrue(activity, isOperationallyActive(session))
                assertEquals(activity, SessionVisualState.Active, sessionVisualState(session))
            }
    }

    @Test
    fun runningFilterIncludesOperationallyActiveIdleSession() {
        val session = session(status = "idle", activityState = "working")
        val sections = buildSections(listOf(session))

        val runningSections = filterSections(sections, statusFilter = "running", query = "")
        val idleSections = filterSections(sections, statusFilter = "idle", query = "")

        assertEquals(listOf(session.id), runningSections.flatMap { it.roots }.map { it.session.id })
        assertTrue(idleSections.isEmpty())
    }

    @Test
    fun idleSessionStillProjectsAsIdle() {
        val session = session(status = "idle", activityState = "idle")

        assertFalse(isOperationallyActive(session))
        assertFalse(isActiveSession(session))
        assertEquals("idle", projectedStatusLabel(session))
        assertEquals(SessionVisualState.Inactive, sessionVisualState(session))
    }

    @Test
    fun runningSessionWithIdleActivityProjectsAsIdle() {
        val session = session(status = "running", activityState = "idle")
        val sections = buildSections(listOf(session))

        assertFalse(isOperationallyActive(session))
        assertFalse(isActiveSession(session))
        assertEquals("idle", projectedStatusLabel(session))
        assertEquals(SessionVisualState.Inactive, sessionVisualState(session))

        val runningSections = filterSections(sections, statusFilter = "running", query = "")
        val idleSections = filterSections(sections, statusFilter = "idle", query = "")

        assertTrue(runningSections.isEmpty())
        assertEquals(listOf(session.id), idleSections.flatMap { it.roots }.map { it.session.id })
    }

    @Test
    fun stoppedStatusWinsOverWorkingActivityVisualTreatment() {
        val session = session(status = "stopped", activityState = "working")

        assertEquals(SessionVisualState.Stopped, sessionVisualState(session))
    }

    @Test
    fun whatUiStateTracksTerminalServerRecord() {
        val initial = WhatUiState(
            targetSessionId = "sess-1",
            targetName = "agent",
            status = "pending",
            activeMode = WhatRequestMode.Full,
        )
        val completed = initial.withRecord(
            WhatRequestRecord(
                requestId = "btw-1",
                targetSessionId = "sess-1",
                targetProvider = "codex-fork",
                status = "completed",
                createdAt = "2026-07-29T00:00:00Z",
                finishedAt = "2026-07-29T00:00:02Z",
                result = "Current summary",
            )
        )

        assertTrue(completed.isTerminal())
        assertEquals("Current summary", completed.entries.single().markdown)
        assertFalse(completed.entries.single().isUpdate)
        assertEquals("btw-1", completed.requestId)
    }

    @Test
    fun whatUpdateAppendsChronologicallyAndPersists() {
        val existing = WhatUiState(
            targetSessionId = "sess-1",
            targetName = "agent",
            entries = listOf(
                WhatSummaryEntry("btw-1", "# Initial\n\n- Built API", "2026-07-29T00:00:00Z", false)
            ),
            status = "running",
            activeMode = WhatRequestMode.Update,
        )
        val updated = existing.withRecord(
            WhatRequestRecord(
                requestId = "btw-2",
                targetSessionId = "sess-1",
                targetProvider = "codex-fork",
                status = "completed",
                createdAt = "2026-07-29T00:10:00Z",
                finishedAt = "2026-07-29T00:10:02Z",
                result = "## Changed\n\n- Added tests",
            )
        )
        val restored = updated.toPersisted().toUiState()

        assertEquals(listOf("btw-1", "btw-2"), updated.entries.map { it.requestId })
        assertTrue(updated.entries.last().isUpdate)
        assertEquals(updated.entries, restored.entries)
        assertEquals("completed", restored.status)
    }

    @Test
    fun successfulRegenerationReplacesHistoryButFailureKeepsIt() {
        val existingEntry = WhatSummaryEntry(
            requestId = "btw-1",
            markdown = "# Existing",
            createdAt = "2026-07-29T00:00:00Z",
            isUpdate = false,
        )
        val regenerating = WhatUiState(
            targetSessionId = "sess-1",
            targetName = "agent",
            entries = listOf(existingEntry),
            status = "running",
            activeMode = WhatRequestMode.Full,
        )

        val failed = regenerating.withRecord(
            WhatRequestRecord(
                requestId = "btw-2",
                targetSessionId = "sess-1",
                targetProvider = "claude",
                status = "failed",
                createdAt = "2026-07-29T00:10:00Z",
                error = "unavailable",
            )
        )
        val completed = regenerating.withRecord(
            WhatRequestRecord(
                requestId = "btw-3",
                targetSessionId = "sess-1",
                targetProvider = "claude",
                status = "completed",
                createdAt = "2026-07-29T00:11:00Z",
                finishedAt = "2026-07-29T00:11:03Z",
                result = "# Fresh",
            )
        )

        assertEquals(listOf(existingEntry), failed.entries)
        assertEquals(listOf("# Fresh"), completed.entries.map { it.markdown })
    }

    @Test
    fun updatePromptIsSingleLineAndWithinServerByteLimit() {
        val prompt = buildWhatUpdatePrompt(
            listOf(
                WhatSummaryEntry(
                    requestId = "btw-1",
                    markdown = "# Summary\n\n" + "🌐 changed `code`\n".repeat(2_000),
                    createdAt = "2026-07-29T00:00:00Z",
                    isUpdate = false,
                ),
                WhatSummaryEntry(
                    requestId = "btw-2",
                    markdown = "## LATEST CHANGE\n\n- Kept the newest update",
                    createdAt = "2026-07-29T00:10:00Z",
                    isUpdate = true,
                ),
            )
        )

        assertTrue(prompt.startsWith("Summarize only what has changed"))
        assertFalse(prompt.contains('\n'))
        assertTrue(prompt.toByteArray().size <= 4 * 1024)
        assertTrue(prompt.contains("LATEST CHANGE"))
    }

    @Test
    fun compactDetailOmitsLowValueMetadataAndReplacesTailWhenSummaryExists() {
        val session = session(status = "running", activityState = "working")
        val detail = SessionDetail(
            actionLines = listOf("exec", "read"),
            tailLines = listOf("clean tail"),
        )

        val fallback = detailLines(session, detail, hasSummary = false)
        val summarized = detailLines(session, detail, hasSummary = true)

        assertTrue(fallback.contains("last 10 tail lines:"))
        assertTrue(fallback.contains("clean tail"))
        assertFalse(summarized.any { it.contains("tail lines") || it.contains("clean tail") })
        assertFalse((fallback + summarized).any {
            it.startsWith("tmux:") ||
                it.startsWith("git remote:") ||
                it.startsWith("aliases:") ||
                it.startsWith("current task:")
        })
    }

    @Test
    fun contextPercentageUsesCompactSmContextFormatting() {
        assertEquals("43%", formatContextPercentage(43.0))
        assertEquals("75.2%", formatContextPercentage(75.237))
        assertEquals(null, formatContextPercentage(null))
    }

    private fun session(
        id: String = "sess-1",
        status: String,
        activityState: String,
    ): ClientSession {
        return ClientSession(
            id = id,
            name = "maintainer",
            workingDir = "/tmp/project",
            status = status,
            createdAt = "2026-04-15T10:00:00Z",
            lastActivity = "2026-04-15T10:05:00Z",
            tmuxSession = "codex-$id",
            provider = "codex",
            activityState = activityState,
        )
    }
}
