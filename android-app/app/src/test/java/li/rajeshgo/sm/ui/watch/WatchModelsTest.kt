package li.rajeshgo.sm.ui.watch

import li.rajeshgo.sm.data.model.ClientSession
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
        assertEquals("active", projectedStatusLabel(session))
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
