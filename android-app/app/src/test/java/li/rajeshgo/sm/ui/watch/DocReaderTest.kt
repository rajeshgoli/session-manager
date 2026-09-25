package li.rajeshgo.sm.ui.watch

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.SessionDoc
import li.rajeshgo.sm.data.model.SessionObligationsResponse
import org.junit.Assert.assertEquals
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
        assertEquals("$server$path", docReaderUrl("$server/", doc(readerPath = path)))
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
            docShareUrl(current, doc(browserUrl = "https://sm.example.com/docs/widgets/memo.md?version=aaaaaaaaaaaa")),
        )
        assertEquals(current, docShareUrl(current, doc()))
    }
}
