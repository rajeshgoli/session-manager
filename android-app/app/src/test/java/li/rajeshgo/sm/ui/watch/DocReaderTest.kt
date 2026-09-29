package li.rajeshgo.sm.ui.watch

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.SessionClaim
import li.rajeshgo.sm.data.model.SessionDoc
import li.rajeshgo.sm.data.model.SessionObligationsResponse
import android.webkit.WebViewClient
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class DocReaderTest {
    private val json = Json { ignoreUnknownKeys = true }
    private val server = "https://sm-app.example.com"
    private val sha = "856f0d6e1a2b3c4d5e6f708192a3b4c5d6e7f809"

    private fun doc(readerPath: String? = null, browserUrl: String? = null) = SessionDoc(
        title = "Ticket memo",
        state = "new",
        repo = "rajeshgoli/fractal-algo-rust",
        path = "docs/working/my memo#1.html",
        latestCommitSha = sha,
        readerPath = readerPath,
        browserUrl = browserUrl,
    )

    @Test fun obligationsWithoutDocsStillParse() {
        val body = """{"sessions":[{"session_id":"abc12345","waiting_on":[],"review_history":[]}],"schema_version":1}"""
        val session = json.decodeFromString(SessionObligationsResponse.serializer(), body).sessions.single()
        assertTrue(session.docs.isEmpty())
    }

    @Test fun schemaThreeObligationsWithClaimsStillParse() {
        val body = """
            {"schema_version":3,"sessions":[{"session_id":"abc12345","waiting_on":[],"review_history":[],"docs":[],
              "claims":[{"kind":"ticket","repo":"rajeshgoli/session-manager","number":1452,"title":"Agent work claims",
                         "state":"open","claimed_at":"2026-09-24T20:00:00Z","source":"explicit",
                         "history_path":"/t/session-manager/1452"}]}]}
        """.trimIndent()
        val session = json.decodeFromString(SessionObligationsResponse.serializer(), body).sessions.single()
        assertEquals("abc12345", session.sessionId)
        assertTrue(session.docs.isEmpty())
        val claim = session.claims.single()
        assertEquals("ticket", claim.kind)
        assertEquals(1452L, claim.number)
        assertEquals("/t/session-manager/1452", claimHistoryPath(claim))
    }

    @Test fun obligationsWithoutClaimsStillParse() {
        val body = """{"schema_version":2,"sessions":[{"session_id":"abc12345","waiting_on":[],"review_history":[],"docs":[]}]}"""
        assertTrue(json.decodeFromString(SessionObligationsResponse.serializer(), body).sessions.single().claims.isEmpty())
    }

    @Test fun workLineListsTicketsBeforePrsAndOpensTicketPages() {
        val pr = SessionClaim(kind = "pr", repo = "rajeshgoli/session-manager", number = 1470, historyPath = "/t/session-manager/1470")
        val ticket = SessionClaim(kind = "ticket", repo = "rajeshgoli/session-manager", number = 1452)
        val claims = workClaims(listOf(pr, ticket))
        assertEquals(listOf("Ticket #1452", "PR #1470"), claims.map { workClaimLabel(it, withRepo = false) })
        assertEquals("PR session-manager #1470", workClaimLabel(pr, withRepo = true))
        // An older server sends no history_path; a non-ticket-page one is never used.
        assertEquals("/t/session-manager/1452", claimHistoryPath(ticket))
        assertEquals("/t/session-manager/1452", claimHistoryPath(ticket.copy(historyPath = "https://elsewhere/t/x/1")))
        assertEquals("/t/session-manager/1470", claimHistoryPath(pr))
        assertEquals("$server/t/session-manager/1470", readerUrl("$server/", claimHistoryPath(pr)))
    }

    @Test fun ownerPagesStayInTheReader() {
        val current = "$server/history"
        listOf(
            "$server/history",
            "$server/history?agent=1490-engineer&open=1",
            "$server/t/session-manager/1452",
            "$server/watch",
            "$server/",
            server,
            "$server/docs/widgets/memo.md",
        ).forEach { assertEquals(it, DocNavigation.Reload, docNavigation(server, current, it)) }
        listOf(
            "$server/historyx",
            "$server/client/sessions",
            "$server/session-obligations",
            "https://sm.example.com/history",
        ).forEach { assertEquals(it, DocNavigation.External, docNavigation(server, current, it)) }
    }

    @Test fun obligationsWithDocsParse() {
        val body = """
            {"schema_version":2,"sessions":[{"session_id":"abc12345","waiting_on":[],"review_history":[],"docs":[
              {"id":"557051fe","title":"Decision memo","state":"review_requested","repo":"rajeshgoli/widgets",
               "path":"specs/memo.md","pr_number":12,"latest_commit_sha":"$sha","published_at":"2026-09-24T20:00:00Z",
               "name":"widgets/specs/memo.md","reader_path":"/docs/widgets/specs/memo.md?version=856f0d6e1a2b",
               "browser_url":"https://sm.example.com/docs/widgets/specs/memo.md?version=856f0d6e1a2b"},
              {"title":"Readout","state":"read","repo":"rajeshgoli/widgets","path":"notes.html","pr_number":null,
               "latest_commit_sha":"$sha","published_at":"2026-09-24T20:00:00Z"}
            ]}]}
        """.trimIndent()
        val docs = json.decodeFromString(SessionObligationsResponse.serializer(), body).sessions.single().docs
        assertEquals(2, docs.size)
        assertEquals("Decision memo", docs[0].title)
        assertEquals(12L, docs[0].prNumber)
        assertEquals("/docs/widgets/specs/memo.md?version=856f0d6e1a2b", docs[0].readerPath)
        assertNull(docs[1].prNumber)
        assertNull(docs[1].readerPath)
        assertEquals("widgets/notes.html", docDisplayName(docs[1]))
    }

    @Test fun readerUsesTheServersReadablePath() {
        val path = "/docs/fractal-algo-rust/docs/working/ticket.html?version=856f0d6e1a2b"
        assertEquals("$server$path", readerUrl("$server/", docReaderPath(doc(readerPath = path))))
    }

    @Test fun readerBuildsTheReadablePathWhenAnOlderServerSendsNone() {
        assertEquals(
            "/docs/fractal-algo-rust/docs/working/my%20memo%231.html?version=856f0d6e1a2b",
            docReaderPath(doc()),
        )
        // A non-readable reader_path (an id route) is never used.
        assertEquals(docReaderPath(doc()), docReaderPath(doc(readerPath = "https://elsewhere/x")))
    }

    @Test fun stateLabelsMatchTheSpec() {
        assertEquals(
            listOf("New", "Updated", "Review requested", "Reviewed", "Read"),
            listOf("new", "updated", "review_requested", "reviewed", "read").map(::docStateLabel),
        )
    }

    @Test fun sameOriginDocNavigationsReloadWithAuth() {
        val current = "$server/docs/widgets/memo.md?version=aaaaaaaaaaaa"
        assertEquals(DocNavigation.Reload, docNavigation(server, current, "$server/docs/widgets/memo.md?version=bbbbbbbbbbbb"))
        assertEquals(DocNavigation.Reload, docNavigation(server, current, "https://SM-APP.example.com:443/docs/widgets/memo.md"))
        assertEquals(DocNavigation.InPage, docNavigation(server, current, "$current#decision"))
    }

    @Test fun everythingElseOpensInTheSystemBrowser() {
        val current = "$server/docs/widgets/memo.md"
        assertEquals(DocNavigation.External, docNavigation(server, current, "https://github.com/rajeshgoli/widgets/pull/12"))
        assertEquals(DocNavigation.External, docNavigation(server, current, "http://sm-app.example.com/docs/widgets/memo.md"))
        assertEquals(DocNavigation.External, docNavigation(server, current, "https://sm-app.example.com:8443/docs/x"))
        assertEquals(DocNavigation.External, docNavigation(server, current, "$server/client/sessions"))
        assertEquals(DocNavigation.External, docNavigation(server, current, "mailto:owner@example.com"))
    }

    @Test fun sharedLinksUseTheBrowserHost() {
        val current = "$server/docs/widgets/memo.md?version=bbbbbbbbbbbb"
        assertEquals(
            "https://sm.example.com/docs/widgets/memo.md?version=bbbbbbbbbbbb",
            shareUrl(current, docReaderPage(doc(browserUrl = "https://sm.example.com/docs/widgets/memo.md?version=aaaaaaaaaaaa")).browserUrl),
        )
        assertEquals(current, shareUrl(current, docReaderPage(doc()).browserUrl))
    }

    @Test fun browserHostLinksOpenInTheReader() {
        val host = "sm.example.com"
        val docLink = "https://sm.example.com/docs/fractal-algo-rust/docs/working/my%20memo%231.html?version=856f0d6e1a2b"
        val doc = readerPageForLink(docLink, host)!!
        assertEquals("/docs/fractal-algo-rust/docs/working/my%20memo%231.html?version=856f0d6e1a2b", doc.path)
        assertEquals("fractal-algo-rust/docs/working/my memo#1.html", doc.subtitle)
        assertEquals(docLink, doc.browserUrl)
        assertEquals(false, doc.followsPage)

        val ticket = readerPageForLink("https://SM.example.com/t/session-manager/1575", host)!!
        assertEquals("/t/session-manager/1575", ticket.path)
        assertTrue(ticket.followsPage)
        assertEquals("/history", readerPageForLink("https://sm.example.com/history", host)!!.path)
    }

    @Test fun obligationsWithAndWithoutMessagesParse() {
        val without = """{"schema_version":3,"sessions":[{"session_id":"abc12345","waiting_on":[],"review_history":[],"docs":[],"claims":[]}]}"""
        assertTrue(json.decodeFromString(SessionObligationsResponse.serializer(), without).sessions.single().messages.isEmpty())
        val with = """
            {"schema_version":4,"sessions":[{"session_id":"abc12345","review_history":[],"docs":[],"claims":[],
              "waiting_on":[{"kind":"owner_message","id":"msg_3f9a2c1d","label":"Rajesh · Keep it?","since":"2026-09-26T10:00:00Z"}],
              "messages":[{"id":"msg_3f9a2c1d","title":"Keep it?","state":"needs_you","created_at":"2026-09-26T10:00:00Z",
                           "reader_path":"/messages/msg_3f9a2c1d"}]}]}
        """.trimIndent()
        val session = json.decodeFromString(SessionObligationsResponse.serializer(), with).sessions.single()
        val message = session.messages.single()
        assertEquals("needs_you", message.state)
        assertEquals("Needs you", messageStateLabel(message.state))
        assertEquals("/messages/msg_3f9a2c1d", messageReaderPage(message).path)
        assertEquals("Keep it?", messageReaderPage(message).title)
        assertEquals("owner_message", session.waitingOn.single().kind)
    }

    @Test fun messageLinksOpenAndStayInTheReader() {
        val link = "https://sm.example.com/messages/msg_3f9a2c1d"
        val page = readerPageForLink(link, "sm.example.com")!!
        assertEquals("/messages/msg_3f9a2c1d", page.path)
        assertEquals(link, page.browserUrl)
        assertNull(readerPageForLink("https://sm.example.com/messages/", "sm.example.com"))
        assertEquals(DocNavigation.Reload, docNavigation(server, "$server/messages/msg_00000001", "$server/messages/msg_3f9a2c1d"))
        assertTrue(isOwnerPagePath("/messages/msg_3f9a2c1d"))
    }

    @Test fun otherLinksAreNotReaderLinks() {
        val host = "sm.example.com"
        assertNull(readerPageForLink("https://sm.example.com/watch", host))
        assertNull(readerPageForLink("https://sm.example.com/docs/", host))
        assertNull(readerPageForLink("http://sm.example.com/docs/widgets/memo.md", host))
        assertNull(readerPageForLink("https://evil.example.com/docs/widgets/memo.md", host))
        assertNull(readerPageForLink("https://sm.example.com/docs/widgets/memo.md", ""))
        assertNull(readerPageForLink("sm-enroll://enroll?url=x", host))
    }

    @Test fun serverDownStatusesAreCloudflareOriginFailuresOnly() {
        listOf(502, 503, 504, 530).forEach { assertTrue("$it", isServerDownStatus(it)) }
        listOf(200, 401, 403, 404, 500, 501).forEach { assertFalse("$it", isServerDownStatus(it)) }
    }

    @Test fun serverDownErrorsAreConnectivityFailuresOnly() {
        listOf(WebViewClient.ERROR_CONNECT, WebViewClient.ERROR_HOST_LOOKUP, WebViewClient.ERROR_TIMEOUT, WebViewClient.ERROR_IO)
            .forEach { assertTrue("$it", isServerDownError(it)) }
        listOf(WebViewClient.ERROR_BAD_URL, WebViewClient.ERROR_UNSUPPORTED_SCHEME, WebViewClient.ERROR_FAILED_SSL_HANDSHAKE)
            .forEach { assertFalse("$it", isServerDownError(it)) }
    }

    @Test fun readerRetryBacksOffForAboutThirtySecondsThenStops() {
        assertEquals(0L, readerRetryDelayMs(0))
        val schedule = (1..6).map { readerRetryDelayMs(it) }
        assertEquals(listOf(1_000L, 2_000L, 4_000L, 8_000L, 8_000L, 8_000L), schedule)
        assertEquals(31_000L, schedule.sumOf { it!! })
        assertNull(readerRetryDelayMs(7))
    }
}
