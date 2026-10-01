package li.rajeshgo.sm.ui.board

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.BoardResponse
import li.rajeshgo.sm.data.model.BoardHolder
import li.rajeshgo.sm.data.model.BoardTicket
import li.rajeshgo.sm.data.model.QueueOverview
import li.rajeshgo.sm.data.model.SessionJob
import java.time.OffsetDateTime
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BoardModelsTest {
    private val json = Json { ignoreUnknownKeys = true }

    /** The shape `GET /client/board` returns (sm#1665 appendix F). */
    private val board = json.decodeFromString(
        BoardResponse.serializer(),
        """{"generated_at":"2026-09-30T00:02:45Z",
           "unseen":{"count":2,"lane_ids":[7]},
           "repos":[{"repo":"rajeshgoli/session-manager","last_ok_at":"2026-09-30T00:02:41Z","stale":false,"error":null}],
           "lanes":[{"id":7,"rank":2,
             "goal":{"repo":"rajeshgoli/session-manager","number":1651,"title":"Context handoff","url":"https://github.com/rajeshgoli/session-manager/issues/1651"},
             "added_at":"2026-09-29T17:00:00Z","added_by_name":"Rajesh",
             "counts":{"needs_you":1,"ready":1,"in_progress":0,"blocked":1,"done":1},
             "unseen":true,
             "longest_chain":[{"repo":"rajeshgoli/session-manager","number":1654},{"repo":"rajeshgoli/session-manager","number":1651}],
             "stale":false,"cycles":[],
             "tickets":[
               {"repo":"rajeshgoli/session-manager","number":1653,"title":"Policy","url":"u","state":"needs_you","done_reason":null,
                "needs_you":{"kind":"review","text":"PR #1700 waits for your review","url":"/docs/session-manager/docs/x.html"},
                "waits_on":[],"holder":{"session_id":"abc123","name":"sm-1653","state":"idle"},"prs":[],
                "chain":3,"on_longest_chain":false,"sub_issues_done":false,"also_in":[{"lane_id":9,"rank":3}],
                "warnings":[],"new":true,"closed_at":null},
               {"repo":"rajeshgoli/session-manager","number":1654,"title":"Handoff","url":"u","state":"ready","done_reason":null,
                "needs_you":null,"waits_on":[{"repo":"rajeshgoli/session-manager","number":1652,"state":"done"}],"holder":null,
                "prs":[{"repo":"rajeshgoli/session-manager","number":1699,"state":"MERGED","url":"p"}],
                "chain":2,"on_longest_chain":true,"sub_issues_done":false,"also_in":[],
                "warnings":["merged_not_closed"],"new":false,"closed_at":null},
               {"repo":"rajeshgoli/session-manager","number":1651,"title":"Context handoff","url":"u","state":"blocked","done_reason":null,
                "needs_you":null,"waits_on":[{"repo":"rajeshgoli/fractal-algo-rust","number":1774,"state":"in_progress"}],"holder":null,"prs":[],
                "chain":1,"on_longest_chain":true,"sub_issues_done":false,"also_in":[],"warnings":[],"new":false,"closed_at":null},
               {"repo":"rajeshgoli/session-manager","number":1652,"title":"Done one","url":"u","state":"done","done_reason":"not_planned",
                "needs_you":null,"waits_on":[],"holder":null,"prs":[],"chain":null,"on_longest_chain":false,"sub_issues_done":false,
                "also_in":[],"warnings":[],"new":false,"closed_at":"2026-09-29T10:00:00Z"}],
             "changes":[{"ts":"2026-09-29T18:00:00Z","kind":"ticket_joined","text":"#1653 joined, added by GitHub"}]}],
           "other":[{"repo":"rajeshgoli/session-manager","tickets":[{"repo":"rajeshgoli/session-manager","number":1700,"title":"Loose","url":"u",
             "state":"ready","done_reason":null,"needs_you":null,"waits_on":[],"holder":null,"prs":[],"sub_issues_done":false,"warnings":[],"closed_at":null}]}],
           "start_defaults":{"provider":"claude","model":null,"reasoning_effort":"high"}}""",
    )

    @Test fun parsesTheServerBoard() {
        assertEquals(2, board.unseen.count)
        assertEquals(listOf(7L), board.unseen.laneIds)
        val lane = board.lanes.single()
        assertTrue(lane.unseen)
        assertEquals(2, lane.rank)
        assertEquals(1651L, lane.goal.number)
        val needs = lane.tickets.first()
        assertEquals("review", needs.needsYou?.kind)
        assertEquals("sm-1653", needs.holder?.name)
        assertEquals(9L, needs.alsoIn.single().laneId)
        assertEquals("not_planned", lane.tickets.last().doneReason)
        val loose = board.other.single().tickets.single()
        assertNull(loose.chain)
        assertFalse(loose.new)
        assertNull(board.startDefaults.model)
        assertEquals("high", board.startDefaults.reasoningEffort)
    }

    @Test fun startShowsOnReadyRowsWithoutAMergedPr() {
        val tickets = board.lanes.single().tickets
        assertFalse(boardCanStart(tickets[0]))
        assertFalse(boardCanStart(tickets[1]))
        assertTrue(boardCanStart(board.other.single().tickets.single()))
        assertEquals("PR merged — close the ticket", boardWarningText(tickets[1].warnings.single()))
    }

    @Test fun laneLinesMatchThePhoneMock() {
        val lane = board.lanes.single()
        assertEquals("1 needs you · 1 ready · 0 in progress · 1 blocked · chain 2", laneCountsLine(lane))
        assertEquals("#1654", boardShortRef("rajeshgoli/session-manager", 1654, lane.goal.repo))
        assertEquals("fractal-algo-rust#1774", boardShortRef("rajeshgoli/fractal-algo-rust", 1774, lane.goal.repo))
    }

    @Test fun boardPathsNameTheirLane() {
        assertEquals(7L, boardLaneFromPath("/board#lane-7"))
        assertEquals(0L, boardLaneFromPath("/board"))
        assertEquals(0L, boardLaneFromPath("/board?x=1"))
        assertNull(boardLaneFromPath("/boards"))
        assertNull(boardLaneFromPath("/docs/board"))
        assertEquals(3L, boardLaneForLink("https://sm.example.com/board#lane-3", "sm.example.com"))
        assertNull(boardLaneForLink("https://other.example.com/board#lane-3", "sm.example.com"))
        assertNull(boardLaneForLink("https://sm.example.com/history", "sm.example.com"))
    }

    /** 29 Sep: 1844-engineer waits on two jobs, 1854 runs two and waits on one. */
    @Test fun laneQueueLineCountsItsAgentsJobs() {
        val queue = QueueOverview(
            running = listOf(
                SessionJob(id = "a", requesterSessionId = "e1854", startedAt = "2026-09-29T22:50:00Z"),
                SessionJob(id = "b", requesterSessionId = "e1854", startedAt = "2026-09-29T23:16:00Z"),
            ),
            queued = listOf(
                SessionJob(id = "c", requesterSessionId = "e1854", queuedAt = "2026-09-29T22:02:00Z", position = 1),
                SessionJob(id = "d", requesterSessionId = "e1844", queuedAt = "2026-09-29T22:54:00Z", position = 6),
                // Jobs count for the agent they notify, as the Queue tab assigns them.
                SessionJob(id = "e", requesterSessionId = "helper", notifySessionId = "e1844", queuedAt = "2026-09-29T23:30:00Z", position = 7),
            ),
        )
        val lane = board.lanes.single().copy(
            tickets = listOf(
                BoardTicket(number = 1844, holder = BoardHolder(sessionId = "e1844")),
                BoardTicket(number = 1854, holder = BoardHolder(sessionId = "e1854")),
            ),
        )
        assertEquals("Queue: 2 running · 3 waiting", laneQueueLine(lane, queue))
        assertNull(laneQueueLine(board.lanes.single().copy(tickets = emptyList()), queue))
    }

    private fun ticket(number: Long, state: String, closedAt: String? = null, blockers: List<Long> = emptyList()) = BoardTicket(
        repo = "o/r",
        number = number,
        state = state,
        closedAt = closedAt,
        waitsOn = blockers.map { li.rajeshgo.sm.data.model.BoardWaitsOn("o/r", it, "in_progress") },
    )

    /** Spec 1782 H1: blocked rows up to 12 non-done tickets, three done rows, short lanes. */
    @Test fun laneGroupsFollowTheVisibilityRules() {
        val active = listOf(ticket(1, "needs_you"), ticket(2, "close_ready"))
        val blocked = (10L..19L).map { ticket(it, "blocked", blockers = listOf(1)) }
        val done = listOf(
            ticket(30, "done", "2026-09-28T10:00:00Z"),
            ticket(31, "done", "2026-09-30T10:00:00Z"),
            ticket(32, "done", "2026-09-29T10:00:00Z"),
            ticket(33, "done", "2026-09-27T10:00:00Z"),
        )
        val groups = laneGroups(active + blocked + done)
        assertEquals(listOf(1L, 2L), groups.active.map { it.number })
        assertTrue("12 non-done tickets stay rows", groups.showBlocked)
        assertFalse(groups.short)
        assertEquals(listOf(31L, 32L, 30L, 33L), groups.done.map { it.number })
        val crowded = laneGroups(active + blocked + ticket(20, "blocked", blockers = listOf(2)))
        assertFalse("13 non-done tickets fold the blocked ones", crowded.showBlocked)
        assertEquals("11 blocked · waiting on #1, #2", blockedFoldCaption(crowded.blocked, "o/r"))
        assertTrue(laneGroups(listOf(ticket(1, "in_progress")) + blocked).short)
    }

    @Test fun otherTicketsKeepNeedsYouAndTenMore() {
        val tickets = (1L..12L).map { ticket(it, "ready") } + ticket(40, "needs_you") + ticket(41, "needs_you")
        val shown = visibleOther(tickets)
        assertEquals(10, shown.size)
        assertEquals(listOf(40L, 41L), shown.take(2).map { it.number })
        assertEquals((1L..8L).toList(), shown.drop(2).map { it.number })
    }

    @Test fun startAnywayNamesWhatIsNotDone() {
        assertEquals("#1777 waits on #1776, which is not done.", startAnywayText(ticket(1777, "blocked", blockers = listOf(1776)), "o/r"))
        assertEquals("#1778 waits on #1776 and #1777, which are not done.", startAnywayText(ticket(1778, "blocked", blockers = listOf(1776, 1777)), "o/r"))
        assertEquals(
            "#1779 waits on #1776, #1777 and #1778, which are not done.",
            startAnywayText(ticket(1779, "blocked", blockers = listOf(1776, 1777, 1778)), "o/r"),
        )
        val blocked = ticket(1, "blocked", blockers = listOf(2))
        assertTrue(boardCanStartAnyway(blocked))
        assertFalse(boardCanStartAnyway(blocked.copy(warnings = listOf("cycle"))))
        assertFalse(boardCanStartAnyway(blocked.copy(warnings = listOf("stale"))))
        assertFalse(boardCanStartAnyway(ticket(3, "ready")))
    }

    /** The fields S3 added to the ticket JSON, and the Links line they make (spec 1782 H2-H4). */
    @Test fun containerTicketsAndLinksParse() {
        val parsed = json.decodeFromString(
            BoardTicket.serializer(),
            """{"repo":"o/r","number":1782,"title":"Fit and finish","url":"u","state":"close_ready",
               "sub_issues":{"total":14,"done":14},"started_early":true,
               "holder":{"session_id":"df9fec5a","name":"iter8-run","provider":"claude","since":"2026-09-30T11:55:00Z","state":"idle"},
               "prs":[{"repo":"o/r","number":1785,"state":"OPEN","url":"p",
                 "review":{"by":"you","round":3,"verdict":null,"verdict_at":null,"waiting_since":"2026-09-30T11:56:00Z"}}],
               "jobs":[{"id":"j1","label":"1858-run","state":"running","type":"background","since":"2026-09-30T10:41:00Z","quiet_since":null},
                       {"id":"j2","label":"b","state":"waiting","type":"tests","since":"2026-09-30T11:58:00Z","quiet_since":null},
                       {"id":"j3","label":"c","state":"quiet","type":"tests","since":"2026-09-30T09:00:00Z","quiet_since":"2026-09-30T11:50:00Z"},
                       {"id":"j4","label":"d","state":"running","type":"tests","since":"2026-09-30T11:00:00Z","quiet_since":null}],
               "thread":{"key":"agent:df9fec5a","needs_you":true,"count":2,"at":"m1"},
               "docs":[{"title":"Memo","reader_path":"/docs/x","state":"new"}]}""",
        )
        val now = OffsetDateTime.parse("2026-09-30T12:00:00Z")
        assertEquals(14, parsed.subIssues.done)
        assertTrue(parsed.startedEarly)
        val pr = parsed.prs.single()
        assertEquals("PR #1785 · open · your review, round 3 · waiting 4m", li.rajeshgo.sm.ui.links.prChipText(pr, now))
        assertEquals(li.rajeshgo.sm.ui.theme.Fuchsia, li.rajeshgo.sm.ui.links.prChipColor(pr))
        assertEquals("iter8-run · Claude · ○ Idle 5m", li.rajeshgo.sm.ui.links.agentChipText(parsed.holder!!, now))
        val chips = li.rajeshgo.sm.ui.links.jobChips(parsed.jobs, now) {}
        assertEquals(
            listOf("1858-run · running 1h 19m", "b · waiting 2m", "c · quiet 10m", "+1 jobs"),
            chips.map { it.text },
        )
        // A trailing lambda is the chip's click, never its ⌨.
        assertTrue(chips.all { it.onClick != null && it.onTerminal == null })
        assertEquals("Inbox · question", li.rajeshgo.sm.ui.links.threadChipText(parsed.thread!!))
        assertEquals("Inbox · 2", li.rajeshgo.sm.ui.links.threadChipText(parsed.thread!!.copy(needsYou = false)))
        assertEquals("/docs/x", parsed.docs.single().readerPath)
    }
}
