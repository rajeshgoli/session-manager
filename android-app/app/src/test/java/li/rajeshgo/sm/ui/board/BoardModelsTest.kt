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

    @Test fun blockedFoldShowsThreeWithNewMarks() {
        val base = "o/r"
        val tickets = listOf(
            BoardTicket(repo = base, number = 1763),
            BoardTicket(repo = base, number = 1805, new = true),
            BoardTicket(repo = base, number = 1766),
            BoardTicket(repo = base, number = 1774),
        )
        assertEquals("#1763 #1805 NEW #1766 …", blockedPreview(tickets, base))
        assertEquals("#1763", blockedPreview(tickets.take(1), base))
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

    /** 1844-engineer idle on 29 Sep: its one job 6th to start, 1854's two running. */
    @Test fun agentQueueSummarisesRunningAndWaitingJobs() {
        val now = OffsetDateTime.parse("2026-09-30T00:23:00Z")
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
        val idle = agentQueue(queue, "e1844", now)!!
        assertEquals("queue: 2 waiting 1h 29m, #6 to start", agentQueueText(idle))
        assertEquals("queue: 2 running 1h 33m · 1 waiting 2h 21m, #1 to start", agentQueueText(agentQueue(queue, "e1854", now)!!))
        assertNull(agentQueue(queue, "nobody", now))
        assertNull(agentQueue(null, "e1844", now))

        val lane = board.lanes.single().copy(
            tickets = listOf(
                BoardTicket(number = 1844, holder = BoardHolder(sessionId = "e1844")),
                BoardTicket(number = 1854, holder = BoardHolder(sessionId = "e1854")),
            ),
        )
        assertEquals("Queue: 2 running · 3 waiting", laneQueueLine(lane, queue))
        assertNull(laneQueueLine(board.lanes.single().copy(tickets = emptyList()), queue))
    }
}
