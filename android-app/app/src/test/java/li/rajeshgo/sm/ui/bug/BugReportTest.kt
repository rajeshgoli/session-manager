package li.rajeshgo.sm.ui.bug

import java.util.Base64
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import li.rajeshgo.sm.data.model.BoardStarted
import li.rajeshgo.sm.data.model.BugReportFiled
import li.rajeshgo.sm.data.model.BugReportIssue
import li.rajeshgo.sm.data.model.BugReportRequest
import li.rajeshgo.sm.data.model.Reviewer
import li.rajeshgo.sm.data.remote.PageDataRing
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BugReportTest {
    private val json = Json { ignoreUnknownKeys = true }

    @Test
    fun ringKeepsTwelvePathsAndEvictsTheOldest() {
        var now = 0L
        val ring = PageDataRing { now++ }
        (1..13).forEach { ring.record("/p$it", """{"n":$it}""") }
        assertEquals(12, ring.paths().size)
        assertFalse("/p1" in ring.paths())
        // Fetching a path again makes it the newest, so /p3 goes next rather than /p2.
        ring.record("/p2", """{"n":"again"}""")
        ring.record("/p14", "{}")
        assertTrue("/p2" in ring.paths())
        assertFalse("/p3" in ring.paths())
        assertEquals(JsonPrimitive("again"), ring.snapshot(0)["/p2"]!!.jsonObject["n"])
    }

    @Test
    fun snapshotTakesEntriesSinceTheScreenShowedAndSkipsNonJson() {
        var now = 0L
        val ring = PageDataRing { now }
        ring.record("/client/sessions", """{"before":true}""")
        now = 10
        ring.record("/client/board", """{"lanes":[]}""")
        ring.record("/client/text", "not json")
        assertEquals(setOf("/client/board"), ring.snapshot(since = 10).keys)
        assertEquals(setOf("/client/sessions", "/client/board"), ring.snapshot(since = 0).keys)
    }

    @Test
    fun snapshotDropsLargestEntriesOverTheCap() {
        val ring = PageDataRing { 0 }
        val big = "x".repeat(200_000)
        ring.record("/big", """{"s":"$big"}""")
        ring.record("/bigger", """{"s":"${big}yy"}""")
        ring.record("/small", """{"s":"ok"}""")
        val snapshot = ring.snapshot(0)
        assertEquals(setOf("/big", "/small"), snapshot.keys)
        assertTrue(snapshot.toString().length <= PageDataRing.MAX_CHARS)
    }

    @Test
    fun requestCarriesTextPageAndScreenshotOnlyWhileOn() {
        val png = byteArrayOf(0x89.toByte(), 0x50, 0x4E, 0x47)
        val data = JsonObject(mapOf("/client/board" to JsonObject(emptyMap())))
        val reviewer = Reviewer(kind = "run", provider = "codex")
        val filing = BugFiling("  Board shows 'all parts done'\nMore  ", screenshot = true, start = BugAgentChoice("codex-fork", "gpt-6-sol", "medium", reviewer))
        val request = bugReportRequest(filing, png, "Board", "board", data, "1.4.0 (140)")
        assertEquals("Board shows 'all parts done'\nMore", request.text)
        assertEquals("android", request.client)
        assertEquals("1.4.0 (140)", request.clientVersion)
        assertEquals("Board", request.page)
        assertEquals("board", request.route)
        assertEquals(data, request.pageData)
        assertEquals(Base64.getEncoder().encodeToString(png), request.screenshotPng)
        assertEquals("codex-fork", request.start!!.provider)
        assertEquals("gpt-6-sol", request.start!!.model)
        assertEquals("medium", request.start!!.reasoningEffort)
        assertEquals(reviewer, request.start!!.reviewer)

        val off = bugReportRequest(filing.copy(screenshot = false, start = null), png, "Board", "board", data, "v")
        assertNull(off.screenshotPng)
        assertNull(off.start)
        // The wire shape the server reads: snake_case keys, and no start or screenshot when off.
        val wire = json.parseToJsonElement(json.encodeToString(BugReportRequest.serializer(), off)).jsonObject
        assertEquals(setOf("text", "client", "client_version", "page", "route", "page_data"), wire.keys)
        assertNull(bugReportRequest(filing, null, "Board", "board", data, "v").screenshotPng)
    }

    @Test
    fun defaultsFollowSavedNewAgentSettings() {
        val settings = json.parseToJsonElement(
            """{"new_agent":{"provider":"codex-fork","claude":{"model":"claude-opus-5-5","effort":"max"},"codex":{"model":"gpt-6-sol","effort":null}}}""",
        ).jsonObject
        assertEquals(BugAgentDefaults("codex-fork", "gpt-6-sol", "high"), bugAgentDefaults(settings))
        val claude = json.parseToJsonElement("""{"new_agent":{"provider":"claude","claude":{"model":"claude-opus-5-5","effort":"max"}}}""").jsonObject
        assertEquals(BugAgentDefaults("claude", "claude-opus-5-5", "max"), bugAgentDefaults(claude))
        assertEquals(BugAgentDefaults("claude", null, "high"), bugAgentDefaults(null))
    }

    @Test
    fun filedResponseParsesAndNamesTheToast() {
        val filed = json.decodeFromString(
            BugReportFiled.serializer(),
            """{"bug_id":"BR-20261001-041502-3fa9c1",
                "issue":{"repo":"rajeshgoli/session-manager","number":1870,"url":"https://github.com/rajeshgoli/session-manager/issues/1870","title":"Board"},
                "facts_url":null,"on_board":true,"board_note":null,
                "started":{"session_id":"abc","name":"sm-1870-engineer"},"start_error":null}""",
        )
        assertEquals(BugReportIssue("rajeshgoli/session-manager", 1870, "https://github.com/rajeshgoli/session-manager/issues/1870", "Board"), filed.issue)
        assertEquals("Filed #1870 · started sm-1870-engineer", bugFiledText(filed))
        assertEquals("Filed #1870", bugFiledText(filed.copy(started = null)))
        assertEquals("Filed #1870 · not on the board", bugFiledText(filed.copy(started = null, onBoard = false, boardNote = "Bugs lane link refused")))
        assertEquals("Filed #1870 · started sm-x", bugFiledText(filed.copy(started = null), BoardStarted("s", "sm-x")))
    }

    @Test
    fun pageLabelsAreNavLabels() {
        assertEquals("Board", bugPageLabel("board"))
        assertEquals("Analytics", bugPageLabel("analytics?section=queue"))
        assertEquals("Watch", bugPageLabel("watch"))
    }
}
