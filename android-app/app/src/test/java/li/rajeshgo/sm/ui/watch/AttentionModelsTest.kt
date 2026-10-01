package li.rajeshgo.sm.ui.watch

import java.time.OffsetDateTime
import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.SessionClaim
import li.rajeshgo.sm.data.model.SessionObligations
import li.rajeshgo.sm.data.model.WatchStateResponse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Watch by attention (spec 1782 J2), drawn from the appendix B worked examples. */
class AttentionModelsTest {
    private val now = OffsetDateTime.parse("2026-09-30T19:47:00Z")

    /** `/watch/state` as the server writes it for the B fixtures, joined onto `/client/sessions` rows. */
    private val watchState = Json { ignoreUnknownKeys = true }.decodeFromString(
        WatchStateResponse.serializer(),
        """
        {"schema_version":1,"counts":{"needs_you":1},"sessions":[
          {"id":"s1726","name":"sm-1726-engineer","facts":{
             "agent":{"state":"idle","since":"2026-09-30T19:40:16Z"},
             "jobs":{"running":0,"waiting":0,"quiet":false,"review":null,"tone":null,"text":"No jobs"},
             "you":{"kind":"message","since":"2026-09-30T19:40:12Z","text":"1771 is waiting for one manual Chrome check","more":0,"message_ids":["msg_1"],"dismissible":true},
             "finished":null},
           "attention":{"section":"you","reason":"message","order_key":"2026-09-30T19:40:12Z"}},
          {"id":"iter8","name":"iter8-run","facts":{
             "agent":{"state":"idle","since":"2026-09-30T19:42:42Z"},
             "jobs":{"running":2,"waiting":0,"quiet":false,"tone":"green","text":"2 running · 2h 56m"},
             "you":null,"finished":null},
           "attention":{"section":"moving","reason":null,"order_key":"8240297838"}},
          {"id":"s1776","name":"sm-1776","facts":{
             "agent":{"state":"working","since":"2026-09-30T19:46:00Z"},
             "jobs":{"running":0,"waiting":0,"tone":null,"text":"No jobs"}},
           "attention":{"section":"moving","reason":null,"order_key":"8240297500"}},
          {"id":"far1855","name":"far-1855","facts":{
             "agent":{"state":"idle","since":"2026-09-30T15:01:43Z"},
             "jobs":{"running":0,"waiting":0,"tone":null,"text":"No jobs"},
             "finished":{"at":"2026-09-30T15:01:00Z","text":"1855 done and closed: 68 views\nDetails follow.","read":false}},
           "attention":{"section":"finished","reason":null,"order_key":"8240283139"}},
          {"id":"s1768","name":"sm-1768","facts":{
             "agent":{"state":"idle","since":"2026-09-30T19:36:39Z"},
             "jobs":{"running":0,"waiting":0,"tone":null,"text":"No jobs"}},
           "attention":{"section":"idle","reason":null,"order_key":"18240297000"}},
          {"id":"queued","name":"queued","facts":{
             "agent":{"state":"idle","since":"2026-09-30T18:00:00Z"},
             "jobs":{"running":0,"waiting":1,"tone":"amber","text":"Waiting 1h 2m · 3rd in line"}},
           "attention":{"section":"waiting_long","reason":"queue_wait","order_key":"2026-09-30T18:45:00Z"}}
        ]}
        """.trimIndent(),
    ).sessions.associateBy { it.id }

    private fun agent(id: String, name: String, ticket: Long? = null, status: String = "running"): ClientSession {
        val claims = listOfNotNull(ticket?.let { SessionClaim(kind = "ticket", repo = "rajeshgoli/session-manager", number = it) })
        return ClientSession(
            id = id,
            name = name,
            workingDir = "/work/session-manager",
            status = status,
            createdAt = "2026-09-30T10:00:00Z",
            lastActivity = "2026-09-30T19:40:00Z",
            tmuxSession = id,
            obligations = SessionObligations(id, claims = claims),
            facts = watchState[id]?.facts,
            attention = watchState[id]?.attention,
        )
    }

    private val sessions = listOf(
        agent("s1768", "sm-1768"),
        agent("iter8", "iter8-run", ticket = 1858),
        agent("far1855", "far-1855", ticket = 1855),
        agent("queued", "queued"),
        agent("s1776", "sm-1776", ticket = 1776),
        agent("s1726", "sm-1726-engineer", ticket = 1771),
        agent("gone", "gone-agent", status = "stopped"),
        agent("unlisted", "unlisted"),
    )

    @Test
    fun sectionsFollowTheServerOrderThenOrderKeyThenName() {
        val groups = attentionGroups(sessions, "all", "")
        assertEquals(
            listOf("you", "finished", "waiting_long", "moving", "idle", "stopped"),
            groups.map { it.section },
        )
        assertEquals("NEEDS YOU · 1", groups.first().label)
        // Moving sorts by order key: sm-1776's key is the smaller.
        assertEquals(listOf("sm-1776", "iter8-run"), groups.single { it.section == "moving" }.sessions.map { it.name })
        // An agent /watch/state did not list is idle; its empty key sorts first.
        assertEquals(listOf("unlisted", "sm-1768"), groups.single { it.section == "idle" }.sessions.map { it.name })
        assertEquals(listOf("gone-agent"), groups.single { it.section == "stopped" }.sessions.map { it.name })
    }

    @Test
    fun searchAndStatusFilterNarrowTheSections() {
        assertEquals(listOf("finished"), attentionGroups(sessions, "all", "far-").map { it.section })
        assertEquals(listOf("stopped"), attentionGroups(sessions, "stopped", "").map { it.section })
    }

    @Test
    fun rowsShowTheThreeFactsOfTheWorkedExamples() {
        val byId = sessions.associateBy { it.id }
        val needsYou = byId.getValue("s1726")
        assertEquals("#1771 · ○ Idle 6m · No jobs", rowFactsLine(needsYou, now))
        assertEquals("◆ 6m: 1771 is waiting for one manual Chrome check", youLine(needsYou, now))

        val iter8 = byId.getValue("iter8")
        assertEquals("#1858 · ○ Idle 4m · 2 running · 2h 56m", rowFactsLine(iter8, now))
        assertNull(youLine(iter8, now))
        assertNull(finishedLine(iter8))

        assertEquals("#1776 · ● Working 1m · No jobs", rowFactsLine(byId.getValue("s1776"), now))

        val far = byId.getValue("far1855")
        assertEquals("#1855 · finished 4h 46m", rowFactsLine(far, now))
        assertEquals("✔ 1855 done and closed: 68 views", finishedLine(far))

        assertEquals("○ Idle 1h 47m · Waiting 1h 2m · 3rd in line", rowFactsLine(byId.getValue("queued"), now))
        assertEquals("■ Stopped · No jobs", rowFactsLine(byId.getValue("gone"), now))
    }

    @Test
    fun youLineCountsMoreQuestionsAndFinishedWaitsForItsText() {
        val base = sessions.single { it.id == "s1726" }
        val two = base.copy(facts = base.facts!!.copy(you = base.facts!!.you!!.copy(more = 1)))
        assertEquals("◆ 6m: 1771 is waiting for one manual Chrome check (+1 more)", youLine(two, now))
        val far = sessions.single { it.id == "far1855" }
        assertEquals("✔ Finishing…", finishedLine(far.copy(facts = far.facts!!.copy(finished = far.facts!!.finished!!.copy(text = null)))))
        assertNull(finishedLine(far.copy(facts = far.facts!!.copy(finished = far.facts!!.finished!!.copy(read = true)))))
    }

    @Test
    fun factAgesUseMinutesUnderAnHourThenHoursAndMinutes() {
        assertEquals("0m", factAge("2026-09-30T19:46:30Z", now))
        assertEquals("59m", factAge("2026-09-30T18:48:00Z", now))
        assertEquals("1h 0m", factAge("2026-09-30T18:47:00Z", now))
        assertNull(factAge(null, now))
    }

    @Test
    fun ticketTitleIsTheTitleOfTheTicketTheRowNumbers() {
        val claims = listOf(
            SessionClaim(kind = "pr", repo = "rajeshgoli/session-manager", number = 1901, title = "Ticket titles on agent cards"),
            SessionClaim(kind = "ticket", repo = "rajeshgoli/session-manager", number = 1900, title = " Show ticket titles next to agent names "),
        )
        val titled = agent("s1900", "sm-1900").copy(obligations = SessionObligations("s1900", claims = claims))
        assertEquals("#1900", ticketLabel(titled))
        assertEquals("Show ticket titles next to agent names", ticketTitle(titled))
        assertNull(ticketTitle(agent("iter8", "iter8-run", ticket = 1858)))
        assertNull(ticketTitle(agent("s1768", "sm-1768")))
    }
}
